//! `thoth -p "..."`: the same agent with stdout and stderr instead of a
//! terminal interface.
//!
//! The split down the middle is the point. Anything a caller might pipe into
//! another program goes to stdout: the model's answer, and nothing else.
//! Everything that is thoth talking about itself, and every line a tool
//! printed, goes to stderr, so `thoth -p "..." > answer.txt` leaves a file
//! with the answer in it and a terminal with the working shown.

use crate::agent::{AgentCmd, AgentEvent, PermReply};
use crate::ui;
use anyhow::{Result, anyhow};
use std::io::Write as _;
use tokio::sync::mpsc;

/// How many lines of a tool result are worth printing before it becomes the
/// output rather than a sign of progress.
const RESULT_LINES: usize = 3;

pub async fn run(
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

    let mut spend = Spend::default();
    while let Some(ev) = ev_rx.recv().await {
        match ev {
            AgentEvent::Content(t) => {
                print!("{t}");
                std::io::stdout().flush().ok();
            }
            AgentEvent::ToolStart { name, summary } => eprintln!("\n[{name}] {summary}"),
            AgentEvent::ToolResult { content, is_error } => {
                let lines: Vec<&str> = content.lines().collect();
                for l in lines.iter().take(RESULT_LINES) {
                    eprintln!("  | {l}");
                }
                if lines.len() > RESULT_LINES {
                    eprintln!("  | … +{} lines", lines.len() - RESULT_LINES);
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
                let _ = reply.send(ask(&tool).await);
            }
            AgentEvent::Diff(t) => {
                for l in t.lines() {
                    eprintln!("  {l}");
                }
            }
            AgentEvent::Info(t) => eprintln!("* {t}"),
            AgentEvent::Error(t) => eprintln!("error: {t}"),
            AgentEvent::Models(models) => {
                for m in models {
                    eprintln!("  {m}");
                }
            }
            AgentEvent::Usage { usage, cost } => spend.add(&usage, cost),
            // nobody is at the keyboard: a plan is simply the answer, and a
            // question is one the agent has to settle for itself
            AgentEvent::PlanReady => {}
            AgentEvent::Choice {
                question, reply, ..
            } => {
                println!("\n[question, unanswered: nobody is here] {question}");
                let _ = reply.send(None);
            }
            AgentEvent::TurnEnd => break,
            AgentEvent::Reasoning(_)
            | AgentEvent::ModelChanged(_)
            | AgentEvent::Connected { .. }
            | AgentEvent::TurnStart => {}
        }
    }
    println!();
    spend.report();
    Ok(())
}

/// Three answers, not four: `skip` needs a running task to carry on without
/// the step, and anything unrecognised is a no, because a stdin that is not
/// a terminal reads as empty and must not be taken for consent.
async fn ask(tool: &str) -> PermReply {
    let q = format!("allow {tool}? [y=yes / a=always / n=no] ");
    let answer = tokio::task::spawn_blocking(move || {
        eprint!("{q}");
        let mut s = String::new();
        std::io::stdin().read_line(&mut s).ok();
        s
    })
    .await
    .unwrap_or_default();
    match answer.trim().to_lowercase().as_str() {
        "y" | "yes" => PermReply::Yes,
        "a" | "always" => PermReply::Always,
        _ => PermReply::No,
    }
}

/// What the run cost, printed once at the end.
#[derive(Default)]
struct Spend {
    ctx_tokens: u64,
    out_tokens: u64,
    usd: f64,
    priced: bool,
}

impl Spend {
    fn add(&mut self, usage: &crate::client::Usage, cost: Option<f64>) {
        // context is where it got to, not a running total; output is
        self.ctx_tokens = usage.prompt_tokens;
        self.out_tokens += usage.completion_tokens;
        self.usd += cost.unwrap_or(0.0);
        self.priced |= cost.is_some();
    }

    fn report(&self) {
        if self.ctx_tokens == 0 {
            return;
        }
        eprintln!(
            "* context {} tokens, output {} tokens{}",
            self.ctx_tokens,
            self.out_tokens,
            if self.priced {
                format!(", cost ${:.4}", self.usd)
            } else {
                String::new()
            }
        );
    }
}
