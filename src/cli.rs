//! What the command line offers, and the subcommands that answer without
//! ever starting a session.
//!
//! Kept away from `main.rs` so that file is only the order things happen in
//! at startup. Everything here is reachable before a config is loaded or a
//! server is contacted, which is the property `thoth config` and `--view`
//! both depend on: they have to work on a machine with no model running.

use crate::{config, ui, upgrade};
use anyhow::{Result, bail};
use clap::Parser;

/// Agentic coding assistant. Runs on your own models (Ollama, llama.cpp) or
/// on an api you pay for (Anthropic, OpenAI, Google, OpenRouter).
#[derive(Parser)]
#[command(name = "thoth", version)]
pub struct Args {
    #[command(subcommand)]
    pub command: Option<Cmd>,
    /// Run a single prompt non-interactively (plain output) and exit
    #[arg(short, long)]
    pub prompt: Option<String>,
    /// Resume this project's previous conversation
    #[arg(short = 'c', long = "continue")]
    pub resume: bool,
    /// Config profile to run with (see `thoth config`)
    #[arg(short = 'P', long)]
    pub profile: Option<String>,
    /// OpenAI-compatible endpoint, e.g. http://localhost:11434/v1 (Ollama)
    /// or http://localhost:8080/v1 (llama.cpp)
    #[arg(long)]
    pub base_url: Option<String>,
    /// Model name, e.g. qwen3:8b
    #[arg(short, long)]
    pub model: Option<String>,
    /// API key, if the server requires one
    #[arg(long)]
    pub api_key: Option<String>,
    /// Sampling temperature
    #[arg(long)]
    pub temperature: Option<f32>,
    /// How much to ask before acting: manual, accept-edits, auto, plan.
    /// Lasts for this run only; shift+tab changes it in the TUI
    #[arg(long, value_name = "MODE")]
    pub mode: Option<String>,
    /// Print a screen of the interface with made-up contents and exit, for
    /// working on the UI without waiting for a model to reach that state.
    /// Bare `--view` lists what there is. Debug builds only
    #[arg(long, value_name = "NAME", num_args = 0..=1, default_missing_value = "")]
    pub view: Option<String>,
    /// Size for --view, e.g. 100x30
    #[arg(long, value_name = "WxH", default_value = "100x30")]
    pub view_size: String,
}

#[derive(clap::Subcommand)]
pub enum Cmd {
    /// Download the latest release and replace this binary
    Upgrade,
    /// Edit the config profiles on a screen, or switch between them
    #[command(alias = "cfg")]
    Config {
        #[command(subcommand)]
        action: Option<ConfigCmd>,
    },
}

#[derive(clap::Subcommand)]
pub enum ConfigCmd {
    /// List the profiles and show which one is active
    List,
    /// Start with this profile from now on
    Use { name: String },
    /// Print the path of the config file
    Path,
}

/// The subcommands that never reach a model. `None` means the caller should
/// carry on and start a session.
pub async fn run_subcommand(cmd: Option<Cmd>) -> Option<Result<()>> {
    match cmd {
        Some(Cmd::Upgrade) => Some(upgrade::run().await),
        Some(Cmd::Config { action }) => Some(run_config(action)),
        None => None,
    }
}

/// The demo screens are not in a release binary, so neither is this.
#[cfg(not(debug_assertions))]
pub fn run_view(name: &str, _size: &str) -> Result<()> {
    bail!("--view is for debug builds. cargo run -- --view {name}")
}

/// `--view` exists so a UI state that normally needs a model behind it can be
/// looked at in one command.
#[cfg(debug_assertions)]
pub fn run_view(name: &str, size: &str) -> Result<()> {
    if name.is_empty() {
        println!("thoth --view <name> [--view-size WxH]\n");
        for v in ui::demo::VIEWS {
            println!("  {:<12} {}", v.name, v.about);
        }
        return Ok(());
    }
    let (w, h) = size
        .split_once(['x', 'X'])
        .and_then(|(a, b)| Some((a.trim().parse().ok()?, b.trim().parse().ok()?)))
        .ok_or_else(|| anyhow::anyhow!("--view-size wants something like 100x30, not {size}"))?;
    if w < 20 || h < 6 {
        bail!("--view-size {size} is too small to draw anything into");
    }
    print!("{}", ui::demo::render(name, w, h)?);
    Ok(())
}

/// `thoth config`: the screen with no arguments, and the three things that
/// are quicker to say than to click.
fn run_config(action: Option<ConfigCmd>) -> Result<()> {
    match action {
        None => ui::config::run_standalone(),
        Some(ConfigCmd::Path) => {
            println!("{}", config::config_path().display());
            Ok(())
        }
        Some(ConfigCmd::List) => {
            let store = config::load_store();
            if store.profiles.is_empty() {
                println!("no profiles yet. `thoth config` makes one");
                return Ok(());
            }
            for (name, p) in &store.profiles {
                println!(
                    "{} {name:<16} {:<24} {}",
                    if store.active.as_ref() == Some(name) {
                        "*"
                    } else {
                        " "
                    },
                    p.model.as_deref().unwrap_or("(server default)"),
                    p.base_url.as_deref().unwrap_or(config::DEFAULT_BASE_URL),
                );
            }
            Ok(())
        }
        Some(ConfigCmd::Use { name }) => {
            let mut store = config::load_store();
            if !store.profiles.contains_key(&name) {
                bail!("no profile named '{name}'. `thoth config list` shows them");
            }
            store.active = Some(name.clone());
            config::save_store(&store)?;
            println!("thoth now starts with profile '{name}'");
            Ok(())
        }
    }
}
