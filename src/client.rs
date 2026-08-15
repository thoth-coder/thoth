use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
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
    /// OpenAI-compatible /chat/completions (OpenAI, Gemini's compat
    /// endpoint, OpenRouter, llama.cpp, vLLM, LM Studio, ...).
    OpenAI,
    /// Ollama native /api/chat — lets us control num_ctx per request.
    Ollama,
    /// Anthropic native /v1/messages — tool use plus prompt caching, which
    /// the OpenAI-compatible shim cannot do.
    Anthropic,
}

impl Transport {
    pub fn name(self) -> &'static str {
        match self {
            Transport::OpenAI => "openai",
            Transport::Ollama => "ollama native",
            Transport::Anthropic => "anthropic native",
        }
    }
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

#[derive(Debug, Clone, Copy, Default)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Part of `prompt_tokens` the provider served from its cache, when it
    /// says so. Billed at a lower rate.
    pub cached_tokens: u64,
}

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
    headers: BTreeMap<String, String>,
    temperature: Option<f32>,
    max_tokens: Option<u32>,
    pub transport: Transport,
    pub num_ctx: u32,
    /// Ollama native only: force thinking on/off (None = model default).
    pub think: Option<bool>,
}

pub const DEFAULT_NUM_CTX: u32 = 32768;
/// Anthropic requires a reply cap, so there has to be a default for it.
const DEFAULT_MAX_TOKENS: u32 = 8192;

impl Client {
    pub fn new(cfg: &crate::config::Config) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("failed to build http client");
        Self {
            http,
            base_url: cfg.base_url.clone(),
            model: cfg.model.clone().unwrap_or_default(),
            api_key: cfg.api_key.clone(),
            headers: cfg.headers.clone(),
            temperature: cfg.temperature,
            max_tokens: cfg.max_tokens,
            transport: match cfg.api {
                crate::config::Api::Openai => Transport::OpenAI,
                crate::config::Api::Ollama => Transport::Ollama,
                crate::config::Api::Anthropic => Transport::Anthropic,
                // settled by detect_transport, which may have to ask the server
                crate::config::Api::Auto => guess_transport(&cfg.base_url),
            },
            num_ctx: cfg.context_window.unwrap_or(DEFAULT_NUM_CTX),
            think: cfg.think,
        }
    }

    /// Finishes what `guess_transport` could not decide from the url alone:
    /// a local OpenAI-compatible endpoint might be Ollama, which is worth
    /// knowing because its native API takes a context window per request
    /// (the default 4096 silently truncates agentic prompts).
    ///
    /// Only ever probes a local address. A hosted endpoint must never see a
    /// stray request to a path that is not part of its api.
    pub async fn detect_transport(&mut self, explicit: bool) {
        if explicit || self.transport != Transport::OpenAI || !is_local(&self.base_url) {
            return;
        }
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

    /// Adds the api key the way this transport expects it, plus whatever
    /// extra headers the profile carries.
    fn auth(&self, mut req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(k) = &self.api_key {
            req = match self.transport {
                Transport::Anthropic => req.header("x-api-key", k),
                _ => req.bearer_auth(k),
            };
        }
        if self.transport == Transport::Anthropic {
            req = req.header("anthropic-version", "2023-06-01");
        }
        for (k, v) in &self.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        req
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
        let resp = self
            .auth(self.http.get(format!("{}/models", self.base_url)))
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
            Transport::Anthropic => {
                self.anthropic_stream(messages, tools, cancel, on_event)
                    .await
            }
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
        if let Some(m) = self.max_tokens {
            body["max_tokens"] = json!(m);
        }
        let resp = self
            .auth(
                self.http
                    .post(format!("{}/chat/completions", self.base_url))
                    .json(&body),
            )
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
                                    cached_tokens: 0,
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

    /// Anthropic native `/v1/messages`. Worth its own transport rather than
    /// the OpenAI-compatible shim because of prompt caching: an agent loop
    /// resends the system prompt, the tool schemas and the whole history on
    /// every step, and cached input is a tenth of the price.
    async fn anthropic_stream(
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
                chunk = stream.next() => {
                    let Some(chunk) = chunk else { break 'outer };
                    let chunk = chunk.context("stream error")?;
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

/// Endpoints we can recognise without asking. Everything else is assumed to
/// be OpenAI-compatible, which almost everything is.
fn guess_transport(base_url: &str) -> Transport {
    if host_of(base_url).ends_with("api.anthropic.com") {
        Transport::Anthropic
    } else {
        Transport::OpenAI
    }
}

fn host_of(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host = rest.split(['/', '?']).next().unwrap_or(rest);
    // strip a port, but not the colons inside a bare ipv6 address
    let host = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(v6),
        None => match host.rsplit_once(':') {
            Some((h, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => h,
            _ => host,
        },
    };
    host.to_ascii_lowercase()
}

/// Localhost or a private network address. Only these get probed for Ollama:
/// a paid endpoint must never see a request to a path we merely guessed at.
fn is_local(url: &str) -> bool {
    let host = host_of(url);
    if matches!(host.as_str(), "localhost" | "127.0.0.1" | "0.0.0.0" | "::1")
        || host.ends_with(".local")
        || host.ends_with(".localhost")
        || host.starts_with("192.168.")
        || host.starts_with("10.")
    {
        return true;
    }
    // 172.16.0.0/12
    host.strip_prefix("172.")
        .and_then(|rest| rest.split('.').next())
        .and_then(|o| o.parse::<u8>().ok())
        .map(|o| (16..=31).contains(&o))
        .unwrap_or(false)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_local_and_hosted_endpoints() {
        for local in [
            "http://localhost:11434/v1",
            "http://127.0.0.1:8080/v1",
            "http://192.168.1.10:11434/v1",
            "http://10.0.0.5:11434/v1",
            "http://172.16.4.4:11434/v1",
            "http://box.local:11434/v1",
        ] {
            assert!(is_local(local), "{local} should count as local");
        }
        for hosted in [
            "https://api.openai.com/v1",
            "https://api.anthropic.com/v1",
            "https://generativelanguage.googleapis.com/v1beta/openai",
            "https://openrouter.ai/api/v1",
            // 172.32 is outside the private range
            "http://172.32.0.1/v1",
        ] {
            assert!(!is_local(hosted), "{hosted} should not count as local");
        }
    }

    #[test]
    fn picks_the_transport_from_the_url() {
        assert_eq!(
            guess_transport("https://api.anthropic.com/v1"),
            Transport::Anthropic
        );
        assert_eq!(
            guess_transport("https://api.openai.com/v1"),
            Transport::OpenAI
        );
        assert_eq!(
            guess_transport("http://localhost:11434/v1"),
            Transport::OpenAI,
            "ollama is only settled by probing the server"
        );
    }

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
