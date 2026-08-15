mod agent;
mod client;
mod config;
mod editor;
mod tools;
mod ui;
mod upgrade;

use agent::{Agent, AgentCmd, AgentEvent, PermReply};
use anyhow::{Result, anyhow, bail};
use clap::Parser;
use client::Client;
use std::io::Write as _;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Agentic coding assistant. Runs on your own models (Ollama, llama.cpp) or
/// on an api you pay for (Anthropic, OpenAI, Google, OpenRouter).
#[derive(Parser)]
#[command(name = "thoth", version)]
struct Args {
    #[command(subcommand)]
    command: Option<Cmd>,
    /// Run a single prompt non-interactively (plain output) and exit
    #[arg(short, long)]
    prompt: Option<String>,
    /// Resume this project's previous conversation
    #[arg(short = 'c', long = "continue")]
    resume: bool,
    /// Config profile to run with (see `thoth config`)
    #[arg(short = 'P', long)]
    profile: Option<String>,
    /// OpenAI-compatible endpoint, e.g. http://localhost:11434/v1 (Ollama)
    /// or http://localhost:8080/v1 (llama.cpp)
    #[arg(long)]
    base_url: Option<String>,
    /// Model name, e.g. qwen3:8b
    #[arg(short, long)]
    model: Option<String>,
    /// API key, if the server requires one
    #[arg(long)]
    api_key: Option<String>,
    /// Sampling temperature
    #[arg(long)]
    temperature: Option<f32>,
}

#[derive(clap::Subcommand)]
enum Cmd {
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
enum ConfigCmd {
    /// List the profiles and show which one is active
    List,
    /// Start with this profile from now on
    Use { name: String },
    /// Print the path of the config file
    Path,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    match args.command {
        Some(Cmd::Upgrade) => return upgrade::run().await,
        Some(Cmd::Config { action }) => return run_config(action),
        None => {}
    }
    let (cfg, profile) = config::load(config::Overrides {
        profile: args.profile,
        base_url: args.base_url,
        model: args.model,
        api_key: args.api_key,
        temperature: args.temperature,
    })?;

    let mut client = Client::connect(&cfg).await;

    let mut startup_note = None;
    if client.model.is_empty() {
        let (model, note) = choose_model(&client).await?;
        client.model = model;
        startup_note = note;
    }

    let model = client.model.clone();
    let base_url = client.base_url.clone();
    let transport = client.transport;
    let window = agent::window_of(&client, &cfg);
    let cancel_slot = Arc::new(Mutex::new(CancellationToken::new()));
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (ev_tx, ev_rx) = mpsc::unbounded_channel();

    if let Some(note) = startup_note {
        let _ = ev_tx.send(AgentEvent::Info(note));
    }
    // the TUI says this on its startup screen; -p has no screen to say it on
    if args.prompt.is_some() && transport != client::Transport::OpenAI {
        let _ = ev_tx.send(AgentEvent::Info(format!(
            "{} api{}",
            transport.name(),
            match window {
                Some(n) => format!(", context window {n} tokens"),
                None => String::new(),
            }
        )));
    }

    let mut agent = Agent::new(client, cfg, ev_tx, cancel_slot.clone());
    if args.resume {
        agent.resume_session();
    }
    tokio::spawn(agent.run(cmd_rx));

