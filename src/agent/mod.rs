pub mod history;
pub mod prompt;
pub mod session;
pub mod stack;
pub mod undo;

use crate::agent::stack::Stack;
use crate::client::{Client, Message, StreamEvent, ToolCall, Usage};
use crate::tools;
use anyhow::{Result, anyhow, bail};
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
    /// How much to ask before acting, from here on.
    SetMode(PermMode),
}

#[derive(Debug, Clone, Copy)]
pub enum PermReply {
    Yes,
    Always,
    /// Not this one, and carry on with the rest of the task without it.
    Skip,
    /// Not this one, and stop: the user wants to say something first.
    No,
}

/// How much thoth asks before it touches the machine. Shift+tab cycles it,
/// `/mode` names it. It is one session's setting, not a saved one: a mode
/// that outlived the task it was turned on for is how a sandbox setting ends
/// up running against a real repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PermMode {
    /// Ask before anything that touches the machine.
    #[default]
    Manual,
    /// File edits go through; the shell and the network still ask.
    AcceptEdits,
    /// Nothing asks.
    Auto,
    /// Nothing is changed at all. Work it out and say what you would do.
    Plan,
}

impl PermMode {
    pub fn next(self) -> Self {
        match self {
            Self::Manual => Self::AcceptEdits,
            Self::AcceptEdits => Self::Auto,
            Self::Auto => Self::Plan,
            Self::Plan => Self::Manual,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::AcceptEdits => "accept edits",
            Self::Auto => "auto",
            Self::Plan => "plan",
        }
    }

    /// What it changes, in the words of what will happen next.
    pub fn note(self) -> &'static str {
        match self {
            Self::Manual => "every change and command asks first",
            Self::AcceptEdits => "file changes go through, the shell still asks",
            Self::Auto => "nothing asks. every change and command runs",
            Self::Plan => "nothing is changed: thoth works out what to do and says so",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        match s
            .trim()
            .to_ascii_lowercase()
            .replace([' ', '-'], "_")
            .as_str()
        {
            "manual" | "ask" | "default" => Some(Self::Manual),
            "accept_edits" | "edits" | "accept" => Some(Self::AcceptEdits),
            "auto" | "yolo" => Some(Self::Auto),
            "plan" => Some(Self::Plan),
            _ => None,
        }
    }
}

