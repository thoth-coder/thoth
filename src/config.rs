//! Settings. The file holds named profiles and which one is active; a run
//! resolves one profile, then lets `THOTH_*` env vars and CLI flags override
//! it. Nothing here talks to the network: `Config` is just the answer to
//! "what should this run connect to".

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub const DEFAULT_BASE_URL: &str = "http://localhost:11434/v1";
pub const DEFAULT_MAX_TURNS: usize = 40;

/// Which wire protocol to speak. `Auto` looks at the endpoint and decides.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Api {
    #[default]
    Auto,
    /// OpenAI-compatible /chat/completions. Also OpenAI itself, Gemini,
    /// OpenRouter, Groq, vLLM, llama.cpp, LM Studio, ...
    Openai,
    /// Ollama native /api/chat: lets us set the context window per request.
    Ollama,
    /// Anthropic native /v1/messages, with prompt caching.
    Anthropic,
}

/// What a run actually uses, after profile, env and flags are layered.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub api: Api,
    pub base_url: String,
    pub model: Option<String>,
    pub api_key: Option<String>,
    /// Extra request headers, for endpoints that do not take a bearer token
    /// (Azure's `api-key`, gateways that want their own).
    pub headers: BTreeMap<String, String>,
    pub temperature: Option<f32>,
    pub max_turns: usize,
    /// Context window. Requested per call on Ollama; elsewhere it is what
    /// auto-compact measures against, since the server never tells us.
    pub context_window: Option<u32>,
    /// Cap on one reply. Required by the Anthropic API, optional elsewhere.
    pub max_tokens: Option<u32>,
    /// Force thinking mode on/off (Ollama native only). Turning it off helps
    /// small context windows a lot.
    pub think: Option<bool>,
    /// USD per million tokens, for the running cost in the status line.
    pub price_in: Option<f64>,
    pub price_out: Option<f64>,
    /// Price of a cached input token, when the provider discounts one.
    pub price_cached: Option<f64>,
}

impl Config {
    /// Cost of a call in USD, or None when the profile has no prices.
    pub fn cost(&self, u: &crate::client::Usage) -> Option<f64> {
        let (pin, pout) = (self.price_in?, self.price_out.unwrap_or(0.0));
        let cached = u.cached_tokens.min(u.prompt_tokens);
        let fresh = u.prompt_tokens - cached;
        let pcached = self.price_cached.unwrap_or(pin);
        Some(
            (fresh as f64 * pin + cached as f64 * pcached + u.completion_tokens as f64 * pout)
                / 1_000_000.0,
        )
    }
}

/// One saved set of settings. Every field is optional: a profile only records
/// what it changes, so the file stays readable by hand.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api: Option<Api>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Only editable in the file: the screen shows how many are set.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<usize>,
    /// `num_ctx` was this field's name while thoth only spoke to Ollama.
    #[serde(alias = "num_ctx", skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub think: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price_in: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price_out: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price_cached: Option<f64>,
}

/// The whole config file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Store {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

/// `~/.thoth` — everything thoth owns on this machine: this file, the
/// per-project state and the editor bridge. One directory on every OS
/// instead of the platform config dir, so it is the same place to find,
/// back up and delete no matter where you run it.
pub fn thoth_home() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".thoth")
}

pub fn config_path() -> PathBuf {
    thoth_home().join("config.toml")
}

/// Where thoth 0.2 and earlier kept it: `%APPDATA%\thoth` on Windows,
/// `~/.config/thoth` elsewhere. Still read, never written.
fn legacy_config_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("thoth").join("config.toml"))
}

impl Store {
    /// Name of the profile a run should use: the one asked for, else the
    /// active one, else the only one there is.
    pub fn pick(&self, requested: Option<&str>) -> Result<Option<String>> {
        if let Some(name) = requested {
            if !self.profiles.contains_key(name) {
                bail!(
                    "no profile named '{name}'. `thoth config` to create one, \
                     `thoth config list` to see what exists"
                );
            }
            return Ok(Some(name.to_string()));
        }
        if let Some(a) = &self.active
            && self.profiles.contains_key(a)
        {
            return Ok(Some(a.clone()));
        }
        if self.profiles.len() == 1 {
            return Ok(self.profiles.keys().next().cloned());
        }
        Ok(None)
    }

    pub fn get(&self, name: &str) -> Profile {
        self.profiles.get(name).cloned().unwrap_or_default()
    }

    /// A name that is not taken yet, for the "new profile" row.
    pub fn free_name(&self) -> String {
        if !self.profiles.contains_key("new") {
            return "new".into();
        }
        (2..)
            .map(|n| format!("new{n}"))
            .find(|n| !self.profiles.contains_key(n))
            .unwrap_or_else(|| "new".into())
    }
}

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

