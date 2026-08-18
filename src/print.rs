//! `thoth -p "..."`: the same agent with stdout and stderr instead of a
//! terminal interface.
//!
//! The split down the middle is the point. Anything a caller might pipe into
//! another program goes to stdout: the model's answer, and nothing else.
//! Everything that is thoth talking about itself, and every line a tool
//! printed, goes to stderr, so `thoth -p "..." > answer.txt` leaves a file
//! with the answer in it and a terminal with the working shown.
//!
//! Both halves go through `printable` on the way out. There is a terminal on
//! the other end of this too, and almost none of what is written to it was
//! written by thoth: a `\x1b[2J` in a file thoth was asked to read wipes the
//! screen here exactly as it would in the interface.

use crate::agent::{AgentCmd, AgentEvent, PermReply};
use crate::ui;
use crate::ui::render::printable;
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
        eprintln!("* {}", printable(&l));
    }
    cmd_tx
        .send(AgentCmd::UserInput(format!("{prompt}{attachments}")))
        .map_err(|_| anyhow!("agent task died"))?;

    let mut spend = Spend::default();
    while let Some(ev) = ev_rx.recv().await {
        match ev {
            AgentEvent::Content(t) => {
                print!("{}", printable(&t));
                std::io::stdout().flush().ok();
            }
            AgentEvent::ToolStart { name, summary } => {
                eprintln!("\n[{}] {}", printable(&name), printable(&summary))
            }
            AgentEvent::ToolResult { content, is_error } => {
                let lines: Vec<&str> = content.lines().collect();
                for l in lines.iter().take(RESULT_LINES) {
                    eprintln!("  | {}", printable(l));
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
                eprintln!("\n{}", printable(&preview));
                let _ = reply.send(ask(&tool).await);
            }
            AgentEvent::Diff(t) => {
                for l in t.lines() {
                    eprintln!("  {}", printable(l));
                }
            }
            AgentEvent::Info(t) => eprintln!("* {}", printable(&t)),
            AgentEvent::Error(t) => eprintln!("error: {}", printable(&t)),
            AgentEvent::Models(models) => {
                for m in models {
                    eprintln!("  {}", printable(&m));
                }
            }
            AgentEvent::Usage { usage, cost } => spend.add(&usage, cost),
            // nobody is at the keyboard: a plan is simply the answer, and a
            // question is one the agent has to settle for itself
            AgentEvent::PlanReady => {}
            AgentEvent::Choice {
                question, reply, ..
            } => {
                println!(
                    "\n[question, unanswered: nobody is here] {}",
                    printable(&question)
                );
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `-p` has a terminal on the other end of it as much as the interface
    /// does, and almost nothing it writes there was written by thoth. This
    /// went unnoticed once already: the escapes were taken out of every line
    /// the interface draws and out of none of the lines this prints.
    #[test]
    fn nothing_reaches_the_terminal_with_its_escapes_still_in_it() {
        let hostile = "\u{1b}[31mred\u{1b}[0m and \u{1b}[2Jwiped";
        let out = printable(hostile);
        assert_eq!(out, "red and wiped");
        assert!(!out.contains('\u{1b}'));

        // an OSC title change and a bidi override are the other two ways of
        // making a terminal show one thing and mean another
        assert_eq!(printable("a\u{1b}]0;title\u{7}b"), "ab");
        assert_eq!(printable("safe\u{202e}gnp.exe"), "safegnp.exe");

        // and ordinary text comes through as it was, tabs aside
        assert_eq!(printable("lines=2 words=3"), "lines=2 words=3");
        assert_eq!(printable("a\tb"), "a    b");
    }
}