    match args.prompt {
        Some(p) => run_print_mode(p, cmd_tx, ev_rx).await,
        None => {
            ui::run(
                ui::Session {
                    model,
                    base_url,
                    profile,
                    api: transport.name(),
                    window,
                },
                cmd_tx,
                ev_rx,
                cancel_slot,
            )
            .await
        }
    }
}

/// Which model to use when the profile does not name one. A local server
/// usually holds one or two and picking for the user is a kindness; a hosted
/// api holds hundreds and picking would spend their money on a guess, so it
/// asks instead.
async fn choose_model(client: &Client) -> Result<(String, Option<String>)> {
    let models = client.models().await.map_err(|e| {
        anyhow!(
            "{e:#}\n\nhint: set a model with `thoth config` or -m NAME, start Ollama \
             (`ollama serve`) or llama.cpp (`llama-server -m model.gguf --jinja`), \
             or point thoth at a server with --base-url"
        )
    })?;
    let chat: Vec<&String> = models.iter().filter(|m| looks_like_chat_model(m)).collect();
    let pick = |list: &[&String]| list.first().map(|m| (*m).clone());
    match (chat.len(), client::is_local(&client.base_url)) {
        (0, _) => bail!(
            "no usable models on {}. pull one first, e.g. `ollama pull qwen3:8b`",
            client.base_url
        ),
        (1, _) => Ok((pick(&chat).expect("one match"), None)),
        (_, true) => Ok((
            pick(&chat).expect("at least one"),
            Some(format!(
                "several models on the server, using '{}'. /models to list, /model NAME to switch",
                chat[0]
            )),
        )),
        // hosted: never guess
        (_, false) => bail!(
            "this profile has no model set, and {} offers {}. pick one with \
             `thoth config`, or -m NAME. some of what it has:\n  {}",
            client.base_url,
            chat.len(),
            chat.iter()
                .take(15)
                .map(|m| m.as_str())
                .collect::<Vec<_>>()
                .join("\n  ")
        ),
    }
}

/// Drops the obvious non-chat entries a hosted /models list is full of.
/// Conservative on purpose: anything unrecognised stays in.
fn looks_like_chat_model(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    ![
        "embed",
        "whisper",
        "tts",
        "dall-e",
        "moderation",
        "rerank",
        "audio",
        "image",
    ]
    .iter()
    .any(|bad| id.contains(bad))
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

/// Plain stdout/stderr mode for `thoth -p "..."` — no TUI.
async fn run_print_mode(
    prompt: String,
    cmd_tx: mpsc::UnboundedSender<AgentCmd>,
    mut ev_rx: mpsc::UnboundedReceiver<AgentEvent>,
) -> Result<()> {
    let (attachments, labels) = ui::input::expand_mentions(&prompt, &ui::input::cwd());
    for l in labels {
        eprintln!("* {l}");
    }
    cmd_tx
        .send(AgentCmd::UserInput(format!("{prompt}{attachments}")))
        .map_err(|_| anyhow!("agent task died"))?;
    let mut ctx_tokens = 0u64;
    let mut out_tokens = 0u64;
    let mut spent = 0.0f64;
    let mut priced = false;
    while let Some(ev) = ev_rx.recv().await {
        match ev {
            AgentEvent::Content(t) => {
                print!("{t}");
                std::io::stdout().flush().ok();
            }
            AgentEvent::Reasoning(_) => {}
            AgentEvent::ToolStart { name, summary } => {
                eprintln!("\n[{name}] {summary}");
            }
            AgentEvent::ToolResult { content, is_error } => {
                let lines: Vec<&str> = content.lines().collect();
                for l in lines.iter().take(3) {
                    eprintln!("  | {l}");
                }
                if lines.len() > 3 {
                    eprintln!("  | … +{} lines", lines.len() - 3);
                }
                if is_error {
                    eprintln!("  | (error)");
                }
            }
            AgentEvent::Permission {
                tool,
                preview,
                reply,
            } => {
                eprintln!("\n{preview}");
                let q = format!("allow {tool}? [y=yes / a=always / n=no] ");
                let ans = tokio::task::spawn_blocking(move || {
                    eprint!("{q}");
                    let mut s = String::new();
                    std::io::stdin().read_line(&mut s).ok();
                    s
                })
                .await
                .unwrap_or_default();
                let r = match ans.trim().to_lowercase().as_str() {
                    "y" | "yes" => PermReply::Yes,
                    "a" | "always" => PermReply::Always,
                    _ => PermReply::No,
                };
                let _ = reply.send(r);
            }
            AgentEvent::Diff(t) => {
                for l in t.lines() {
                    eprintln!("  {l}");
                }
            }
            AgentEvent::Info(t) => eprintln!("* {t}"),
            AgentEvent::Error(t) => eprintln!("error: {t}"),
            AgentEvent::ModelChanged(_) | AgentEvent::Connected { .. } => {}
            AgentEvent::Models(models) => {
                for m in models {
                    eprintln!("  {m}");
                }
            }
            AgentEvent::TurnStart => {}
            AgentEvent::Usage { usage, cost } => {
                ctx_tokens = usage.prompt_tokens;
                out_tokens += usage.completion_tokens;
                spent += cost.unwrap_or(0.0);
                priced |= cost.is_some();
            }
            AgentEvent::TurnEnd => break,
        }
    }
    println!();
    if ctx_tokens > 0 {
        eprintln!(
            "* context {ctx_tokens} tokens, output {out_tokens} tokens{}",
            if priced {
                format!(", cost ${spent:.4}")
            } else {
                String::new()
            }
        );
    }
    Ok(())
}
