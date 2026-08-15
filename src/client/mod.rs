//! Talking to the model server. Three wire protocols live behind one
//! `Client`: OpenAI-compatible SSE, Ollama native and Anthropic native, one
//! module each. What they share is here: the message shape, the transport
//! choice, and the request plumbing.

mod anthropic;
mod ollama;
mod openai;
mod stream;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
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
pub(crate) const DEFAULT_MAX_TOKENS: u32 = 8192;

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

    /// Builds a client and settles its transport. Startup and a mid-session
    /// profile switch both go through here, so the two cannot drift.
    pub async fn connect(cfg: &crate::config::Config) -> Self {
        let mut client = Self::new(cfg);
        client
            .detect_transport(cfg.api != crate::config::Api::Auto)
            .await;
        client
    }

    /// Finishes what `guess_transport` could not decide from the url alone:
    /// a local OpenAI-compatible endpoint might be Ollama, which is worth
    /// knowing because its native API takes a context window per request
    /// (the default 4096 silently truncates agentic prompts).
    ///
    /// Only ever probes a local address. A hosted endpoint must never see a
    /// stray request to a path that is not part of its api.
    async fn detect_transport(&mut self, explicit: bool) {
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

/// Localhost or a private network address. Only these get probed for Ollama,
/// and only for these will thoth pick a model on its own: a paid endpoint
/// must never see a request to a path we guessed, nor a model we guessed.
pub fn is_local(url: &str) -> bool {
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
}