/// What came back from a question the model asked. Not an index into the
/// options: the last row of the chooser is the user writing their own answer,
/// and "none of those, do this instead" has to reach the model as a different
/// thing from "the second one", or it carries on down a road nobody picked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    Picked(String),
    Wrote(String),
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
    /// A plan-mode turn ended with a plan in it, so the UI can offer to
    /// carry it out instead of making the user retype the request.
    PlanReady,
    /// The model asked the user to pick between options before going on.
    Choice {
        question: String,
        options: Vec<String>,
        reply: oneshot::Sender<Option<Answer>>,
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
    /// Tool calls (name + arguments) seen in the current request that came
    /// back with the same answer as the time before, and a hash of that
    /// answer. A model going in circles is stopped; a model watching
    /// something change is not, which is the reason for the hash. Reset with
    /// `repeated`: both only make sense while the results they talk about
    /// are still in `messages`, and a compaction takes those away.
    call_counts: std::collections::HashMap<String, (u32, u64)>,
    /// Files written whole in this request, and how many times, cleared
    /// whenever a check runs. A second write of a file the model has not
    /// found out anything about since the first is the expensive mistake
    /// this catches.
    rewrites: std::collections::HashMap<String, u32>,
    /// The language the user last wrote in, when it is not a Latin-script
    /// one. Kept because the reminder rides on the user message, and a
    /// compaction throws every user message away.
    reply_language: Option<&'static str>,
    /// How much to ask before acting. Set from the UI, never persisted.
    perm_mode: PermMode,
    /// Whether anything was refused in the request being run, so a final
    /// answer that says it was done can be contradicted.
    denied: usize,
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
        let system = prompt::system_prompt(window, turns, &client.model);
        Self {
            auto_compact_at: compact_threshold(&client, &cfg),
            client,
            cfg,
            messages: vec![Message::system(system)],
            always_allow: crate::agent::session::load_allow(),
            tx,
            cancel_slot,
            last_editor_note: None,
            warned_text_calls: false,
            said_undo: false,
            repeated: std::collections::HashMap::new(),
            call_counts: std::collections::HashMap::new(),
            rewrites: std::collections::HashMap::new(),
            reply_language: None,
            perm_mode: PermMode::default(),
            denied: 0,
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

    /// Start in a mode other than manual, from `--mode`. Set before the
    /// agent runs; after that the UI moves it with `AgentCmd::SetMode`.
    pub fn set_mode(&mut self, m: PermMode) {
        self.perm_mode = m;
    }

    /// The model wants the user to decide something before it goes further.
    /// Answering is the point, so a run with nobody at the keyboard says so
    /// rather than hanging on a question no one will ever see.
    async fn ask_user(&mut self, args: &Value, token: &CancellationToken) -> Result<String> {
        let question = args
            .get("question")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let options: Vec<String> = args
            .get("options")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if question.is_empty() || options.len() < 2 {
            bail!("ask_user needs a question and at least two options");
        }
        if options.len() > 9 {
            bail!("ask_user takes at most 9 options; the user has to be able to pick one");
        }
        let (tx, rx) = oneshot::channel();
        self.send(AgentEvent::Choice {
            question,
            options: options.clone(),
            reply: tx,
        });
        let picked = tokio::select! {
            _ = token.cancelled() => None,
            r = rx => r.unwrap_or(None),
        };
        Ok(match picked {
            Some(Answer::Picked(s)) => {
                format!("The user chose: {s}. Carry on with that, and do not ask again about it.")
            }
            // saying only what they typed would read as one of the options
            // coming back in the user's own words, and the model would go on
            // building whichever offer it sounded closest to
            Some(Answer::Wrote(s)) => format!(
                "The user took NONE of the options and answered in their own words: {s}. Do that, \
                 not the nearest thing you offered, and do not ask again about it."
            ),
            None => "The user did not answer. Do not ask again; pick the option that changes \
                     the least, say which one you took and why."
                .into(),
        })
    }

    /// Keeps one turn's tool results inside one turn's share of the window.
    /// Each result is capped on its own, but a model that asks for six files
    /// at once gets six of those, and the request after it goes over the
    /// window. The server does not complain: it drops the front of the
    /// prompt, which is the system prompt and everything the conversation
    /// had agreed so far, and the model answers the last file as if it had
    /// been handed a stranger's code with no question attached. Checking
    /// after the batch is too late, so the budget is spent as it goes.
    fn fit_in_turn(content: String, left: &mut usize) -> Option<String> {
        let size = content.chars().count();
        if size <= *left {
            *left -= size;
            return Some(content);
        }
        // a scrap of a file is worse than none: it reads as the whole thing
        const WORTH_KEEPING: usize = 400;
        if *left < WORTH_KEEPING {
            *left = 0;
            return None;
        }
        let note = "\n…(cut here: the rest of this turn's results would not fit the context. \
                    Ask again for what is missing, on its own.)";
        let keep = left.saturating_sub(note.chars().count());
        let cut: String = content.chars().take(keep).collect();
        *left = 0;
        Some(cut + note)
    }

    /// Whether a call changed a file that something could have compiled or
    /// run. A README with a typo fixed is a change nobody was going to
    /// build, and saying so after every one of those is noise that teaches
    /// the reader to skip the line that matters.
    fn changed_code(tc: &ToolCall) -> bool {
        const CODE: [&str; 20] = [
            "rs", "ts", "tsx", "js", "jsx", "mjs", "py", "go", "java", "kt", "rb", "php", "c", "h",
            "cc", "cpp", "hpp", "cs", "swift", "sh",
        ];
        // a manifest compiles nothing itself, but a dependency written into
        // one and never resolved is the same broken handoff as code that
        // was never built, and a guessed version number is how it happens
        const MANIFESTS: [&str; 6] = [
            "Cargo.toml",
            "package.json",
            "pyproject.toml",
            "go.mod",
            "requirements.txt",
            "build.gradle",
        ];
        if !tools::changes_files(&tc.function.name) {
            return false;
        }
        let Ok(args) = serde_json::from_str::<Value>(&tc.function.arguments) else {
            return false;
        };
        ["path", "to"].iter().any(|k| {
            let Some(p) = args.get(*k).and_then(|v| v.as_str()) else {
                return false;
            };
            let path = std::path::Path::new(p);
            let named = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| MANIFESTS.contains(&n));
            named
                || path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| CODE.contains(&e.to_ascii_lowercase().as_str()))
        })
    }

    /// Which of this project's stacks a call changed a file of, when running
    /// that file would not have checked it. Every compiled stack fails loudly
    /// on its own, so it is not in the answer: rustc refuses to emit, dotnet
    /// refuses to build. The rest hand back a program that starts, answers the
    /// one request the test made, and is wrong everywhere else, and the
    /// "nothing was run" note cannot see it because something *was* run.
    fn changed_unchecked<'a>(tc: &ToolCall, stacks: &[&'a Stack]) -> Option<&'a Stack> {
        if !tools::changes_files(&tc.function.name) {
            return None;
        }
        let args = serde_json::from_str::<Value>(&tc.function.arguments).ok()?;
        let paths = ["path", "to"]
            .iter()
            .filter_map(|k| args.get(*k).and_then(|v| v.as_str()));
        let mut owners = paths.flat_map(|p| {
            stacks
                .iter()
                .filter(move |s| !s.run_checks && s.owns(p))
                .copied()
        });
        owners.next()
    }

    /// Records what a call answered, so the next identical one can be told
    /// apart from the same question asked twice.
    ///
    /// The breaker is there to stop a model going in circles, and its whole
    /// claim is that running this again would tell it nothing new. That is
    /// only true when the answer has not moved. A log being written to is
    /// the case that proves it: thoth hands the model a path and asks it to
    /// watch that file, so the one call thoth told it to make repeatedly
    /// must not be the one thoth punishes it for. A different answer starts
    /// the count again; the same answer three times over is a circle.
    fn note_answer(&mut self, key: String, content: &str) {
        let digest = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            content.hash(&mut h);
            h.finish()
        };
        self.call_counts
            .entry(key)
            .and_modify(|(n, prev)| {
                if *prev == digest {
                    *n += 1;
                } else {
                    *n = 1;
                    *prev = digest;
                }
            })
            .or_insert((1, digest));
    }

    /// The file a call throws away and writes again from scratch, if it does.
    ///
    /// `edit_file` changes part of a file and is how a mistake in one is
    /// meant to be fixed. `write_file` replaces the whole thing, and doing
    /// that twice to the same path with nothing learned in between means the
    /// second version was written out of exactly the knowledge that produced
    /// the first. Matched on the file name, the way `forget_reads_of` does
    /// it, because one call may name the file relatively and the next
    /// absolutely.
    fn overwritten(tc: &ToolCall) -> Option<String> {
        if tc.function.name != "write_file" {
            return None;
        }
        let args = serde_json::from_str::<Value>(&tc.function.arguments).ok()?;
        let p = args.get("path")?.as_str()?;
        Some(
            std::path::Path::new(p)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.to_string()),
        )
    }

    /// Why this whole-file write is being refused, when it is.
    ///
    /// The expensive failure this exists for: the model writes a file, reads
    /// nothing back, decides on its own that the file is wrong, and writes
    /// the same file again. Each pass costs a full generation and every one
    /// of them is guesswork, because nothing between them told it anything.
    /// The way out is in the message, and it is the cheap one either way:
    /// run the check, or change the part that is wrong instead of all of it.
    fn rewritten_blindly(&self, tc: &ToolCall, stacks: &[&Stack]) -> Option<String> {
        let name = Self::overwritten(tc)?;
        if *self.rewrites.get(&name)? == 0 {
            return None;
        }
        // Only when there is a check to name, and the reason is not the
        // wording. `ran_a_check` reads the same list, so with no stack in it
        // nothing the model can run counts as having found anything out, and
        // a block here would be one it can never lift: it would be locked
        // out of writing that file for the rest of the request. A guard that
        // cannot say what would satisfy it has no business refusing.
        let s = stacks
            .iter()
            .find(|s| s.owns(&name))
            .or_else(|| stacks.first())?;
        Some(format!(
            "STOP: {name} was already written whole in this request, and nothing has been run \
             against it since. This rewrite is made out of the same guesses as the last one. \
             Find out what is wrong with it first ({}), then change those parts with edit_file. \
             Writing the file again from scratch replaces untested code with untested code.",
            s.check
        ))
    }

    /// Whether a call told the model something it did not know about the code
    /// it had just changed. Wider than `ran_a_check` on purpose.
    ///
    /// `problems` is the editor's language server answering, and it exists
    /// only when there is an editor on the other end. It is not the build, so
    /// it does not settle the question the end-of-request note asks. But it is
    /// unquestionably finding something out, and the guards that refuse on the
    /// grounds that nothing has been found out have to see it, or a model that
    /// does what the `problems` tool description tells it to do gets refused
    /// for the rest of the request with no way to clear the refusal. That is
    /// the same trap the empty stack list was, and the same rule applies: a
    /// guardrail that refuses has to be able to say what would satisfy it, and
    /// everything it would accept has to be reachable.
    fn learned_something(tc: &ToolCall, stacks: &[&Stack]) -> bool {
        tc.function.name == "problems" || Self::ran_a_check(tc, stacks)
    }

    /// Whether a shell call ran one of this project's checks. The words that
    /// count are the stack's own, out of the same table the prompt reads.
    fn ran_a_check(tc: &ToolCall, stacks: &[&Stack]) -> bool {
        if tc.function.name != "shell" {
            return false;
        }
        let Ok(args) = serde_json::from_str::<Value>(&tc.function.arguments) else {
            return false;
        };
        let Some(cmd) = args.get("command").and_then(|v| v.as_str()) else {
            return false;
        };
        stacks.iter().any(|s| s.checked_by(cmd))
    }

    /// Whether the running mode answers the permission question by itself.
    /// Auto answers it for everything; accept-edits only for changes to
    /// files, which are the ones with a diff shown and an undo behind them.
    /// The shell and the network keep asking there, because a command is not
    /// a diff and cannot be taken back.
    fn mode_allows(&self, tool: &str) -> bool {
        match self.perm_mode {
            PermMode::Auto => true,
            PermMode::AcceptEdits => tools::changes_files(tool),
            PermMode::Manual | PermMode::Plan => false,
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
                        &self.client.model,
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
                AgentCmd::SetMode(m) => {
                    self.perm_mode = m;

                    self.send(AgentEvent::Info(format!("{} mode: {}", m.name(), m.note())));
                }
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
        let window = window_of(&self.client, &self.cfg);
        let (content, is_error) = match tools::execute("shell", args, token, window).await {
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
        // said on the request, not in the system prompt, because the mode is
        // switched mid-session and the system prompt is built once
        if self.perm_mode == PermMode::Plan {
            full_input.push_str(
                "\n\n(plan mode: do not change anything. Read, search and run nothing that \
                 writes. Answer with the plan itself: the files to change and what the change \
                 to each one is. Keep it to what you would actually do, in a few lines.)",
            );
        }
        self.messages.push(Message::user(full_input));
        // both depend on where this turn is running: the budgets the tools
        // size themselves against, and whether there is an editor to ask
        // about problems
        let window = window_of(&self.client, &self.cfg);
        let tools_def = tools::definitions(crate::editor::connected());
        // breaks infinite loops: counts tool calls within this task that
        // came back with the answer they came back with last time
        self.call_counts.clear();
        self.rewrites.clear();
        // what this request actually did, for the note at the end of it
        let (mut changed_files, mut ran_command) = (false, false);
        // the stacks are read once for the request: a check is a fact about
        // the project, and an existing project does not change kind mid-turn.
        // A directory with nothing in it is the exception, and it is a whole
        // kind of request: "build me a project" starts with no manifest, so
        // there is nothing to detect until the model writes one, and until
        // then nothing it runs can be recognised as a check
        let mut stacks = stack::here();
        let mut unchecked: Option<&Stack> = None;
        let mut checked = false;
        // the one turn spent asking for a check that was never run, so a
        // model that will not run it is not asked round and round
        let mut nudged = false;
        // code has been changed and nothing has been run against it *since*.
        // Not "no command ran this request": a model that builds, then edits
        // twice more and stops has run a command and checked nothing, and
        // that is the shape this is here to catch
        let mut unchecked_edits = false;
        // replies with nothing in them at all, so asking again cannot become
        // its own loop
        let mut empty_replies = 0u32;
        self.denied = 0;
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
                // Nothing at all came back: no answer, no call, and not the
                // context running out either. A thinking model does this
                // when it spends the reply on reasoning and emits none of
                // it, and taking it for "finished" ends the request wherever
                // it happened to be. It is not an answer, so it is not
                // treated as one: ask again, a couple of times, and only
                // then hand it back to the user.
                if !had_content && !turn.truncated {
                    empty_replies += 1;
                    if empty_replies <= 2 {
                        self.messages.push(Message::user(
                            "That reply was empty. Carry on from where you were, or say what \
                             is stopping you."
                                .to_string(),
                        ));
                        self.send(AgentEvent::Info(
                            "empty reply from the model, asking again".into(),
                        ));
                        continue;
                    }
                    self.send(AgentEvent::Info(
                        "(empty response from the model. rephrase, or /compact to free context)"
                            .into(),
                    ));
                } else if self.perm_mode == PermMode::Plan {
                    // it stopped with something to say and could not have
                    // changed anything to say it: that is the plan
                    self.send(AgentEvent::PlanReady);
                }
                // Before any of the notes below: they tell the user that the
                // work was not checked, which is worth saying, but they say
                // it once the model has already stopped and cannot act on
                // it. Say it to the model first, once, while there is still
                // a turn left to fix something in. Only when there is a
                // check to name, and only when nothing was run at all: a
                // model that ran its own program and drew the wrong
                // conclusion needs the note, not another turn.
                if !nudged
                    && unchecked_edits
                    && let Some(s) = stacks.first()
                {
                    nudged = true;
                    self.messages.push(Message::user(format!(
                        "You changed code and have not run anything against it since. Run {} now. \
                         If it reports problems, fix them and run it again. If you cannot run \
                         it, say so plainly instead of saying the work is done.",
                        s.check
                    )));
                    self.send(AgentEvent::Info(
                        "code changed and nothing was run: asking the model to check it".into(),
                    ));
                    continue;
                }
                // The prompt tells the model to build what it changed, and
                // a model that did not do it still writes "build passed" at
                // the end. Whether a command ran is not its word against
                // itself: thoth watched every tool call this request made.
                // a model that was told no still signs off with "written the
                // file, it has hello in it". Whether the user said yes is
                // not something to take its word on
                if self.denied > 0 {
                    self.send(AgentEvent::Info(format!(
                        "note: {} action(s) were denied this request and did not happen. \
                         Anything above saying they were done is wrong",
                        self.denied
                    )));
                }
                if changed_files && !ran_command {
                    self.send(AgentEvent::Info(
                        "note: files changed and nothing was built or run this request. Anything \
                         above about it building or passing was not checked here"
                            .into(),
                    ));
                } else if let Some(s) = unchecked.filter(|_| !checked) {
                    // the harder half of the same lie: something did run, so
                    // the note above stays quiet, and the model has watched
                    // its own program answer a request. None of that looked at
                    // the code it did not reach
                    self.send(AgentEvent::Info(format!(
                        "note: {} changed and none of it was checked this request. Running it is \
                         not the check here, {}",
                        s.name, s.because
                    )));
                }
                // "closing the server now" and then stopping leaves it
                // holding the port for the rest of the day. Whether it is
                // still up is the operating system's answer, not the
                // model's word about itself
                let left = tools::shell::still_running();
                if !left.is_empty() {
                    let list: Vec<String> = left.iter().map(u32::to_string).collect();
                    self.send(AgentEvent::Error(format!(
                        "still running in the background: pid {}. Say \"stop it\" to have thoth \
                         kill it, or leave it if you meant to",
                        list.join(", ")
                    )));
                }
                return Ok(());
            }

            // what all of this turn's results together may take
            let mut turn_budget = tools::output_cap(window);
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
                let n = self.call_counts.get(&key).map(|(n, _)| *n).unwrap_or(0);
                if n > 2 {
                    let content = format!(
                        "STOP: this exact {} call was already run {n} times and gave the same \
                         answer every time. Take a different approach, or explain what is \
                         blocking you and stop.",
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
                if let Some(content) = self.rewritten_blindly(tc, &stacks) {
                    self.send(AgentEvent::ToolStart {
                        name: tc.function.name.clone(),
                        summary: "(rewrite blocked: nothing was checked since the last one)".into(),
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
                let result = self.run_tool(tc, &token, window).await;
                let (content, is_error) = match result {
                    Ok(s) => (s, false),
                    Err(e) => (format!("Error: {e:#}"), true),
                };
                let (content, is_error) = match Self::fit_in_turn(content, &mut turn_budget) {
                    Some(c) => (c, is_error),
                    // over budget entirely: say so as an error, so the model
                    // reads it as something to do differently rather than as
                    // the file being empty
                    None => (
                        "(not read: the results of this turn have already filled the context. \
                         Ask for this one on its own, or with offset and limit.)"
                            .to_string(),
                        true,
                    ),
                };
                self.note_answer(key, &content);
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
                    // a manifest just written is a project that did not
                    // exist when this request started. Only while there is
                    // nothing to find: once a stack is known, the listing is
                    // settled and re-reading it every call would be waste
                    if stacks.is_empty() && tools::changes_files(&tc.function.name) {
                        stacks = stack::here();
                    }
                    self.drop_stale_copy(tc);
                    self.forget_reads_of(tc);
                    changed_files |= Self::changed_code(tc);
                    ran_command |= tc.function.name == "shell";
                    unchecked = unchecked.or_else(|| Self::changed_unchecked(tc, &stacks));
                    if Self::changed_code(tc) {
                        unchecked_edits = true;
                    }
                    if Self::ran_a_check(tc, &stacks) {
                        checked = true;
                        // the edits up to here have had their answer
                        unchecked_edits = false;
                    }
                    if Self::learned_something(tc, &stacks) {
                        // every file is fair to write again on the strength
                        // of it. Deliberately wider than `checked`: the
                        // editor's diagnostics unblock the model without
                        // settling what the user is told at the end
                        self.rewrites.clear();
                    }
                    if let Some(f) = Self::overwritten(tc) {
                        *self.rewrites.entry(f).or_insert(0) += 1;
                    }
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

    async fn run_tool(
        &mut self,
        tc: &ToolCall,
        token: &CancellationToken,
        window: Option<u32>,
    ) -> Result<String> {
        let name = tc.function.name.as_str();
        let args: Value = serde_json::from_str(&tc.function.arguments)
            .map_err(|e| anyhow!("invalid JSON in tool arguments: {e}"))?;

        self.send(AgentEvent::ToolStart {
            name: name.to_string(),
            summary: tools::summarize(name, &args),
        });

        // plan mode is a wall, not a prompt: the answer to "may I change
        // this" is no, and saying so as a tool result is what tells the
        // model to describe the change instead of trying again
        if self.perm_mode == PermMode::Plan && tools::changes_files(name) {
            let content = format!(
                "Plan mode is on, so nothing may be changed yet. Do not call {name} or any other \
                 tool that writes. Work the task out with the read-only tools and describe what \
                 you would do: the files, and for each one what the change is. The user turns \
                 plan mode off when they want it carried out."
            );
            self.send(AgentEvent::ToolResult {
                content: content.clone(),
                is_error: true,
            });
            return Ok(content);
        }

        // the one tool the user answers rather than the machine
        if name == "ask_user" {
            return self.ask_user(&args, token).await;
        }

        let key = tools::permission_key(name, &args);
        let must_ask = tools::needs_permission(name, &args)
            && !self.always_allow.contains(&key)
            && !self.mode_allows(name);
        // permission skipped (always-allowed): still show exactly what runs —
        // the full diff for file changes, the full command line for shell
        if !must_ask && (tools::changes_files(name) || name == "shell") {
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
                PermReply::Skip => {
                    self.denied += 1;
                    return Ok(
                        "The user skipped this action. It did NOT happen. Leave it out and carry \
                         on with the rest of the task; say at the end which step was skipped and \
                         what that leaves undone. Never report it as done."
                            .into(),
                    );
                }
                PermReply::No => {
                    self.denied += 1;
                    return Ok(
                        "The user denied this action. It did NOT happen. Do not retry it, do not \
                         work around it, and do not report it as done. Stop here: say what is \
                         missing and ask the user how to go on."
                            .into(),
                    );
                }
            }
        }

        tools::execute(name, args, token.clone(), window).await
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
        agent.call_counts.insert("read_file:{}".into(), (3, 0));
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
        agent.call_counts.insert(read_key.into(), (3, 0));
        agent
            .call_counts
            .insert("grep:{\"pattern\":\"lib.rs\"}".into(), (2, 0));
        agent
            .call_counts
            .insert("list_dir:{\"path\":\"src\"}".into(), (1, 0));
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
        // the copy taken before the change is not merely old, it is wrong.
        // What lets the re-read supersede it has to survive, or both
        // versions of the file sit in the context at once
        assert_eq!(
            agent.repeated.get(read_key).map(String::as_str),
            Some("old-result"),
            "the stale copy would be left in the context beside the new one"
        );
        assert!(
            !counted(&agent, "grep:{\"pattern\":\"lib.rs\"}"),
            "a search that named the file is stale as well"
        );
        assert!(
            counted(&agent, "list_dir:{\"path\":\"src\"}"),
            "a listing that did not name it is untouched"
        );

        // a command names no file and reads the whole tree, so a change to
        // any of it makes the same command a different answer: running the
        // tests again after a fix is the habit, not a loop
        agent
            .call_counts
            .insert("shell:{\"command\":\"cargo test\"}".into(), (3, 0));
        agent.forget_reads_of(&call("edit_file", "{\"path\":\"src/main.rs\"}"));
        assert!(
            !counted(&agent, "shell:{\"command\":\"cargo test\"}"),
            "the tests have to be runnable again after the fix"
        );

        // and a call that changes nothing leaves the count alone
        agent.call_counts.insert(read_key.into(), (3, 0));
        agent.forget_reads_of(&call("shell", "{\"command\":\"cargo test\"}"));
        assert!(counted(&agent, read_key));
    }

    /// Six files asked for at once used to arrive as six full results, and
    /// the request after them went over the window. The server does not say
    /// so, it drops the front of the prompt, and the model answers the last
    /// file with "what would you like me to do with this?".
    #[test]
    fn one_turn_of_results_stays_inside_one_turn_of_room() {
        let mut left = 1_000usize;
        let whole = Agent::fit_in_turn("x".repeat(600), &mut left).unwrap();
        assert_eq!(whole.chars().count(), 600, "what fits arrives whole");
        assert_eq!(left, 400);

        // the next one only half fits, and says where it was cut
        let cut = Agent::fit_in_turn("y".repeat(600), &mut left).unwrap();
        assert!(cut.chars().count() <= 400, "{}", cut.chars().count());
        assert!(cut.contains("cut here"), "{cut}");
        assert!(cut.contains("Ask again"), "it has to say what to do: {cut}");
        assert_eq!(left, 0);

        // and once there is no room, a scrap is worse than nothing
        assert!(Agent::fit_in_turn("z".repeat(600), &mut left).is_none());

        // a result that fits exactly is not cut
        let mut left = 5usize;
        assert_eq!(
            Agent::fit_in_turn("abcde".into(), &mut left).as_deref(),
            Some("abcde")
        );
    }

    /// The note about an unchecked change is for code, not for every file:
    /// after a typo fixed in a README it would be noise, and noise is what
    /// teaches a reader to skip the line that matters.
    #[test]
    fn only_a_change_to_code_is_worth_saying_was_not_built() {
        let call = |name: &str, args: &str| ToolCall {
            id: "x".into(),
            kind: "function".into(),
            function: crate::client::FunctionCall {
                name: name.into(),
                arguments: args.to_string(),
            },
        };
        assert!(Agent::changed_code(&call(
            "write_file",
            "{\"path\":\"src/main.rs\"}"
        )));
        assert!(Agent::changed_code(&call(
            "multi_edit",
            "{\"path\":\"C:/p/app.TS\"}"
        )));
        assert!(Agent::changed_code(&call(
            "move_file",
            "{\"from\":\"a.md\",\"to\":\"b.py\"}"
        )));
        assert!(!Agent::changed_code(&call(
            "edit_file",
            "{\"path\":\"README.md\"}"
        )));
        // a dependency written in and never resolved is the same handoff
        assert!(Agent::changed_code(&call(
            "edit_file",
            "{\"path\":\"Cargo.toml\"}"
        )));
        assert!(Agent::changed_code(&call(
            "write_file",
            "{\"path\":\"C:/p/package.json\"}"
        )));
        assert!(!Agent::changed_code(&call(
            "write_file",
            "{\"path\":\"notes\"}"
        )));
        assert!(!Agent::changed_code(&call("shell", "{\"command\":\"ls\"}")));
    }

    /// A stack whose runtime does not check it needs its own pair, because
    /// the note they feed fires in the case the one above cannot see: a
    /// command did run, it just was not a check. Which words count is the
    /// table's business, not this loop's; what is tested here is that the
    /// loop asks it about the right stacks and the right calls.
    #[test]
    fn a_check_is_recognised_and_running_the_file_is_not() {
        let call = |name: &str, args: &str| ToolCall {
            id: "x".into(),
            kind: "function".into(),
            function: crate::client::FunctionCall {
                name: name.into(),
                arguments: args.to_string(),
            },
        };
        let cmd = |c: &str| {
            call(
                "shell",
                &format!("{{\"command\":{}}}", serde_json::json!(c)),
            )
        };
        let listing =
            |names: &[&str]| -> Vec<String> { names.iter().map(|s| (*s).to_string()).collect() };
        let ts = stack::detect(&listing(&["tsconfig.json", "package.json"]));
        let rust = stack::detect(&listing(&["Cargo.toml"]));

        let changed =
            |tc: &ToolCall, st: &[&'static Stack]| Agent::changed_unchecked(tc, st).map(|s| s.name);
        assert_eq!(
            changed(&call("write_file", "{\"path\":\"src/index.ts\"}"), &ts),
            Some("TypeScript")
        );
        assert_eq!(
            changed(
                &call("move_file", "{\"from\":\"a.md\",\"to\":\"App.TSX\"}"),
                &ts
            ),
            Some("TypeScript")
        );
        // the compiler already read every line of it, so there is nothing to
        // say: a stack that checks itself never reaches the note
        assert_eq!(
            changed(&call("write_file", "{\"path\":\"src/main.rs\"}"), &rust),
            None
        );
        // and a file belonging to no stack that is here is not this note's
        assert_eq!(
            changed(&call("write_file", "{\"path\":\"src/main.rs\"}"), &ts),
            None
        );
        assert_eq!(changed(&call("shell", "{\"command\":\"ls\"}"), &ts), None);

        assert!(Agent::ran_a_check(&cmd("bunx tsc --noEmit"), &ts));
        assert!(!Agent::ran_a_check(&cmd("bun index.ts"), &ts));
        // it has to be a shell call, not a file tool that mentions one
        assert!(!Agent::ran_a_check(
            &call("write_file", "{\"path\":\"tsc.md\"}"),
            &ts
        ));
    }

    /// What each mode answers by itself, and what it still leaves to the
    /// user. A shell command is not a diff and has no undo behind it, so
    /// accept-edits must never wave one through.
    #[tokio::test]
    async fn a_mode_answers_only_what_it_is_meant_to() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let cfg = crate::config::resolve(&crate::config::Profile::default());
        let mut agent = Agent::new(
            Client::new(&cfg),
            cfg,
            tx,
            Arc::new(Mutex::new(CancellationToken::new())),
        );

        for tool in ["write_file", "edit_file", "shell", "web_fetch"] {
            assert!(!agent.mode_allows(tool), "manual asks about {tool}");
        }

        agent.perm_mode = PermMode::AcceptEdits;
        assert!(agent.mode_allows("write_file"));
        assert!(agent.mode_allows("delete_file"));
        assert!(!agent.mode_allows("shell"), "a command is not a diff");
        assert!(!agent.mode_allows("web_fetch"));

        agent.perm_mode = PermMode::Auto;
        for tool in ["write_file", "shell", "web_fetch"] {
            assert!(agent.mode_allows(tool), "auto asks about nothing: {tool}");
        }

        // plan mode does not answer the question, it removes it: the tools
        // that write are stopped before anything is asked
        agent.perm_mode = PermMode::Plan;
        assert!(!agent.mode_allows("write_file"));
        assert!(tools::changes_files("write_file"));
        assert!(!tools::changes_files("shell"));
    }

    #[test]
    fn the_modes_cycle_and_answer_to_their_names() {
        let mut m = PermMode::default();
        assert_eq!(m.name(), "manual");
        let mut seen = vec![m.name()];
        for _ in 0..3 {
            m = m.next();
            seen.push(m.name());
        }
        assert_eq!(seen, ["manual", "accept edits", "auto", "plan"]);
        assert_eq!(m.next(), PermMode::Manual, "it has to come back around");

        assert_eq!(
            PermMode::from_name("Accept Edits"),
            Some(PermMode::AcceptEdits)
        );
        assert_eq!(
            PermMode::from_name("accept-edits"),
            Some(PermMode::AcceptEdits)
        );
        assert_eq!(PermMode::from_name("plan"), Some(PermMode::Plan));
        assert_eq!(PermMode::from_name("nonsense"), None);
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

    /// The rewrite loop this exists to stop: the model writes a file, is
    /// told nothing by anyone, decides on its own that the file is wrong,
    /// and writes the whole file again. Each pass is a full generation, and
    /// each is made out of exactly the knowledge that produced the last one.
    #[test]
    fn a_file_cannot_be_written_from_scratch_twice_without_a_check() {
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
        let listing =
            |names: &[&str]| -> Vec<String> { names.iter().map(|s| (*s).to_string()).collect() };
        let rust = stack::detect(&listing(&["Cargo.toml"]));
        let write = call("write_file", "{\"path\":\"src/main.rs\"}");

        // the first one is how a file gets written at all
        assert!(agent.rewritten_blindly(&write, &rust).is_none());
        *agent.rewrites.entry("main.rs".into()).or_insert(0) += 1;

        // the second, with nothing run in between, is the one that costs
        let blocked = agent
            .rewritten_blindly(&write, &rust)
            .expect("a second whole-file write with nothing learned since");
        assert!(blocked.contains("main.rs"));
        assert!(
            blocked.contains(rust[0].check),
            "it has to name this project's check, not a language it guessed: {blocked}"
        );
        assert!(blocked.contains("edit_file"), "and the cheaper way out");

        // naming the same file the other way round is the same file
        let far = call("write_file", "{\"path\":\"C:/p/src/main.rs\"}");
        assert!(agent.rewritten_blindly(&far, &rust).is_some());

        // a different file has its own first go
        let other = call("write_file", "{\"path\":\"src/lib.rs\"}");
        assert!(agent.rewritten_blindly(&other, &rust).is_none());

        // an edit is the way to fix what one write got wrong, never blocked
        let edit = call("edit_file", "{\"path\":\"src/main.rs\"}");
        assert!(agent.rewritten_blindly(&edit, &rust).is_none());

        // The editor's diagnostics are the other way of finding something
        // out, and they only exist when an editor is attached: a session in
        // the interface with VS Code open gets a `problems` tool that a `-p`
        // run in a bare directory never sees. The tool's own description
        // tells the model to reach for it "before falling back to a full
        // build", so a guard that does not count it refuses the model for
        // doing exactly what it was told, with nothing it can do to clear the
        // refusal. That difference was invisible for a day because every lab
        // case ran `-p` in a temp directory with no editor attached.
        *agent.rewrites.entry("main.rs".into()).or_insert(0) += 1;
        let problems = ToolCall {
            id: "x".into(),
            kind: "function".into(),
            function: crate::client::FunctionCall {
                name: "problems".into(),
                arguments: "{}".to_string(),
            },
        };
        assert!(agent.rewritten_blindly(&write, &rust).is_some());
        assert!(
            !Agent::ran_a_check(&problems, &rust),
            "it is not the build, and the note at the end must still say so"
        );
        assert!(
            Agent::learned_something(&problems, &rust),
            "but it is finding something out, and the block has to lift"
        );
        agent.rewrites.clear();
        assert!(agent.rewritten_blindly(&write, &rust).is_none());

        // and once something has actually been run, the slate is clean
        assert!(Agent::ran_a_check(
            &call("shell", "{\"command\":\"cargo check\"}"),
            &rust
        ));
        agent.rewrites.clear();
        assert!(agent.rewritten_blindly(&write, &rust).is_none());

        // "build me a project" starts in an empty directory: no manifest
        // means no stack, and `ran_a_check` reads the same empty list, so
        // nothing the model could run would lift a block made here. It must
        // not make one, or the file it wrote once can never be written again
        *agent.rewrites.entry("main.rs".into()).or_insert(0) += 1;
        let nothing: Vec<&'static Stack> = Vec::new();
        assert!(!Agent::ran_a_check(
            &call("shell", "{\"command\":\"cargo check\"}"),
            &nothing
        ));
        assert!(
            agent.rewritten_blindly(&write, &nothing).is_none(),
            "a guard with no check to name would be one nothing can satisfy"
        );
    }

    /// The breaker's claim is that running this again would tell the model
    /// nothing new, and that is a claim about the answer, not about the
    /// question. thoth hands out a log path and asks the model to watch that
    /// file: the one call it is told to repeat must not be the one it is
    /// stopped for.
    #[test]
    fn a_call_whose_answer_keeps_changing_is_not_going_in_circles() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let cfg = crate::config::resolve(&crate::config::Profile::default());
        let mut agent = Agent::new(
            Client::new(&cfg),
            cfg,
            tx,
            Arc::new(Mutex::new(CancellationToken::new())),
        );
        let key = "read_file:{\"path\":\"build.log\"}";
        let count = |a: &Agent| a.call_counts.get(key).map(|(n, _)| *n).unwrap_or(0);

        // a log that is still being written to: every read says something new
        for (i, line) in ["compiling 1/400", "compiling 90/400", "compiling 400/400"]
            .iter()
            .enumerate()
        {
            agent.note_answer(key.into(), line);
            assert_eq!(count(&agent), 1, "read {i} moved, so it starts over");
        }

        // the same answer over and over is the circle the breaker is for
        agent.note_answer(key.into(), "done");
        assert_eq!(count(&agent), 1);
        agent.note_answer(key.into(), "done");
        assert_eq!(count(&agent), 2);
        agent.note_answer(key.into(), "done");
        assert_eq!(count(&agent), 3, "the next one is refused");

        // and one new byte is enough to let it go again
        agent.note_answer(key.into(), "done, with a warning");
        assert_eq!(count(&agent), 1);
    }
}