/// Profile values with `THOTH_*` env vars layered on top. Used both at
/// startup and when the config screen switches profile mid-session, so the
/// two paths cannot drift apart.
pub fn resolve(p: &Profile) -> Config {
    Config {
        api: p.api.unwrap_or_default(),
        base_url: env("THOTH_BASE_URL")
            .or_else(|| p.base_url.clone())
            .unwrap_or_else(|| DEFAULT_BASE_URL.into())
            .trim_end_matches('/')
            .to_string(),
        model: env("THOTH_MODEL").or_else(|| p.model.clone()),
        api_key: env("THOTH_API_KEY").or_else(|| p.api_key.clone()),
        headers: p.headers.clone(),
        temperature: env("THOTH_TEMPERATURE")
            .and_then(|v| v.parse().ok())
            .or(p.temperature),
        max_turns: p.max_turns.unwrap_or(DEFAULT_MAX_TURNS),
        context_window: env("THOTH_CONTEXT_WINDOW")
            .or_else(|| env("THOTH_NUM_CTX"))
            .and_then(|v| v.parse().ok())
            .or(p.context_window),
        max_tokens: p.max_tokens,
        think: env("THOTH_THINK").and_then(|v| v.parse().ok()).or(p.think),
        price_in: p.price_in,
        price_out: p.price_out,
        price_cached: p.price_cached,
    }
}

/// Command-line flags, which beat both the profile and the environment.
#[derive(Debug, Default)]
pub struct Overrides {
    pub profile: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub temperature: Option<f32>,
}

/// Resolves the settings for a run. Returns the profile name it used, if any,
/// so the interface can show which one is live.
pub fn load(o: Overrides) -> Result<(Config, Option<String>)> {
    let store = load_store();
    let name = store.pick(o.profile.or_else(|| env("THOTH_PROFILE")).as_deref())?;
    let profile = name.as_deref().map(|n| store.get(n)).unwrap_or_default();
    let mut cfg = resolve(&profile);
    if let Some(u) = o.base_url {
        cfg.base_url = u.trim_end_matches('/').to_string();
    }
    cfg.model = o.model.or(cfg.model);
    cfg.api_key = o.api_key.or(cfg.api_key);
    cfg.temperature = o.temperature.or(cfg.temperature);
    Ok((cfg, name))
}

pub fn load_store() -> Store {
    let new = std::fs::read_to_string(config_path()).ok();
    let legacy = legacy_config_path().and_then(|p| std::fs::read_to_string(p).ok());
    pick_store(new.as_deref(), legacy.as_deref())
}

/// The current path wins, unless what is there holds no profiles: saving an
/// empty screen once must not hide a working config at the old path.
fn pick_store(new: Option<&str>, legacy: Option<&str>) -> Store {
    let store = new.map(parse_store).unwrap_or_default();
    if !store.profiles.is_empty() {
        return store;
    }
    legacy
        .map(parse_store)
        .filter(|s| !s.profiles.is_empty())
        .unwrap_or(store)
}

/// Reads the current format, and the flat pre-profile file thoth 0.1/0.2
/// wrote, which becomes a profile named "default".
fn parse_store(text: &str) -> Store {
    let mut store: Store = toml::from_str(text).unwrap_or_default();
    if store.profiles.is_empty()
        && let Ok(flat) = toml::from_str::<Profile>(text)
        && flat != Profile::default()
    {
        store.active = Some("default".into());
        store.profiles.insert("default".into(), flat);
    }
    store
}

