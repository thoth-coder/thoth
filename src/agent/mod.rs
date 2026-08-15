pub mod prompt;
pub mod session;

use crate::client::{Client, Message, StreamEvent, ToolCall, Usage};
use crate::tools;
use anyhow::{Result, anyhow};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

pub enum AgentCmd {
    UserInput(String),
    SetModel(String),
    ListModels,
    Clear,
    /// Summarize the conversation with the model and replace the history
    /// with the summary, freeing context space.
    Compact,
    /// Load .thoth/last-session.md back into the conversation context.
    Recap,
    /// A command the user ran themselves with `!cmd`: executed without asking
    /// the model, its output goes into the conversation as context.
    Shell(String),
    /// Show the persistent allowlist, or clear it (`/allow reset`).
    Permissions {
        reset: bool,
    },
    /// Point the session at another config profile, keeping the conversation.
    UseProfile {
        name: String,
        cfg: Box<crate::config::Config>,
    },
}

#[derive(Debug, Clone, Copy)]
pub enum PermReply {
    Yes,
    Always,
    No,
}

pub enum AgentEvent {
    Reasoning(String),
    Content(String),
    ToolStart {
        name: String,
        summary: String,
    },
    Permission {
        tool: String,
        preview: String,
        reply: oneshot::Sender<PermReply>,
    },
    ToolResult {
        content: String,
        is_error: bool,
    },
    /// Diff of a file change that was auto-approved (no permission prompt shown).
    Diff(String),
    Info(String),
    Error(String),
    ModelChanged(String),
    /// Answer to /models: what the server offers, in its own order.
    Models(Vec<String>),
    /// The session moved to another profile, server or context window.
    Connected {
        profile: Option<String>,
        model: String,
        base_url: String,
        api: &'static str,
        num_ctx: Option<u32>,
    },
    /// Token usage of the latest model call, and what it cost when the
    /// profile carries prices.
    Usage {
        usage: Usage,
        cost: Option<f64>,
    },
    /// A user request (possibly queued) started processing.
    TurnStart,
    TurnEnd,
}

pub struct Agent {
    client: Client,
    /// Settings of the running profile: turn budget and token prices.
    cfg: crate::config::Config,
    messages: Vec<Message>,
    always_allow: HashSet<String>,
    tx: mpsc::UnboundedSender<AgentEvent>,
    /// UI cancels the current generation/tool by cancelling the token in this slot.
    cancel_slot: Arc<Mutex<CancellationToken>>,
    last_editor_note: Option<String>,
    /// Auto-compact when the prompt reaches this many tokens (None = never).
    auto_compact_at: Option<u64>,
}

/// The window the conversation is measured against. Ollama is told what to
/// use per request; every other api only knows what the profile claims, and
/// says nothing when the profile is silent.
pub fn window_of(client: &Client, cfg: &crate::config::Config) -> Option<u32> {
    match client.transport {
        crate::client::Transport::Ollama => Some(client.num_ctx),
        _ => cfg.context_window,
    }
}

/// Compact at 2/3 of the window, so the summary itself still has room to
/// generate.
pub fn compact_threshold(client: &Client, cfg: &crate::config::Config) -> Option<u64> {
    window_of(client, cfg).map(|n| n as u64 * 2 / 3)
}

impl Agent {
    pub fn new(
        client: Client,
        cfg: crate::config::Config,
        tx: mpsc::UnboundedSender<AgentEvent>,
        cancel_slot: Arc<Mutex<CancellationToken>>,
    ) -> Self {
        Self {
            auto_compact_at: compact_threshold(&client, &cfg),
            client,
            cfg,
            messages: vec![Message::system(prompt::system_prompt())],
            always_allow: crate::agent::session::load_allow(),
            tx,
            cancel_slot,
            last_editor_note: None,
        }
    }

    fn send(&self, ev: AgentEvent) {
        let _ = self.tx.send(ev);
    }

