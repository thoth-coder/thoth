use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize)]
pub struct Message {
    pub role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Tool name for tool-result messages (needed by the Ollama native API).
    #[serde(skip_serializing)]
    pub name: Option<String>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system",
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user",
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }
    pub fn assistant(content: Option<String>, tool_calls: Option<Vec<ToolCall>>) -> Self {
        Self {
            role: "assistant",
            content,
            tool_calls,
            tool_call_id: None,
            name: None,
        }
    }
    pub fn tool(tool_call_id: String, name: String, content: String) -> Self {
        Self {
            role: "tool",
            content: Some(content),
            tool_calls: None,
            tool_call_id: Some(tool_call_id),
            name: Some(name),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Transport {
    /// OpenAI-compatible /chat/completions (llama.cpp, vLLM, LM Studio, ...).
    OpenAI,
    /// Ollama native /api/chat — lets us control num_ctx per request.
    Ollama,
}

pub enum StreamEvent {
    Content(String),
    Reasoning(String),
}

#[derive(Default)]
pub struct AssistantTurn {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub interrupted: bool,
    pub usage: Option<Usage>,
    /// Generation stopped because the context window filled up.
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
}

#[derive(Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    usage: Option<Usage>,
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

/// Routes `<think>…</think>` spans that some models emit inside `content`
/// to reasoning instead. Tags can be split across stream chunks.
#[derive(Default)]
struct ThinkFilter {
    buf: String,
    in_think: bool,
}

impl ThinkFilter {
    /// Returns (is_reasoning, text) pieces ready to emit.
    fn push(&mut self, t: &str) -> Vec<(bool, String)> {
        self.buf.push_str(t);
        let mut out = Vec::new();
        loop {
            let tag = if self.in_think { "</think>" } else { "<think>" };
            if let Some(pos) = self.buf.find(tag) {
                if pos > 0 {
                    out.push((self.in_think, self.buf[..pos].to_string()));
                }
                self.buf.drain(..pos + tag.len());
                self.in_think = !self.in_think;
                continue;
            }
            // hold back a suffix that could be the start of a split tag
            let hold = longest_suffix_prefix(&self.buf, tag);
            let emit = self.buf.len() - hold;
            if emit > 0 {
                let text: String = self.buf.drain(..emit).collect();
                out.push((self.in_think, text));
            }
            break;
        }
        out
    }

    fn flush(&mut self) -> Vec<(bool, String)> {
        if self.buf.is_empty() {
            Vec::new()
        } else {
            vec![(self.in_think, std::mem::take(&mut self.buf))]
        }
    }
}

fn longest_suffix_prefix(s: &str, tag: &str) -> usize {
    let max = (tag.len() - 1).min(s.len());
    for l in (1..=max).rev() {
        let start = s.len() - l;
        if s.is_char_boundary(start) && tag.as_bytes().starts_with(&s.as_bytes()[start..]) {
            return l;
        }
    }
    0
}

pub struct Client {
    http: reqwest::Client,
    pub base_url: String,
    pub model: String,
    api_key: Option<String>,
    temperature: Option<f32>,
    pub transport: Transport,
    pub num_ctx: u32,
    /// Ollama native only: force thinking on/off (None = model default).
    pub think: Option<bool>,
}

pub const DEFAULT_NUM_CTX: u32 = 32768;

impl Client {
    pub fn new(
        base_url: String,
        api_key: Option<String>,
        temperature: Option<f32>,
        num_ctx: Option<u32>,
    ) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("failed to build http client");
        Self {
            http,
            base_url,
            model: String::new(),
            api_key,
            temperature,
            transport: Transport::OpenAI,
            num_ctx: num_ctx.unwrap_or(DEFAULT_NUM_CTX),
            think: None,
        }
    }

    /// Switches to the Ollama native API when the server is Ollama, so we can
    /// request a proper context window (Ollama's default 4096 silently
    /// truncates agentic prompts).
    pub async fn detect_ollama(&mut self) {
        let Some(origin) = self.base_url.strip_suffix("/v1") else {
            return;
        };
        let probe = self.http.get(format!("{origin}/api/version")).send();
        if let Ok(Ok(resp)) = tokio::time::timeout(std::time::Duration::from_secs(3), probe).await
            && resp.status().is_success()
        {
            self.transport = Transport::Ollama;
        }
    }

    pub async fn models(&self) -> Result<Vec<String>> {
        #[derive(Deserialize)]
        struct M {
            id: String,
        }
        #[derive(Deserialize)]
        struct R {
            data: Vec<M>,
        }
        let mut req = self.http.get(format!("{}/models", self.base_url));
        if let Some(k) = &self.api_key {
            req = req.bearer_auth(k);
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("cannot reach {}. is the server running?", self.base_url))?;
        let r: R = resp
            .error_for_status()
            .context("listing models failed")?
            .json()
            .await
            .context("unexpected response from /models")?;
        Ok(r.data.into_iter().map(|m| m.id).collect())
    }

    pub async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &Value,
        cancel: CancellationToken,
        on_event: impl FnMut(StreamEvent),
    ) -> Result<AssistantTurn> {
        match self.transport {
            Transport::OpenAI => self.openai_stream(messages, tools, cancel, on_event).await,
            Transport::Ollama => self.ollama_stream(messages, tools, cancel, on_event).await,
        }
    }

    async fn openai_stream(
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
        let mut req = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .json(&body);
        if let Some(k) = &self.api_key {
            req = req.bearer_auth(k);
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("cannot reach {}. is the server running?", self.base_url))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            let body: String = body.chars().take(500).collect();
            bail!("server returned {status}: {body}");
        }

        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut turn = AssistantTurn::default();
        let mut think = ThinkFilter::default();
        // (id, name, arguments) accumulated per tool-call index
        let mut partials: Vec<(String, String, String)> = Vec::new();

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
                        let Some(data) = line.strip_prefix("data:") else { continue };
                        let data = data.trim();
                        if data == "[DONE]" {
                            break 'outer;
                        }
                        let Ok(parsed) = serde_json::from_str::<StreamChunk>(data) else { continue };
                        if let Some(u) = parsed.usage {
                            turn.usage = Some(u);
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
                                    for (is_reasoning, text) in think.push(&t) {
                                        if is_reasoning {
                                            on_event(StreamEvent::Reasoning(text));
                                        } else {
                                            turn.content.push_str(&text);
                                            on_event(StreamEvent::Content(text));
                                        }
                                    }
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

        for (is_reasoning, text) in think.flush() {
            if is_reasoning {
                on_event(StreamEvent::Reasoning(text));
            } else {
                turn.content.push_str(&text);
                on_event(StreamEvent::Content(text));
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
        Ok(turn)
    }

    async fn ollama_stream(
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
        let mut req = self.http.post(format!("{origin}/api/chat")).json(&body);
        if let Some(k) = &self.api_key {
            req = req.bearer_auth(k);
        }
        let resp = req
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
        let mut think = ThinkFilter::default();

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
                                    for (is_reasoning, text) in think.push(&t) {
                                        if is_reasoning {
                                            on_event(StreamEvent::Reasoning(text));
                                        } else {
                                            turn.content.push_str(&text);
                                            on_event(StreamEvent::Content(text));
                                        }
                                    }
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
                                });
                            }
                            break 'outer;
                        }
                    }
                }
            }
        }

        for (is_reasoning, text) in think.flush() {
            if is_reasoning {
                on_event(StreamEvent::Reasoning(text));
            } else {
                turn.content.push_str(&text);
                on_event(StreamEvent::Content(text));
            }
        }
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
