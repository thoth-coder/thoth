pub mod prompt;
pub mod session;
pub mod undo;

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
    /// Put the files back the way they were before the last request that
    /// changed any, or list what could be undone.
    Undo {
        list: bool,
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
        /// Context window this conversation is measured against, when one
        /// is known: what Ollama is told per request, or what the profile
        /// declares for any other api.
        window: Option<u32>,
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
    /// Said once, the first time a tool call has to be read out of the text.
    warned_text_calls: bool,
    /// Said once, the first time a request leaves something to undo.
    said_undo: bool,
    /// Read-only call (name + arguments) to the id of the message holding
    /// its latest result, so an older identical one can be dropped.
    repeated: std::collections::HashMap<String, String>,
    /// Identical tool calls (name + arguments) seen in the current request,
    /// so a model going in circles is stopped. Reset with `repeated`: both
    /// only make sense while the results they talk about are still in
    /// `messages`, and a compaction takes those away.
    call_counts: std::collections::HashMap<String, u32>,
    /// The language the user last wrote in, when it is not a Latin-script
    /// one. Kept because the reminder rides on the user message, and a
    /// compaction throws every user message away.
    reply_language: Option<&'static str>,
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
        let window = window_of(&client, &cfg);
        let turns = cfg.max_turns;
        Self {
            auto_compact_at: compact_threshold(&client, &cfg),
            client,
            cfg,
            messages: vec![Message::system(prompt::system_prompt(window, turns))],
            always_allow: crate::agent::session::load_allow(),
            tx,
            cancel_slot,
            last_editor_note: None,
            warned_text_calls: false,
            said_undo: false,
            repeated: std::collections::HashMap::new(),
            call_counts: std::collections::HashMap::new(),
            reply_language: None,
        }
    }

    /// Said on the request, and again on the summary that replaces it: a long
    /// task compacts more than once, and after the first one nothing in the
    /// context is in the user's language any more.
    fn language_note(lang: &str) -> String {
        format!(
            "\n\n(the user is writing in {lang}. Answer in {lang}, however this request and the \
             tool results are written.)"
        )
    }

    /// Everything that remembers what is in the context, dropped together
    /// when the context itself is dropped.
    fn forget_call_history(&mut self) {
        self.repeated.clear();
        self.call_counts.clear();
    }

    fn send(&self, ev: AgentEvent) {
        let _ = self.tx.send(ev);
    }

    /// Rebuilds the connection from a config profile without touching the
    /// conversation: the model, server, context window and turn budget can
    /// all change mid-session.
    async fn use_profile(&mut self, name: &str, cfg: &crate::config::Config) {
        let keep_model = self.client.model.clone();
        let mut client = Client::connect(cfg).await;
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
            window,
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

    /// `/undo`: the files thoth changed, back the way they were.
    fn undo(&mut self, list: bool) {
        if list {
            let items = undo::list();
            if items.is_empty() {
                self.send(AgentEvent::Info("nothing to undo in this project".into()));
                return;
            }
            let lines: Vec<String> = items.iter().map(|i| format!("  {i}")).collect();
            self.send(AgentEvent::Info(format!(
                "undo history, newest first:\n{}",
                lines.join("\n")
            )));
            return;
        }
        match undo::undo_last() {
            Err(e) => self.send(AgentEvent::Error(format!("{e:#}"))),
            Ok(r) => {
                let mut out = format!("undid \"{}\"", r.label);
                for f in &r.restored {
                    out.push_str(&format!("\n  restored {f}"));
                }
                for f in &r.skipped {
                    out.push_str(&format!("\n  kept {f}"));
                }
                if r.restored.is_empty() {
                    out.push_str("\n  (nothing was put back)");
                }
                self.send(AgentEvent::Info(out));
                // the model's picture of those files is stale now
                self.messages.push(Message::user(format!(
                    "(the user ran /undo: {} file(s) were restored to how they were before \
                     your last changes. Read them again before editing.)",
                    r.restored.len()
                )));
            }
        }
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
                    // everything this request changed becomes one checkpoint.
                    // Worth saying once: after that the diffs speak for
                    // themselves and a line per request is just noise
                    let n = undo::commit(input.lines().next().unwrap_or(""));
                    if n > 0 && !self.said_undo {
                        self.said_undo = true;
                        self.send(AgentEvent::Info(
                            "the files this changed are saved: /undo puts a whole request back"
                                .into(),
                        ));
                    }
                }
                AgentCmd::Undo { list } => self.undo(list),
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
                    self.forget_call_history();
                    let window = window_of(&self.client, &self.cfg);
                    self.messages.push(Message::system(prompt::system_prompt(
                        window,
                        self.cfg.max_turns,
                    )));
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
        let cap = tools::output_cap(window_of(&self.client, &self.cfg));
        let (content, is_error) = match tools::execute("shell", args, token, cap).await {
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
             current state, and what remains to be done. Write the summary in the language the \
             user used, so the conversation continues in it. Reply with only the summary.",
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
                self.forget_call_history();
                let mut carried =
                    format!("(conversation continued from a compacted summary)\n{summary}");
                if let Some(lang) = self.reply_language {
                    carried.push_str(&Self::language_note(lang));
                }
                self.messages.push(Message::user(carried));
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
        // a result cut to 200 characters is a result the model has to read
        // again, so the count of what it already read goes with it
        self.forget_call_history();
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
        // a short "ok" in the middle of a Thai conversation names no
        // language, and is no reason to fall back to English
        if let Some(lang) = prompt::user_language(input) {
            self.reply_language = Some(lang);
        }
        if let Some(lang) = self.reply_language {
            full_input.push_str(&Self::language_note(lang));
        }
        self.messages.push(Message::user(full_input));
        // both depend on where this turn is running: how much room a result
        // may take, and whether there is an editor to ask about problems
        let cap = tools::output_cap(window_of(&self.client, &self.cfg));
        let tools_def = tools::definitions(crate::editor::connected());
        // breaks infinite loops: counts identical tool calls within this task
        self.call_counts.clear();
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
                    // this is the compaction the pending request was asking
                    // for, and the only path out of the loop that skips the
                    // place where the flag is answered: leaving it set
                    // compacts a two-message conversation one turn later,
                    // throwing away the result that turn just fetched
                    compact_pending = false;
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
            // a call read out of the model's own text runs like any other,
            // but the user should know that is what happened
            if !self.warned_text_calls && tool_calls.iter().any(|c| c.id.starts_with("text_call_"))
            {
                self.warned_text_calls = true;
                self.send(AgentEvent::Info(
                    "this model writes tool calls as text instead of sending them. thoth is \
                     reading them anyway; a model with proper tool support will be steadier"
                        .into(),
                ));
            }
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
                    let c = self.call_counts.entry(key).or_insert(0);
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
                let result = self.run_tool(tc, &token, cap).await;
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
                if !is_error {
                    self.drop_stale_copy(tc);
                    self.forget_reads_of(tc);
                }
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

    /// Calls whose result is the same every time, so a second one is a copy
    /// of the first and a third is a model going in circles.
    fn is_read_only(name: &str) -> bool {
        matches!(
            name,
            "read_file" | "grep" | "glob" | "list_dir" | "problems" | "web_fetch" | "web_search"
        )
    }

    /// A file that just changed makes every earlier read of it out of date.
    /// An identical read_file after an edit is not a repeat: it is the only
    /// way to see what is there now, and it is exactly what a model does
    /// after its own edit came out wrong. The breaker counts calls, not
    /// results, so without this it blocks the read that would let the model
    /// recover, and tells it "the result will not change" while the file on
    /// disk says otherwise.
    fn forget_reads_of(&mut self, tc: &ToolCall) {
        if !matches!(
            tc.function.name.as_str(),
            "write_file" | "edit_file" | "multi_edit" | "move_file" | "delete_file"
        ) {
            return;
        }
        let Ok(args) = serde_json::from_str::<Value>(&tc.function.arguments) else {
            return;
        };
        for key in ["path", "from", "to"] {
            let Some(p) = args.get(key).and_then(|v| v.as_str()) else {
                continue;
            };
            // the write may name the file relatively and the read
            // absolutely: match on the name, where over-matching costs one
            // more allowed read of a file that happens to share it
            let name = std::path::Path::new(p)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.to_string());
            let stale = |k: &String| {
                k.split_once(':')
                    .is_some_and(|(tool, args)| Self::is_read_only(tool) && args.contains(&name))
            };
            self.call_counts.retain(|k, _| !stale(k));
            self.repeated.retain(|k, _| !stale(k));
        }
    }

    /// A model that reads the same thing twice leaves two copies of it in the
    /// context, and the older one is dead weight. When a read-only call is
    /// repeated with the exact same arguments, the earlier result is replaced
    /// by a one-line note. Identical arguments is the whole safety condition:
    /// a read of another range, or a search with another pattern, says
    /// something the newer one does not, and is left alone. The todo list is
    /// the exception that needs no arguments to match: it is one list.
    fn drop_stale_copy(&mut self, tc: &ToolCall) {
        let name = tc.function.name.as_str();
        let key = if name == "todo" {
            // the plan is one thing that keeps being rewritten: only the
            // latest version of it means anything
            "todo".to_string()
        } else if Self::is_read_only(name) {
            format!("{name}:{}", tc.function.arguments)
        } else {
            return;
        };
        let Some(old_id) = self.repeated.insert(key, tc.id.clone()) else {
            return;
        };
        if old_id == tc.id {
            return;
        }
        if let Some(m) = self
            .messages
            .iter_mut()
            .find(|m| m.role == "tool" && m.tool_call_id.as_deref() == Some(old_id.as_str()))
        {
            m.content = Some("(the same call was made again later; see that result)".into());
        }
    }

    async fn run_tool(
        &mut self,
        tc: &ToolCall,
        token: &CancellationToken,
        cap: usize,
    ) -> Result<String> {
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
        if !must_ask
            && matches!(
                name,
                "write_file" | "edit_file" | "multi_edit" | "move_file" | "delete_file" | "shell"
            )
        {
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

        tools::execute(name, args, token.clone(), cap).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Client;

    /// Reading the same thing twice leaves two copies in the context. The
    /// older one is dropped, but only when the call was identical: a read of
    /// another range says something the newer one does not.
    #[tokio::test]
    async fn an_identical_read_replaces_the_older_copy() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let cfg = crate::config::resolve(&crate::config::Profile::default());
        let mut agent = Agent::new(
            Client::new(&cfg),
            cfg,
            tx,
            Arc::new(Mutex::new(CancellationToken::new())),
        );
        let call = |id: &str, args: &str| ToolCall {
            id: id.to_string(),
            kind: "function".into(),
            function: crate::client::FunctionCall {
                name: "read_file".into(),
                arguments: args.to_string(),
            },
        };
        let body = |a: &Agent, id: &str| {
            a.messages
                .iter()
                .find(|m| m.tool_call_id.as_deref() == Some(id))
                .and_then(|m| m.content.clone())
                .unwrap()
        };

        let first = call("a", "{\"path\":\"src/main.rs\"}");
        agent
            .messages
            .push(Message::tool("a".into(), "read_file".into(), "one".into()));
        agent.drop_stale_copy(&first);

        // a different range keeps both
        let ranged = call("b", "{\"path\":\"src/main.rs\",\"offset\":40}");
        agent
            .messages
            .push(Message::tool("b".into(), "read_file".into(), "two".into()));
        agent.drop_stale_copy(&ranged);
        assert_eq!(body(&agent, "a"), "one", "a different call must survive");

        // the same call again supersedes the first
        let again = call("c", "{\"path\":\"src/main.rs\"}");
        agent.messages.push(Message::tool(
            "c".into(),
            "read_file".into(),
            "three".into(),
        ));
        agent.drop_stale_copy(&again);
        assert!(
            body(&agent, "a").starts_with("(the same call"),
            "not replaced"
        );
        assert_eq!(body(&agent, "b"), "two");
        assert_eq!(body(&agent, "c"), "three");

        // an edit is never dropped: its result is not a copy of anything
        let edit = ToolCall {
            id: "d".into(),
            kind: "function".into(),
            function: crate::client::FunctionCall {
                name: "edit_file".into(),
                arguments: "{\"path\":\"x\"}".into(),
            },
        };
        agent.messages.push(Message::tool(
            "d".into(),
            "edit_file".into(),
            "edited".into(),
        ));
        agent.drop_stale_copy(&edit);
        agent.messages.push(Message::tool(
            "e".into(),
            "edit_file".into(),
            "edited".into(),
        ));
        let mut second = edit.clone();
        second.id = "e".into();
        agent.drop_stale_copy(&second);
        assert_eq!(body(&agent, "d"), "edited");
    }

    /// The loop breaker counts calls whose results are in the context. A
    /// compaction throws those results away, so a read the model no longer
    /// has must be allowed again: otherwise the file it is working on
    /// becomes unreadable for the rest of the request.
    #[tokio::test]
    async fn compaction_forgets_what_was_called_before_it() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let cfg = crate::config::resolve(&crate::config::Profile::default());
        let mut agent = Agent::new(
            Client::new(&cfg),
            cfg,
            tx,
            Arc::new(Mutex::new(CancellationToken::new())),
        );
        agent.call_counts.insert("read_file:{}".into(), 3);
        agent.repeated.insert("read_file:{}".into(), "a".into());
        agent.forget_call_history();
        assert!(agent.call_counts.is_empty(), "still blocked after compact");
        assert!(agent.repeated.is_empty());
    }

    /// Reading a file again after changing it is not going in circles, it
    /// is the only way to see the change. The breaker has to let that
    /// through, and still stop a read that nothing happened between.
    #[tokio::test]
    async fn a_change_to_a_file_lets_it_be_read_again() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let cfg = crate::config::resolve(&crate::config::Profile::default());
        let mut agent = Agent::new(
            Client::new(&cfg),
            cfg,
            tx,
            Arc::new(Mutex::new(CancellationToken::new())),
        );
        let call = |name: &str, args: &str| ToolCall {
            id: "x".into(),
            kind: "function".into(),
            function: crate::client::FunctionCall {
                name: name.into(),
                arguments: args.to_string(),
            },
        };
        let counted = |a: &Agent, k: &str| a.call_counts.contains_key(k);

        let read_key = "read_file:{\"path\":\"src/lib.rs\"}";
        agent.call_counts.insert(read_key.into(), 3);
        agent
            .call_counts
            .insert("grep:{\"pattern\":\"lib.rs\"}".into(), 2);
        agent
            .call_counts
            .insert("list_dir:{\"path\":\"src\"}".into(), 1);
        agent
            .repeated
            .insert(read_key.into(), "old-result".to_string());

        // an edit somewhere else changes nothing about this file
        agent.forget_reads_of(&call("edit_file", "{\"path\":\"src/main.rs\"}"));
        assert!(counted(&agent, read_key), "another file is another file");

        // the same file, named the other way round, still counts
        agent.forget_reads_of(&call("write_file", "{\"path\":\"C:/p/src/lib.rs\"}"));
        assert!(
            !counted(&agent, read_key),
            "the read has to be allowed again"
        );
        assert!(agent.repeated.is_empty(), "and its result is stale too");
        assert!(
            !counted(&agent, "grep:{\"pattern\":\"lib.rs\"}"),
            "a search that named the file is stale as well"
        );
        assert!(
            counted(&agent, "list_dir:{\"path\":\"src\"}"),
            "a listing that did not name it is untouched"
        );

        // and a call that changes nothing leaves the count alone
        agent.call_counts.insert(read_key.into(), 3);
        agent.forget_reads_of(&call("shell", "{\"command\":\"cargo test\"}"));
        assert!(counted(&agent, read_key));
    }

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
