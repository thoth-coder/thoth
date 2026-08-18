//! Anthropic native `/v1/messages`. Worth its own transport rather than the
//! OpenAI-compatible shim because of prompt caching: an agent loop resends
//! the system prompt, the tool schemas and the whole history on every step,
//! and cached input is a tenth of the price.

use super::{
    AssistantTurn, Client, DEFAULT_MAX_TOKENS, FunctionCall, Message, StreamEvent, ToolCall, Usage,
};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

impl Client {
    /// Anthropic native `/v1/messages`. Worth its own transport rather than
    /// the OpenAI-compatible shim because of prompt caching: an agent loop
    /// resends the system prompt, the tool schemas and the whole history on
    /// every step, and cached input is a tenth of the price.
    pub(super) async fn anthropic_stream(
        &self,
        messages: &[Message],
        tools: &Value,
        cancel: CancellationToken,
        mut on_event: impl FnMut(StreamEvent),
    ) -> Result<AssistantTurn> {
        let (system, mut msgs) = anthropic_messages(messages);
        // two cache breakpoints: the static prefix (tools + system), and the
        // end of the history, so the next step reads all of it from cache
        mark_cacheable(msgs.last_mut());
        let mut body = json!({
            "model": self.model,
            "max_tokens": self.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
            "messages": msgs,
            "stream": true,
        });
        if !system.is_empty() {
            body["system"] = json!([{
                "type": "text",
                "text": system,
                "cache_control": {"type": "ephemeral"},
            }]);
        }
        if let Some(t) = anthropic_tools(tools, system.is_empty()) {
            body["tools"] = t;
        }
        if let Some(t) = self.temperature {
            body["temperature"] = json!(t);
        }

        let resp = self
            .auth(
                self.http
                    .post(format!("{}/messages", self.base_url))
                    .json(&body),
            )
            .send()
            .await
            .with_context(|| format!("cannot reach {}", self.base_url))?;
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
        let mut usage = Usage::default();
        // tool_use block being streamed: (id, name, partial json)
        let mut open_tool: Option<(String, String, String)> = None;

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
                        // "event:" lines repeat the type that is in the data
                        let Some(data) = line.trim().strip_prefix("data:") else { continue };
                        let Ok(ev) = serde_json::from_str::<AnthChunk>(data.trim()) else { continue };
                        if let Some(e) = ev.error {
                            bail!("{}", e.message.unwrap_or_else(|| "api error".into()));
                        }
                        match ev.kind.as_str() {
                            "message_start" => {
                                if let Some(u) = ev.message.and_then(|m| m.usage) {
                                    usage.prompt_tokens =
                                        u.input_tokens + u.cache_read_input_tokens
                                            + u.cache_creation_input_tokens;
                                    usage.cached_tokens = u.cache_read_input_tokens;
                                }
                            }
                            "content_block_start" => {
                                if let Some(b) = ev.content_block
                                    && b.kind == "tool_use"
                                {
                                    open_tool = Some((
                                        b.id.unwrap_or_default(),
                                        b.name.unwrap_or_default(),
                                        String::new(),
                                    ));
                                }
                            }
                            "content_block_delta" => {
                                let Some(d) = ev.delta else { continue };
                                if let Some(t) = d.text
                                    && !t.is_empty()
                                {
                                    turn.content.push_str(&t);
                                    on_event(StreamEvent::Content(t));
                                }
                                if let Some(t) = d.thinking
                                    && !t.is_empty()
                                {
                                    on_event(StreamEvent::Reasoning(t));
                                }
                                if let Some(j) = d.partial_json
                                    && let Some(open) = &mut open_tool
                                {
                                    open.2.push_str(&j);
                                }
                            }
                            "content_block_stop" => {
                                if let Some((id, name, args)) = open_tool.take()
                                    && !name.is_empty()
                                {
                                    turn.tool_calls.push(ToolCall {
                                        id: if id.is_empty() {
                                            format!("call_{}", turn.tool_calls.len())
                                        } else {
                                            id
                                        },
                                        kind: "function".into(),
                                        function: FunctionCall {
                                            name,
                                            arguments: if args.is_empty() {
                                                "{}".into()
                                            } else {
                                                args
                                            },
                                        },
                                    });
                                }
                            }
                            "message_delta" => {
                                if let Some(d) = &ev.delta
                                    && d.stop_reason.as_deref() == Some("max_tokens")
                                {
                                    turn.truncated = true;
                                }
                                if let Some(u) = ev.usage {
                                    usage.completion_tokens = u.output_tokens;
                                }
                            }
                            "message_stop" => break 'outer,
                            _ => {}
                        }
                    }
                }
            }
        }
        if usage.prompt_tokens > 0 || usage.completion_tokens > 0 {
            turn.usage = Some(usage);
        }
        Ok(turn)
    }
}