    /// Rebuilds the connection from a config profile without touching the
    /// conversation: the model, server, context window and turn budget can
    /// all change mid-session.
    async fn use_profile(&mut self, name: &str, cfg: &crate::config::Config) {
        let keep_model = self.client.model.clone();
        let mut client = Client::new(cfg);
        client
            .detect_transport(cfg.api != crate::config::Api::Auto)
            .await;
        if client.model.is_empty() {
            // the profile leaves the model to the server: keep the one that
            // is already running rather than blanking it
            client.model = keep_model;
        }
        self.auto_compact_at = compact_threshold(&client, cfg);
        self.cfg = cfg.clone();
        let (model, base_url) = (client.model.clone(), client.base_url.clone());
        let window = window_of(&client, cfg);
        self.client = client;
        self.send(AgentEvent::Connected {
            profile: Some(name.to_string()),
            model: model.clone(),
            base_url: base_url.clone(),
            api: self.client.transport.name(),
            num_ctx: window,
        });
        self.send(AgentEvent::Info(format!(
            "profile {name}: {model} at {base_url} over the {} api{}",
            self.client.transport.name(),
            match window {
                Some(n) => format!(", context window {n} tokens"),
                None => String::new(),
            }
        )));
    }

    /// Loads the saved transcript of this project's last run. The system
    /// prompt stays the fresh one built at startup.
    pub fn resume_session(&mut self) {
        match crate::agent::session::load() {
            Some(msgs) => {
                let n = msgs.len();
                self.messages.extend(msgs);
                self.send(AgentEvent::Info(format!(
                    "resumed the previous session ({n} messages). /clear starts fresh"
                )));
            }
            None => self.send(AgentEvent::Info(
                "no saved session for this project yet".into(),
            )),
        }
    }

    pub async fn run(mut self, mut rx: mpsc::UnboundedReceiver<AgentCmd>) {
        while let Some(cmd) = rx.recv().await {
            let persist = !matches!(cmd, AgentCmd::ListModels | AgentCmd::Permissions { .. });
            match cmd {
                AgentCmd::UserInput(input) => {
                    self.send(AgentEvent::TurnStart);
                    if let Err(e) = self.run_turn(&input).await {
                        self.send(AgentEvent::Error(format!("{e:#}")));
                    }
                }
                AgentCmd::SetModel(m) => {
                    self.client.model = m.clone();
                    self.send(AgentEvent::ModelChanged(m.clone()));
                    self.send(AgentEvent::Info(format!("model set to {m}")));
                }
                AgentCmd::ListModels => match self.client.models().await {
                    Ok(models) => self.send(AgentEvent::Models(models)),
                    Err(e) => self.send(AgentEvent::Error(format!("{e:#}"))),
                },
                AgentCmd::Clear => {
                    // rebuild the system prompt so memory saved this session
                    // (and any project changes) are picked up immediately
                    self.messages.clear();
                    self.messages.push(Message::system(prompt::system_prompt()));
                    crate::agent::session::clear();
                    self.send(AgentEvent::Info("conversation context cleared".into()));
                }
                AgentCmd::Shell(command) => self.run_user_command(&command).await,
                AgentCmd::Permissions { reset } => {
                    if reset {
                        self.always_allow.clear();
                        crate::agent::session::clear_allow();
                        self.send(AgentEvent::Info(
                            "permission allowlist cleared, every tool asks again".into(),
                        ));
                    } else if self.always_allow.is_empty() {
                        self.send(AgentEvent::Info(
                            "no tools are always-allowed in this project. answer (a) at a \
                             permission prompt to add one"
                                .into(),
                        ));
                    } else {
                        let mut names: Vec<&str> =
                            self.always_allow.iter().map(String::as_str).collect();
                        names.sort_unstable();
                        self.send(AgentEvent::Info(format!(
                            "always allowed in this project (saved): {}\n/allow reset to undo",
                            names.join(", ")
                        )));
                    }
                }
                AgentCmd::UseProfile { name, cfg } => self.use_profile(&name, &cfg).await,
                AgentCmd::Compact => self.compact().await,
                AgentCmd::Recap => match tools::memory::load_recap() {
                    Some(recap) => {
                        self.messages.push(Message::user(format!(
                            "(recap of the previous session, for context only, no reply needed)\n{recap}"
                        )));
                        self.send(AgentEvent::Info(
                            "recap from the previous session loaded into context".into(),
                        ));
                    }
                    None => self.send(AgentEvent::Info(
                        "no recap for this project yet. one is written when a session compacts"
                            .into(),
                    )),
                },
            }
            if persist {
                crate::agent::session::save(&self.messages);
            }
            self.send(AgentEvent::TurnEnd);
        }
    }