/// Writes through a temp file, owner-only: this holds api keys.
pub fn save_store(store: &Store) -> Result<()> {
    let text = toml::to_string_pretty(store)?;
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_profiles_and_the_active_one() {
        let store = parse_store(
            r#"
active = "big"

[profiles.small]
model = "qwen3:8b"

[profiles.big]
base_url = "http://box:11434/v1"
model = "qwen3.6:35b"
num_ctx = 65536
"#,
        );
        assert_eq!(store.profiles.len(), 2);
        assert_eq!(store.pick(None).unwrap().as_deref(), Some("big"));
        assert_eq!(store.pick(Some("small")).unwrap().as_deref(), Some("small"));
        assert!(store.pick(Some("nope")).is_err());
        assert_eq!(store.get("big").context_window, Some(65536));
    }

    /// The 0.1/0.2 file must keep working: it becomes one profile, and the
    /// field `num_ctx` becomes `context_window`.
    #[test]
    fn migrates_a_flat_config_file() {
        let store = parse_store("model = \"qwen3:8b\"\nnum_ctx = 16384\n");
        assert_eq!(store.pick(None).unwrap().as_deref(), Some("default"));
        let p = store.get("default");
        assert_eq!(p.model.as_deref(), Some("qwen3:8b"));
        assert_eq!(p.context_window, Some(16384));
        // and it round-trips into the new shape
        let text = toml::to_string_pretty(&store).unwrap();
        assert!(text.contains("[profiles.default]"), "{text}");
        assert!(text.contains("context_window"), "{text}");
        assert_eq!(parse_store(&text).get("default"), p);
    }

    #[test]
    fn cost_uses_the_cached_rate_for_cached_input() {
        let cfg = resolve(&Profile {
            price_in: Some(3.0),
            price_out: Some(15.0),
            price_cached: Some(0.3),
            ..Default::default()
        });
        let usage = crate::client::Usage {
            prompt_tokens: 1_000_000,
            completion_tokens: 1_000_000,
            cached_tokens: 900_000,
        };
        // 100k fresh at $3, 900k cached at $0.30, 1M out at $15
        let cost = cfg.cost(&usage).unwrap();
        assert!((cost - (0.3 + 0.27 + 15.0)).abs() < 1e-9, "{cost}");
        // no prices in the profile means no number to show
        assert!(resolve(&Profile::default()).cost(&usage).is_none());
    }

    #[test]
    fn falls_back_to_the_only_profile_then_to_defaults() {
        let store = parse_store("[profiles.solo]\nmodel = \"m\"\n");
        assert_eq!(store.pick(None).unwrap().as_deref(), Some("solo"));
        let empty = parse_store("");
        assert_eq!(empty.pick(None).unwrap(), None);
        assert_eq!(resolve(&Profile::default()).base_url, DEFAULT_BASE_URL);
        assert_eq!(resolve(&Profile::default()).max_turns, DEFAULT_MAX_TURNS);
    }

    #[test]
    fn trailing_slash_never_reaches_the_url() {
        let p = Profile {
            base_url: Some("http://localhost:8080/v1/".into()),
            ..Default::default()
        };
        assert_eq!(resolve(&p).base_url, "http://localhost:8080/v1");
    }

    /// The file moved to ~/.thoth in 0.3. Both paths are still read, and the
    /// new one wins when someone has both.
    #[test]
    fn config_lives_in_the_thoth_home() {
        let path = config_path();
        assert!(path.ends_with(".thoth/config.toml") || path.ends_with(".thoth\\config.toml"));
        assert_eq!(path.parent(), Some(thoth_home().as_path()));
        let legacy = legacy_config_path().expect("every platform has a config dir");
        assert_ne!(legacy, path);
    }

    #[test]
    fn an_empty_new_file_does_not_hide_the_old_one() {
        let legacy = "model = \"qwen3:8b\"\n";
        // what the screen writes when it is saved with nothing in it
        assert_eq!(
            pick_store(Some("[profiles]\n"), Some(legacy))
                .get("default")
                .model
                .as_deref(),
            Some("qwen3:8b")
        );
        // but a real one at the new path wins
        let new = "[profiles.mine]\nmodel = \"m\"\n";
        let store = pick_store(Some(new), Some(legacy));
        assert!(store.profiles.contains_key("mine"), "{store:?}");
        assert!(!store.profiles.contains_key("default"), "{store:?}");
        assert!(pick_store(None, None).profiles.is_empty());
    }

    #[test]
    fn suggests_a_free_name() {
        let mut store = parse_store("[profiles.new]\n");
        assert_eq!(store.free_name(), "new2");
        store.profiles.insert("new2".into(), Profile::default());
        assert_eq!(store.free_name(), "new3");
    }
}

/// toml puts tables after values: a map in the middle of the struct
/// could swallow every field written after it. Every field at once,
/// through save and load.
#[test]
fn a_full_profile_survives_a_round_trip() {
    let mut store = Store::default();
    let p = Profile {
        api: Some(Api::Anthropic),
        base_url: Some("https://api.anthropic.com/v1".into()),
        model: Some("claude-sonnet-4-5".into()),
        api_key: Some("sk-ant-x".into()),
        headers: [("api-key".to_string(), "abc".to_string())]
            .into_iter()
            .collect(),
        temperature: Some(0.7),
        max_turns: Some(30),
        context_window: Some(200_000),
        max_tokens: Some(8192),
        think: Some(false),
        price_in: Some(3.0),
        price_out: Some(15.0),
        price_cached: Some(0.3),
    };
    store.active = Some("claude".into());
    store.profiles.insert("claude".into(), p.clone());
    store.profiles.insert("second".into(), Profile::default());

    let text = toml::to_string_pretty(&store).expect("serializes");
    let back = parse_store(&text);
    assert_eq!(back.active.as_deref(), Some("claude"), "{text}");
    assert_eq!(back.get("claude"), p, "{text}");
    assert_eq!(back.profiles.len(), 2, "{text}");
}