/// Marks the last content block of a message as a cache breakpoint.
fn mark_cacheable(msg: Option<&mut Value>) {
    if let Some(block) = msg
        .and_then(|m| m.get_mut("content"))
        .and_then(|c| c.as_array_mut())
        .and_then(|blocks| blocks.last_mut())
    {
        block["cache_control"] = json!({"type": "ephemeral"});
    }
}

/// OpenAI tool schemas to Anthropic's. `cache_last` marks the tool list as a
/// breakpoint when there is no system prompt to carry one.
fn anthropic_tools(tools: &Value, cache_last: bool) -> Option<Value> {
    let mut out: Vec<Value> = tools
        .as_array()?
        .iter()
        .filter_map(|t| {
            let f = t.get("function")?;
            Some(json!({
                "name": f.get("name")?,
                "description": f.get("description").cloned().unwrap_or_else(|| json!("")),
                "input_schema": f
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
            }))
        })
        .collect();
    if out.is_empty() {
        return None;
    }
    if cache_last && let Some(last) = out.last_mut() {
        last["cache_control"] = json!({"type": "ephemeral"});
    }
    Some(Value::Array(out))
}

/// Our OpenAI-shaped history to Anthropic's (system, messages). System text
/// moves out of the list; tool results become user messages carrying
/// tool_result blocks, and neighbours of the same role are merged, which the
/// api requires.
fn anthropic_messages(messages: &[Message]) -> (String, Vec<Value>) {
    let mut system = String::new();
    let mut out: Vec<Value> = Vec::new();
    let mut push = |role: &str, blocks: Vec<Value>| {
        if blocks.is_empty() {
            return;
        }
        if let Some(last) = out.last_mut()
            && last["role"] == role
            && let Some(existing) = last["content"].as_array_mut()
        {
            existing.extend(blocks);
            return;
        }
        out.push(json!({"role": role, "content": blocks}));
    };
    for m in messages {
        match m.role {
            "system" => {
                if let Some(c) = &m.content {
                    if !system.is_empty() {
                        system.push_str("\n\n");
                    }
                    system.push_str(c);
                }
            }
            "user" => push("user", text_block(m.content.as_deref().unwrap_or_default())),
            "assistant" => {
                let mut blocks = text_block(m.content.as_deref().unwrap_or_default());
                for tc in m.tool_calls.iter().flatten() {
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": tc.id,
                        "name": tc.function.name,
                        // arguments arrive as a json string; a model that
                        // sent something unparseable gets an empty object
                        // rather than a request the api will reject
                        "input": serde_json::from_str::<Value>(&tc.function.arguments)
                            .ok()
                            .filter(|v| v.is_object())
                            .unwrap_or_else(|| json!({})),
                    }));
                }
                push("assistant", blocks);
            }
            "tool" => push(
                "user",
                vec![json!({
                    "type": "tool_result",
                    "tool_use_id": m.tool_call_id.clone().unwrap_or_default(),
                    "content": m.content.clone().unwrap_or_default(),
                })],
            ),
            _ => {}
        }
    }
    (system, out)
}

