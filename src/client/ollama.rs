//! Ollama's native `/api/chat`. Worth its own transport because it takes the
//! context window as a per-request option, and Ollama's own default of 4096
//! silently truncates an agentic prompt.

use super::stream::TextStream;
use super::{AssistantTurn, Client, FunctionCall, Message, StreamEvent, ToolCall, Usage};
use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

impl Client {
    pub(super) async fn ollama_stream(
        &self,
        messages: &[Message],
        tools: &Value,
        cancel: CancellationToken,
        mut on_event: impl FnMut(StreamEvent),
    ) -> Result<AssistantTurn> {
        #[derive(Deserialize)]
        struct NativeChunk {
            error: Option<String>,
            message: Option<NativeMsg>,
            #[serde(default)]
            done: bool,
            done_reason: Option<String>,
            prompt_eval_count: Option<u64>,
            eval_count: Option<u64>,
        }
        #[derive(Deserialize)]
        struct NativeMsg {
            content: Option<String>,
            thinking: Option<String>,
            tool_calls: Option<Vec<NativeToolCall>>,
        }
        #[derive(Deserialize)]
        struct NativeToolCall {
            function: NativeFn,
        }
        #[derive(Deserialize)]
        struct NativeFn {
            name: String,
            #[serde(default)]
            arguments: Value,
        }

        let origin = self.base_url.strip_suffix("/v1").unwrap_or(&self.base_url);
        let native_messages: Vec<Value> = messages.iter().map(to_ollama_message).collect();
        let mut options = json!({"num_ctx": self.num_ctx});
        if let Some(t) = self.temperature {
            options["temperature"] = json!(t);
        }
        let mut body = json!({
            "model": self.model,
            "messages": native_messages,
            "stream": true,
            "options": options,
        });
        if tools.as_array().map(|a| !a.is_empty()).unwrap_or(false) {
            body["tools"] = tools.clone();
        }
        if let Some(t) = self.think {
            body["think"] = json!(t);
        }
        let resp = self
            .auth(self.http.post(format!("{origin}/api/chat")).json(&body))
            .send()
            .await
            .with_context(|| format!("cannot reach {origin}. is Ollama running?"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            let body: String = body.chars().take(500).collect();
            bail!("ollama returned {status}: {body}");
        }

        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut turn = AssistantTurn::default();
        let mut text = TextStream::new(tools);

        'outer: loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    turn.interrupted = true;
                    break 'outer;
                }
                chunk = stream.next() => {
                    let Some(chunk) = chunk else { break 'outer };
                    let chunk = chunk.context("stream error")?;
                    buf.extend_from_slice(&chunk);
                    while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                        let line: Vec<u8> = buf.drain(..=pos).collect();
                        let line = String::from_utf8_lossy(&line);
                        let line = line.trim();
                        if line.is_empty() {
                            continue;
                        }
                        let Ok(parsed) = serde_json::from_str::<NativeChunk>(line) else { continue };
                        if let Some(e) = parsed.error {
                            bail!("ollama error: {e}");
                        }
                        if let Some(m) = parsed.message {
                            if let Some(t) = m.thinking
                                && !t.is_empty() {
                                    on_event(StreamEvent::Reasoning(t));
                                }
                            if let Some(t) = m.content
                                && !t.is_empty() {
                                    text.content(&t, &mut turn, &mut on_event);
                                }
                            for tc in m.tool_calls.unwrap_or_default() {
                                let i = turn.tool_calls.len();
                                turn.tool_calls.push(ToolCall {
                                    id: format!("call_{i}"),
                                    kind: "function".into(),
                                    function: FunctionCall {
                                        name: tc.function.name,
                                        arguments: tc.function.arguments.to_string(),
                                    },
                                });
                            }
                        }
                        if parsed.done {
                            if parsed.done_reason.as_deref() == Some("length") {
                                turn.truncated = true;
                            }
                            if parsed.prompt_eval_count.is_some() || parsed.eval_count.is_some() {
                                turn.usage = Some(Usage {
                                    prompt_tokens: parsed.prompt_eval_count.unwrap_or(0),
                                    completion_tokens: parsed.eval_count.unwrap_or(0),
                                    cached_tokens: 0,
                                });
                            }
                            break 'outer;
                        }
                    }
                }
            }
        }

        text.finish(&mut turn, &mut on_event);
        Ok(turn)
    }
}

/// Converts to the Ollama native message shape: tool results carry
/// `tool_name`, and tool-call arguments are JSON objects, not strings.
fn to_ollama_message(m: &Message) -> Value {
    match m.role {
        "tool" => json!({
            "role": "tool",
            "tool_name": m.name.clone().unwrap_or_default(),
            "content": m.content.clone().unwrap_or_default(),
        }),
        "assistant" => {
            let mut v = json!({
                "role": "assistant",
                "content": m.content.clone().unwrap_or_default(),
            });
            if let Some(tcs) = &m.tool_calls {
                let calls: Vec<Value> = tcs
                    .iter()
                    .map(|tc| {
                        let args: Value = serde_json::from_str(&tc.function.arguments)
                            .unwrap_or_else(|_| json!({}));
                        json!({"function": {"name": tc.function.name, "arguments": args}})
                    })
                    .collect();
                v["tool_calls"] = json!(calls);
            }
            v
        }
        _ => json!({"role": m.role, "content": m.content.clone().unwrap_or_default()}),
    }
}
