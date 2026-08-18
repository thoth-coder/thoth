//! OpenAI-compatible `/chat/completions` over SSE: OpenAI itself, Gemini's
//! compatibility endpoint, OpenRouter, llama.cpp, vLLM, LM Studio and the
//! rest.

use super::stream::TextStream;
use super::{AssistantTurn, Client, FunctionCall, Message, StreamEvent, ToolCall, Usage};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

#[derive(Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    usage: Option<UsageChunk>,
}

/// Usage as the OpenAI stream reports it, with the cache detail nested.
#[derive(Deserialize)]
struct UsageChunk {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<PromptDetails>,
}

#[derive(Deserialize)]
struct PromptDetails {
    #[serde(default)]
    cached_tokens: u64,
}

impl From<UsageChunk> for Usage {
    fn from(u: UsageChunk) -> Self {
        Usage {
            prompt_tokens: u.prompt_tokens,
            completion_tokens: u.completion_tokens,
            cached_tokens: u
                .prompt_tokens_details
                .map(|d| d.cached_tokens)
                .unwrap_or(0),
        }
    }
}

#[derive(Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: Delta,
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct Delta {
    content: Option<String>,
    reasoning_content: Option<String>,
    reasoning: Option<String>,
    tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Deserialize)]
struct ToolCallDelta {
    index: Option<usize>,
    id: Option<String>,
    function: Option<FunctionDelta>,
}

#[derive(Deserialize)]
struct FunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

impl Client {
    pub(super) async fn openai_stream(
        &self,
        messages: &[Message],
        tools: &Value,
        cancel: CancellationToken,
        mut on_event: impl FnMut(StreamEvent),
    ) -> Result<AssistantTurn> {
        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "stream": true,
            "stream_options": {"include_usage": true},
        });
        if tools.as_array().map(|a| !a.is_empty()).unwrap_or(false) {
            body["tools"] = tools.clone();
        }
        if let Some(t) = self.temperature {
            body["temperature"] = json!(t);
        }
        if let Some(m) = self.max_tokens {
            body["max_tokens"] = json!(m);
        }
        let url = format!("{}/chat/completions", self.base_url);
        let send = |body: &Value| self.auth(self.http.post(&url).json(body)).send();
        let mut resp = send(&body)
            .await
            .with_context(|| format!("cannot reach {}. is the server running?", self.base_url))?;
        // stream_options is an OpenAI extension and not every compatible
        // server takes it. Losing the token counts beats losing the answer,
        // so drop it and go again rather than failing the turn.
        if resp.status() == reqwest::StatusCode::BAD_REQUEST {
            let complaint = resp.text().await.unwrap_or_default();
            if !complaint.contains("stream_options") {
                let short: String = complaint.chars().take(500).collect();
                bail!("server returned 400 Bad Request: {short}");
            }
            body.as_object_mut()
                .expect("json object")
                .remove("stream_options");
            resp = send(&body).await.context("retry without stream_options")?;
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            let body: String = body.chars().take(500).collect();
            bail!("server returned {status}: {body}");
        }

        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        // a stream that has started talking and stopped is a different
        // failure from one that never started, and waits a different length
        let mut started = false;
        let mut turn = AssistantTurn::default();
        let mut text = TextStream::new(tools);
        // (id, name, arguments) accumulated per tool-call index
        let mut partials: Vec<(String, String, String)> = Vec::new();

        'outer: loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    turn.interrupted = true;
                    break 'outer;
                }
                chunk = super::stream::next_chunk(&mut stream, started) => {
                    let Some(chunk) = chunk? else { break 'outer };
                    started = true;
                    buf.extend_from_slice(&chunk);
                    while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                        let line: Vec<u8> = buf.drain(..=pos).collect();
                        let line = String::from_utf8_lossy(&line);
                        let line = line.trim();
                        let Some(data) = line.strip_prefix("data:") else { continue };
                        let data = data.trim();
                        if data == "[DONE]" {
                            break 'outer;
                        }
                        let Ok(parsed) = serde_json::from_str::<StreamChunk>(data) else { continue };
                        if let Some(u) = parsed.usage {
                            turn.usage = Some(u.into());
                        }
                        for choice in parsed.choices {
                            if choice.finish_reason.as_deref() == Some("length") {
                                turn.truncated = true;
                            }
                            let d = choice.delta;
                            if let Some(t) = d.reasoning_content.or(d.reasoning)
                                && !t.is_empty() {
                                    on_event(StreamEvent::Reasoning(t));
                                }
                            if let Some(t) = d.content
                                && !t.is_empty() {
                                    text.content(&t, &mut turn, &mut on_event);
                                }
                            for tcd in d.tool_calls.unwrap_or_default() {
                                let idx = tcd.index.unwrap_or_else(|| {
                                    if tcd.id.is_some() || partials.is_empty() {
                                        partials.len()
                                    } else {
                                        partials.len() - 1
                                    }
                                });
                                while partials.len() <= idx {
                                    partials.push(Default::default());
                                }
                                let p = &mut partials[idx];
                                if let Some(id) = tcd.id
                                    && !id.is_empty() {
                                        p.0 = id;
                                    }
                                if let Some(f) = tcd.function {
                                    if let Some(n) = f.name {
                                        p.1.push_str(&n);
                                    }
                                    if let Some(a) = f.arguments {
                                        p.2.push_str(&a);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        for (i, (id, name, args)) in partials.into_iter().enumerate() {
            if name.is_empty() {
                continue;
            }
            turn.tool_calls.push(ToolCall {
                id: if id.is_empty() {
                    format!("call_{i}")
                } else {
                    id
                },
                kind: "function".into(),
                function: FunctionCall {
                    name,
                    arguments: if args.is_empty() { "{}".into() } else { args },
                },
            });
        }
        text.finish(&mut turn, &mut on_event);
        Ok(turn)
    }
}