    /// `!cmd`: the user ran this themselves, so it needs no permission. The
    /// output is added to the conversation as context, without a model call.
    async fn run_user_command(&mut self, command: &str) {
        self.send(AgentEvent::ToolStart {
            name: "shell".into(),
            summary: command.to_string(),
        });
        let token = CancellationToken::new();
        *self.cancel_slot.lock().unwrap() = token.clone();
        let args = serde_json::json!({ "command": command });
        let (content, is_error) = match tools::execute("shell", args, token).await {
            Ok(s) => (s, false),
            Err(e) => (format!("Error: {e:#}"), true),
        };
        self.send(AgentEvent::ToolResult {
            content: content.clone(),
            is_error,
        });
        self.messages.push(Message::user(format!(
            "(the user ran this command in their terminal, for context, no reply needed)\n\
             $ {command}\n{content}"
        )));
    }

    async fn compact(&mut self) {
        if self.messages.len() <= 1 {
            self.send(AgentEvent::Info("nothing to compact yet".into()));
            return;
        }
        let token = CancellationToken::new();
        *self.cancel_slot.lock().unwrap() = token.clone();
        // trim bulky tool outputs in the copy we summarize, so the summary
        // request itself fits even when the window is nearly full
        let mut msgs = self.messages.clone();
        for m in msgs.iter_mut() {
            if m.role == "tool"
                && let Some(c) = &m.content
                && c.chars().count() > 400
            {
                let cut: String = c.chars().take(400).collect();
                m.content = Some(format!("{cut}\n...(trimmed)"));
            }
        }
        msgs.push(Message::user(
            "Summarize this entire conversation so it can be continued later: the user's \
             original request, key facts and decisions, files read or changed (with paths), \
             current state, and what remains to be done. Reply with only the summary.",
        ));
        let before = self.messages.len();
        match self
            .client
            .chat_stream(&msgs, &serde_json::json!([]), token, |_| {})
            .await
        {
            Ok(turn) if !turn.interrupted && !turn.content.trim().is_empty() => {
                let summary = turn.content.trim().to_string();
                tools::memory::save_recap(&summary);
                self.messages.truncate(1);
                self.messages.push(Message::user(format!(
                    "(conversation continued from a compacted summary)\n{summary}"
                )));
                self.send(AgentEvent::Info(format!(
                    "context compacted: {before} messages -> 2. recap saved, /recap restores it next session"
                )));
            }
            Ok(turn) if turn.interrupted => {
                self.send(AgentEvent::Info("compact cancelled".into()));
            }
            Ok(_) => {
                // summary came back empty/truncated — free space mechanically
                self.hard_trim();
            }
            Err(e) => {
                self.send(AgentEvent::Error(format!("compact failed: {e:#}")));
                self.hard_trim();
            }
        }
    }

    /// Model-free fallback: shorten old tool results in place, keeping the
    /// last few intact.
    fn hard_trim(&mut self) {
        let keep_from = self.messages.len().saturating_sub(4);
        let mut trimmed = 0usize;
        for m in self.messages.iter_mut().take(keep_from) {
            if m.role != "tool" {
                continue;
            }
            if let Some(c) = &m.content
                && c.chars().count() > 200
            {
                let cut: String = c.chars().take(200).collect();
                m.content = Some(format!("{cut}\n...(trimmed to save context)"));
                trimmed += 1;
            }
        }
        self.send(AgentEvent::Info(format!(
            "trimmed {trimmed} old tool results to free context"
        )));
    }

