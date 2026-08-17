mod agent;
mod cli;
mod client;
mod config;
mod editor;
mod print;
mod tools;
mod ui;
mod upgrade;

use agent::{Agent, AgentEvent};
use anyhow::{Result, anyhow, bail};
use clap::Parser;
use client::Client;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> Result<()> {
    let args = cli::Args::parse();
    if let Some(done) = cli::run_subcommand(args.command).await {
        return done;
    }
    // before any of the config and network work below: a view needs nothing
    // but the drawing code, and asking for one on a machine with no model
    // running has to still show the screen
    if let Some(name) = &args.view {
        return cli::run_view(name, &args.view_size);
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

    // from here on every file change is snapshotted, so /undo has something
    // to put back. Arming it here rather than in Agent keeps the file tools
    // inert for anything that is not a real session
    agent::undo::arm();
    let mut agent = Agent::new(client, cfg, ev_tx, cancel_slot.clone());
    if let Some(m) = &args.mode {
        match agent::PermMode::from_name(m) {
            Some(m) => agent.set_mode(m),
            None => {
                eprintln!("thoth: no mode called {m}. manual, accept-edits, auto or plan");
                std::process::exit(2);
            }
        }
    }
    if args.resume {
        agent.resume_session();
    }
    tokio::spawn(agent.run(cmd_rx));

    match args.prompt {
        Some(p) => print::run(p, cmd_tx, ev_rx).await,
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