fn text_block(text: &str) -> Vec<Value> {
    if text.is_empty() {
        Vec::new()
    } else {
        vec![json!({"type": "text", "text": text})]
    }
}

#[derive(Deserialize)]
struct AnthChunk {
    #[serde(rename = "type")]
    kind: String,
    message: Option<AnthStart>,
    content_block: Option<AnthBlock>,
    delta: Option<AnthDelta>,
    usage: Option<AnthUsage>,
    error: Option<AnthError>,
}

#[derive(Deserialize)]
struct AnthStart {
    usage: Option<AnthUsage>,
}

#[derive(Deserialize)]
struct AnthBlock {
    #[serde(rename = "type")]
    kind: String,
    id: Option<String>,
    name: Option<String>,
}

#[derive(Deserialize)]
struct AnthDelta {
    text: Option<String>,
    thinking: Option<String>,
    partial_json: Option<String>,
    stop_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct AnthUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
}

#[derive(Deserialize)]
struct AnthError {
    message: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anth(messages: &[Message]) -> (String, Vec<Value>) {
        anthropic_messages(messages)
    }

    #[test]
    fn anthropic_conversion_moves_system_out_and_merges_tool_results() {
        let call = ToolCall {
            id: "call_1".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "grep".into(),
                arguments: "{\"pattern\":\"fn main\"}".into(),
            },
        };
        let (system, msgs) = anth(&[
            Message::system("be brief"),
            Message::user("find main"),
            Message::assistant(Some("looking".into()), Some(vec![call])),
            Message::tool("call_1".into(), "grep".into(), "src/main.rs:1".into()),
            Message::tool("call_2".into(), "grep".into(), "src/lib.rs:2".into()),
        ]);
        assert_eq!(system, "be brief");
        // user, assistant, then ONE user message holding both tool results
        assert_eq!(msgs.len(), 3, "{msgs:#?}");
        assert_eq!(msgs[2]["role"], "user");
        assert_eq!(msgs[2]["content"].as_array().unwrap().len(), 2);
        assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
        assert_eq!(msgs[2]["content"][0]["tool_use_id"], "call_1");
        // the assistant turn keeps its text and its tool call
        let blocks = msgs[1]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[1]["type"], "tool_use");
        assert_eq!(blocks[1]["name"], "grep");
        assert_eq!(blocks[1]["input"]["pattern"], "fn main");
    }

    #[test]
    fn anthropic_conversion_survives_a_broken_tool_call() {
        let call = ToolCall {
            id: String::new(),
            kind: "function".into(),
            function: FunctionCall {
                name: "grep".into(),
                arguments: "not json".into(),
            },
        };
        let (_, msgs) = anth(&[Message::assistant(None, Some(vec![call]))]);
        // an empty object instead of a request the api would reject
        assert_eq!(msgs[0]["content"][0]["input"], json!({}));
        assert_eq!(msgs[0]["content"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn cache_breakpoint_lands_on_the_last_block() {
        let (_, mut msgs) = anth(&[Message::user("hello")]);
        mark_cacheable(msgs.last_mut());
        assert_eq!(msgs[0]["content"][0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn tools_convert_to_the_anthropic_shape() {
        let tools = json!([{
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "read it",
                "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}
            }
        }]);
        let out = anthropic_tools(&tools, false).unwrap();
        assert_eq!(out[0]["name"], "read_file");
        assert_eq!(
            out[0]["input_schema"]["properties"]["path"]["type"],
            "string"
        );
        assert!(out[0].get("cache_control").is_none());
        // with no system prompt the tool list carries the breakpoint instead
        let out = anthropic_tools(&tools, true).unwrap();
        assert_eq!(out[0]["cache_control"]["type"], "ephemeral");
        assert!(anthropic_tools(&json!([]), false).is_none());
    }
}
