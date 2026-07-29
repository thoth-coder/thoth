mod agent;
mod client;
mod config;
mod editor;
mod prompt;
mod tools;
mod tui;

use agent::{Agent, AgentCmd, AgentEvent, PermReply};
use anyhow::{Result, anyhow, bail};
use clap::Parser;
use client::Client;
use std::io::Write as _;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Agentic coding assistant for local LLMs (Ollama / llama.cpp)
#[derive(Parser)]
#[command(name = "thoth", version)]
struct Args {
    /// Run a single prompt non-interactively (plain output) and exit
    #[arg(short, long)]
    prompt: Option<String>,
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

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let cfg = config::load(args.base_url, args.model, args.api_key, args.temperature);

    if let (Some(key), Some(cx)) = (cfg.google_api_key.clone(), cfg.google_cx.clone()) {
        tools::web::set_google(key, cx);
    }

    let mut client = Client::new(
        cfg.base_url.clone(),
        cfg.api_key.clone(),
        cfg.temperature,
        cfg.num_ctx,
    );
    client.detect_ollama().await;
    client.think = cfg.think;

    let mut startup_note = None;
    client.model = match cfg.model {
        Some(m) => m,
        None => {
            let models = client.models().await.map_err(|e| {
                anyhow!(
                    "{e:#}\n\nhint: start Ollama (`ollama serve`) or llama.cpp \
                     (`llama-server -m model.gguf --jinja`), or point thoth at it with --base-url"
                )
            })?;
            match models.len() {
                0 => bail!(
                    "no models available on {}. pull one first, e.g. `ollama pull qwen3:8b`",
                    cfg.base_url
                ),
                1 => models[0].clone(),
                _ => {
                    startup_note = Some(format!(
                        "multiple models on server, using '{}'. /models to list, /model NAME to switch",
                        models[0]
                    ));
                    models[0].clone()
                }
            }
        }
    };

    let model = client.model.clone();
    let base_url = client.base_url.clone();
    let is_ollama = client.transport == client::Transport::Ollama;
    // only Ollama tells us the real window size; compact at 2/3 of it so the
    // summary itself still has room to generate
    let auto_compact_at = is_ollama.then(|| client.num_ctx as u64 * 2 / 3);
    let num_ctx_ui = is_ollama.then_some(client.num_ctx);
    let cancel_slot = Arc::new(Mutex::new(CancellationToken::new()));
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (ev_tx, ev_rx) = mpsc::unbounded_channel();

    if let Some(note) = startup_note {
        let _ = ev_tx.send(AgentEvent::Info(note));
    }
    if is_ollama {
        let _ = ev_tx.send(AgentEvent::Info(format!(
            "ollama native api, context window {} tokens",
            client.num_ctx
        )));
    }

    let agent = Agent::new(
        client,
        cfg.max_turns,
        ev_tx,
        cancel_slot.clone(),
        auto_compact_at,
    );
    tokio::spawn(agent.run(cmd_rx));

    match args.prompt {
        Some(p) => run_print_mode(p, cmd_tx, ev_rx).await,
        None => tui::run(model, base_url, num_ctx_ui, cmd_tx, ev_rx, cancel_slot).await,
    }
}

/// Plain stdout/stderr mode for `thoth -p "..."` — no TUI.
async fn run_print_mode(
    prompt: String,
    cmd_tx: mpsc::UnboundedSender<AgentCmd>,
    mut ev_rx: mpsc::UnboundedReceiver<AgentEvent>,
) -> Result<()> {
    cmd_tx
        .send(AgentCmd::UserInput(prompt))
        .map_err(|_| anyhow!("agent task died"))?;
    let mut ctx_tokens = 0u64;
    let mut out_tokens = 0u64;
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
            AgentEvent::ModelChanged(_) => {}
            AgentEvent::TurnStart => {}
            AgentEvent::Usage(u) => {
                ctx_tokens = u.prompt_tokens;
                out_tokens += u.completion_tokens;
            }
            AgentEvent::TurnEnd => break,
        }
    }
    println!();
    if ctx_tokens > 0 {
        eprintln!("* context {ctx_tokens} tokens, output {out_tokens} tokens");
    }
    Ok(())
}
