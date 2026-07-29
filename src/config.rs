use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    pub base_url: String,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub temperature: Option<f32>,
    pub max_turns: usize,
    /// Context window to request (Ollama native API only).
    pub num_ctx: Option<u32>,
    /// Force thinking mode on/off (Ollama native only). Turning it off helps
    /// small context windows a lot.
    pub think: Option<bool>,
    /// Google Programmable Search credentials; when set, web_search uses
    /// Google instead of DuckDuckGo.
    pub google_api_key: Option<String>,
    pub google_cx: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    base_url: Option<String>,
    model: Option<String>,
    api_key: Option<String>,
    temperature: Option<f32>,
    max_turns: Option<usize>,
    num_ctx: Option<u32>,
    think: Option<bool>,
    google_api_key: Option<String>,
    google_cx: Option<String>,
}

pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("thoth")
}

pub fn load(
    base_url: Option<String>,
    model: Option<String>,
    api_key: Option<String>,
    temperature: Option<f32>,
) -> Config {
    let file: FileConfig = std::fs::read_to_string(config_dir().join("config.toml"))
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default();
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    Config {
        base_url: base_url
            .or_else(|| env("THOTH_BASE_URL"))
            .or(file.base_url)
            .unwrap_or_else(|| "http://localhost:11434/v1".into())
            .trim_end_matches('/')
            .to_string(),
        model: model.or_else(|| env("THOTH_MODEL")).or(file.model),
        api_key: api_key.or_else(|| env("THOTH_API_KEY")).or(file.api_key),
        temperature: temperature.or(file.temperature),
        max_turns: file.max_turns.unwrap_or(40),
        num_ctx: env("THOTH_NUM_CTX")
            .and_then(|v| v.parse().ok())
            .or(file.num_ctx),
        think: env("THOTH_THINK")
            .and_then(|v| v.parse().ok())
            .or(file.think),
        google_api_key: env("THOTH_GOOGLE_API_KEY").or(file.google_api_key),
        google_cx: env("THOTH_GOOGLE_CX").or(file.google_cx),
    }
}