    async fn run_turn(&mut self, input: &str) -> Result<()> {
        let mut full_input = input.to_string();
        if let Some(ctx) = crate::editor::detect().await {
            if self.last_editor_note.as_deref() != Some(ctx.label.as_str()) {
                self.send(AgentEvent::Info(format!("editor: {}", ctx.label)));
                self.last_editor_note = Some(ctx.label.clone());
            }
            full_input.push_str(&format!(
                "\n\n<editor-context>\n{}\n</editor-context>",
                ctx.note
            ));
        }
        self.messages.push(Message::user(full_input));
        let tools_def = tools::definitions();
        // breaks infinite loops: counts identical tool calls within this task
        let mut call_counts: std::collections::HashMap<String, u32> =
            std::collections::HashMap::new();
        let mut compact_pending = false;
        let mut truncations = 0u32;

        for _ in 0..self.cfg.max_turns {
            let token = CancellationToken::new();
            *self.cancel_slot.lock().unwrap() = token.clone();

            let tx = self.tx.clone();
            let turn = self
                .client
                .chat_stream(&self.messages, &tools_def, token.clone(), |ev| {
                    let _ = match ev {
                        StreamEvent::Content(t) => tx.send(AgentEvent::Content(t)),
                        StreamEvent::Reasoning(t) => tx.send(AgentEvent::Reasoning(t)),
                    };
                })
                .await?;

            if let Some(u) = turn.usage {
                if let Some(limit) = self.auto_compact_at
                    && u.prompt_tokens >= limit
                {
                    compact_pending = true;
                }
                self.send(AgentEvent::Usage {
                    usage: u,
                    cost: self.cfg.cost(&u),
                });
            }
            let trimmed = turn.content.trim();
            let content = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            };

            if turn.truncated {
                self.messages.push(Message::assistant(
                    content.clone().or_else(|| Some("(truncated)".into())),
                    None,
                ));
                truncations += 1;
                if truncations <= 2 {
                    self.send(AgentEvent::Info(
                        "hit the context limit mid-generation, compacting and continuing".into(),
                    ));
                    self.compact().await;
                    continue;
                }
                self.send(AgentEvent::Error(
                    "generation keeps hitting the context limit. raise context_window in the \
                     profile, or set think = false to spend fewer tokens on reasoning"
                        .into(),
                ));
                return Ok(());
            }

            if turn.interrupted {
                self.messages.push(Message::assistant(
                    content.or_else(|| Some("(interrupted)".into())),
                    None,
                ));
                self.send(AgentEvent::Info("interrupted".into()));
                return Ok(());
            }

            let tool_calls = turn.tool_calls;
            let had_content = content.is_some();
            self.messages.push(Message::assistant(
                content,
                if tool_calls.is_empty() {
                    None
                } else {
                    Some(tool_calls.clone())
                },
            ));
            if tool_calls.is_empty() {
                if !had_content && !turn.truncated {
                    self.send(AgentEvent::Info(
                        "(empty response from the model. rephrase, or /compact to free context)"
                            .into(),
                    ));
                }
                return Ok(());
            }

            for tc in &tool_calls {
                if token.is_cancelled() {
                    self.messages.push(Message::tool(
                        tc.id.clone(),
                        tc.function.name.clone(),
                        "Cancelled by user.".into(),
                    ));
                    continue;
                }
                let key = format!("{}:{}", tc.function.name, tc.function.arguments);
                let n = {
                    let c = call_counts.entry(key).or_insert(0);
                    *c += 1;
                    *c
                };
                if n > 2 {
                    let content = format!(
                        "STOP: this exact {} call was already run {n} times with identical \
                         input. The result will not change. Take a different approach, or \
                         explain what is blocking you and stop.",
                        tc.function.name
                    );
                    self.send(AgentEvent::ToolStart {
                        name: tc.function.name.clone(),
                        summary: "(repeated call blocked)".into(),
                    });
                    self.send(AgentEvent::ToolResult {
                        content: content.clone(),
                        is_error: true,
                    });
                    self.messages.push(Message::tool(
                        tc.id.clone(),
                        tc.function.name.clone(),
                        content,
                    ));
                    continue;
                }
                let result = self.run_tool(tc, &token).await;
                let (content, is_error) = match result {
                    Ok(s) => (s, false),
                    Err(e) => (format!("Error: {e:#}"), true),
                };
                self.send(AgentEvent::ToolResult {
                    content: content.clone(),
                    is_error,
                });
                self.messages.push(Message::tool(
                    tc.id.clone(),
                    tc.function.name.clone(),
                    content,
                ));
            }
            if token.is_cancelled() {
                self.send(AgentEvent::Info("interrupted".into()));
                return Ok(());
            }
            if compact_pending {
                compact_pending = false;
                self.send(AgentEvent::Info("context almost full, compacting".into()));
                self.compact().await;
            }
        }
        self.send(AgentEvent::Info(format!(
            "paused after {} agent steps (safety limit). say \"continue\" to keep going, or \
             raise max_turns in the config",
            self.cfg.max_turns
        )));
        Ok(())
    }

    async fn run_tool(&mut self, tc: &ToolCall, token: &CancellationToken) -> Result<String> {
        let name = tc.function.name.as_str();
        let args: Value = serde_json::from_str(&tc.function.arguments)
            .map_err(|e| anyhow!("invalid JSON in tool arguments: {e}"))?;

        self.send(AgentEvent::ToolStart {
            name: name.to_string(),
            summary: tools::summarize(name, &args),
        });

        let key = tools::permission_key(name, &args);
        let must_ask = tools::needs_permission(name, &args) && !self.always_allow.contains(&key);
        // permission skipped (always-allowed): still show exactly what runs —
        // the full diff for file changes, the full command line for shell
        if !must_ask && matches!(name, "write_file" | "edit_file" | "shell") {
            self.send(AgentEvent::Diff(tools::preview(name, &args)));
        }
        if must_ask {
            let (ptx, prx) = oneshot::channel();
            self.send(AgentEvent::Permission {
                tool: name.to_string(),
                preview: tools::preview(name, &args),
                reply: ptx,
            });
            let reply = tokio::select! {
                _ = token.cancelled() => PermReply::No,
                r = prx => r.unwrap_or(PermReply::No),
            };
            match reply {
                PermReply::Always => {
                    self.always_allow.insert(key.clone());
                    crate::agent::session::save_allow(&self.always_allow);
                    self.send(AgentEvent::Info(format!(
                        "always allowing {} in this project. /allow to review",
                        tools::permission_scope(&key)
                    )));
                }
                PermReply::Yes => {}
                PermReply::No => {
                    return Ok(
                        "The user denied this action. Do not retry it; ask the user or try another approach."
                            .into(),
                    )
                }
            }
        }

        tools::execute(name, args, token.clone()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Client;

    /// `!cmd` runs without a model or a permission prompt, and its output
    /// lands in the conversation for the next turn to use.
    #[tokio::test]
    async fn user_command_runs_and_enters_the_conversation() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let cfg = crate::config::resolve(&crate::config::Profile {
            base_url: Some("http://127.0.0.1:1".into()),
            max_turns: Some(1),
            ..Default::default()
        });
        let mut agent = Agent::new(
            Client::new(&cfg),
            cfg,
            tx,
            Arc::new(Mutex::new(CancellationToken::new())),
        );
        agent.run_user_command("echo hello-from-thoth").await;

        match rx.try_recv() {
            Ok(AgentEvent::ToolStart { name, summary }) => {
                assert_eq!(name, "shell");
                assert_eq!(summary, "echo hello-from-thoth");
            }
            _ => panic!("expected the command to be shown before it runs"),
        }
        match rx.try_recv() {
            Ok(AgentEvent::ToolResult { content, is_error }) => {
                assert!(!is_error, "command failed: {content}");
                assert!(content.contains("hello-from-thoth"), "got: {content}");
            }
            _ => panic!("expected a result"),
        }
        let last = agent.messages.last().unwrap();
        assert_eq!(last.role, "user");
        let text = last.content.clone().unwrap();
        assert!(text.contains("$ echo hello-from-thoth"));
        assert!(text.contains("hello-from-thoth"));
    }
}
