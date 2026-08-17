pub mod clipboard;
pub mod config;
/// Not in a release binary at all. It is a development tool, and its made-up
/// transcripts are dead weight in something a user installs.
#[cfg(debug_assertions)]
pub mod demo;
pub mod input;
pub mod keys;
pub mod render;
mod screen;
pub mod theme;

use crate::agent::{AgentCmd, AgentEvent, Answer, PermMode, PermReply};
use crate::ui::config::{ConfigAction, ConfigScreen};
use crate::ui::input::{
    byte_idx, command_at, complete_candidates, complete_commands, cwd, expand_mentions, mention_at,
    split_path_fragment,
};
use crate::ui::render::{fmt_usd, printable};
use crate::ui::theme::SPINNER;
use anyhow::Result;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, MouseEventKind,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::text::Line;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

const RESULT_PREVIEW_LINES: usize = 4;
/// Transcript blocks kept in memory. Tool results can be 12k chars each, so
/// an all-day session would grow without this.
const MAX_BLOCKS: usize = 600;

const HELP: &str = "commands:
  /help          show this help
  /clear         clear screen and reset conversation context
  /compact       summarize the conversation to free context space
  /recap         load the previous session's summary into context
  /memory        show project memory (/memory clear to wipe)
  /config        edit the config profiles and switch between them (/cfg)
  /undo          put back the files the last request changed (/undo list)
  /copy          copy the last reply to the clipboard (/copy all: everything)
  /allow         tools always allowed here (/allow reset to clear)
  /mode          how much thoth asks: manual, accept edits, auto, plan
  /plan          short for /mode plan
  /status        session info: profile, model, api, tokens, cost, uptime
  /init          analyze the project and generate THOTH.md
  /model NAME    switch model
  /models        list models available on the server
  /quit          exit
input:
  /              a list of these commands opens as you type; tab takes one
  @path          attach a file to your message (a file picker opens as you type)
  !command       run a command yourself; its output goes into the context
keys:
  enter          send
  shift+enter    new line in the message (also alt+enter, ctrl+j, or \\ then enter)
  shift+tab      cycle mode: manual / accept edits / auto / plan
  tab / enter    take the highlighted path while the picker is open
  esc            close the picker / interrupt generation / clear input
  up / down      move in the picker, then between the lines of the message,
                 otherwise input history
  mouse wheel / pgup / pgdn   scroll transcript
  ctrl+o         expand / collapse long tool outputs
  ctrl+y         copy the last reply to the clipboard
  ctrl+t         hand the mouse to the terminal so you can select and copy
                 text yourself; ctrl+t again gives thoth the wheel back
  ctrl+c         quit
answering:
  y / a / s / n  permission: once / always / skip it / no, stop
  up/down enter  move through thoth's options and take one (1-9 picks directly)
  last row       none of them: write your own answer instead (esc goes back)
tips: shift while dragging selects text in most terminals; ctrl+t works in all
      start thoth with --continue to resume this project's last conversation";

/// Rows the `@path` picker may take from the transcript.
const PICKER_ROWS: usize = 8;

/// The list of paths shown under the input while an `@path` is being typed.
struct Picker {
    /// Char index of the `@` or `/` that opened it.
    at: usize,
    /// Directory part already typed, e.g. "src/". Kept so accepting an entry
    /// can rebuild the whole mention. Empty for commands, which have no path
    /// in front of them, and that is the whole difference: taking a row is
    /// the same operation either way.
    dir: String,
    items: Vec<String>,
    /// What each row is, drawn after the name. Empty for paths, where the
    /// name is the whole story.
    notes: Vec<String>,
    sel: usize,
    /// First visible row, so a long directory scrolls instead of growing.
    top: usize,
}

impl Picker {
    fn height(&self) -> u16 {
        self.items.len().min(PICKER_ROWS) as u16
    }

    fn move_sel(&mut self, delta: isize) {
        let n = self.items.len();
        if n == 0 {
            return;
        }
        // wraps, so holding one arrow key reaches everything
        self.sel = (self.sel as isize + delta).rem_euclid(n as isize) as usize;
        let rows = PICKER_ROWS.min(n);
        self.top = self
            .top
            .min(self.sel)
            .max((self.sel + 1).saturating_sub(rows));
    }
}

enum ChatBlock {
    /// The startup screen: logo, version, where we are, what to type.
    Banner,
    User(String),
    Reasoning(String),
    Assistant(String),
    Tool {
        name: String,
        summary: String,
        /// Full command line / diff preview, shown between header and result.
        detail: Option<String>,
        result: Option<(String, bool)>,
    },
    Diff(String),
    Info(String),
    Error(String),
}

enum Mode {
    Input,
    Busy,
    Perm(oneshot::Sender<PermReply>),
    /// A plan came back and the user is choosing what to do with it.
    PlanChoice,
    /// The model asked something and is waiting on the answer. The rows are
    /// the options plus one more for an answer of the user's own, so a
    /// question whose real answer nobody offered is still answerable.
    Choice {
        options: Vec<String>,
        /// Highlighted row. `options.len()` is the write-your-own row.
        sel: usize,
        /// First row drawn. Nine options and the row after them want ten
        /// rows, which a short terminal does not have; the drawing code moves
        /// this to keep `sel` among the ones on screen, the same way the
        /// transcript's own scroll is settled while drawing.
        top: usize,
        /// Set while they are writing that answer, holding whatever was
        /// half-typed in the input box when the question arrived: a question
        /// turning up is not a reason to lose a draft.
        typing: Option<String>,
        reply: oneshot::Sender<Option<Answer>>,
    },
}

impl Mode {
    /// Rows the chooser puts on screen: every option, then the one that lets
    /// the user write an answer instead of taking one.
    fn choice_rows(options: &[String]) -> usize {
        options.len() + 1
    }
}

struct App {
    model: String,
    base_url: String,
    /// Config profile this session is running, when there is one.
    profile: Option<String>,
    /// Which wire protocol the session ended up on.
    api: &'static str,
    /// Last answer to /models, so `/model 3` can mean something.
    models: Vec<String>,
    /// Open config screen; while it is up it owns the frame and the keys.
    config: Option<ConfigScreen>,
    blocks: Vec<ChatBlock>,
    input: String,
    /// Cursor position in the input, in chars.
    cursor: usize,
    history: Vec<String>,
    /// Position while browsing history; None = editing a fresh line.
    hist_idx: Option<usize>,
    /// The unfinished line stashed while browsing history.
    draft: String,
    /// Open `@path` picker, rebuilt after every edit of the input.
    picker: Option<Picker>,
    /// Esc closed the picker: stay closed until the mention is edited again.
    picker_off: bool,
    mode: Mode,
    /// None = follow the bottom of the transcript.
    scroll: Option<usize>,
    max_scroll: usize,
    spin: usize,
    quit: bool,
    /// How much thoth asks before it acts. The agent holds the same value;
    /// this copy is what the status line draws.
    perm_mode: PermMode,
    session_start: std::time::Instant,
    turn_start: Option<std::time::Instant>,
    /// Live "In file.rs, N lines selected" label from the IDE extension.
    editor_status: Option<String>,
    tick_count: u64,
    /// Context window, when the api or the profile says what it is.
    window: Option<u32>,
    /// Show tool outputs in full instead of a short preview (ctrl+o).
    expanded: bool,
    /// Whether thoth is taking the mouse. It wants it for the scroll wheel,
    /// but taking it is also what stops the terminal's own click-and-drag
    /// selection, so `ctrl+t` hands it back for as long as the user is
    /// selecting something.
    mouse: bool,
    /// Wrapped-line cache for all blocks except the last (see ensure_cache).
    cache: Vec<Line<'static>>,
    cached_blocks: usize,
    cache_width: usize,
    /// prompt_tokens of the latest model call = current context size.
    ctx_tokens: u64,
    /// completion tokens accumulated over the session.
    out_tokens: u64,
    /// input tokens the provider served from its cache, this session.
    cached_tokens: u64,
    /// USD spent this session, when the profile carries prices.
    spent: Option<f64>,
    cmd_tx: mpsc::UnboundedSender<AgentCmd>,
    cancel_slot: Arc<Mutex<CancellationToken>>,
}

/// What the interface knows about the connection before the first turn.
pub struct Session {
    pub model: String,
    pub base_url: String,
    pub profile: Option<String>,
    pub api: &'static str,
    /// Context window, when anything knows it.
    pub window: Option<u32>,
}

pub async fn run(
    session: Session,
    cmd_tx: mpsc::UnboundedSender<AgentCmd>,
    mut ev_rx: mpsc::UnboundedReceiver<AgentEvent>,
    cancel_slot: Arc<Mutex<CancellationToken>>,
) -> Result<()> {
    let mut terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableBracketedPaste, EnableMouseCapture);
    // Without this a terminal on unix reports shift+enter as plain enter,
    // and there is no way to type a second line. The windows console tells
    // them apart on its own and says it supports nothing, which is why this
    // asks first. Only the disambiguation flag: the others change what every
    // other key looks like.
    let enhanced = matches!(
        ratatui::crossterm::terminal::supports_keyboard_enhancement(),
        Ok(true)
    );
    if enhanced {
        let _ = execute!(
            std::io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }

    // Blocking reader thread feeding terminal events into the async loop.
    let (key_tx, mut key_rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        while let Ok(ev) = event::read() {
            if key_tx.send(ev).is_err() {
                break;
            }
        }
    });

    let mut app = App::new(session, cmd_tx, cancel_slot);

    let mut tick = tokio::time::interval(std::time::Duration::from_millis(100));
    let mut dirty = true;
    while !app.quit {
        tokio::select! {
            Some(ev) = key_rx.recv() => {
                app.on_term_event(ev);
                while let Ok(ev) = key_rx.try_recv() { app.on_term_event(ev); }
                dirty = true;
            }
            ev = ev_rx.recv() => {
                match ev {
                    Some(ev) => {
                        app.on_agent_event(ev);
                        while let Ok(ev) = ev_rx.try_recv() { app.on_agent_event(ev); }
                    }
                    // the agent task is gone (panicked or shut down): say so
                    // instead of spinning forever on a dead channel
                    None => {
                        app.blocks.push(ChatBlock::Error(
                            "the agent stopped unexpectedly. ctrl+c to quit, then restart thoth".into(),
                        ));
                        app.mode = Mode::Input;
                        app.turn_start = None;
                        terminal.draw(|f| app.draw(f))?;
                        break;
                    }
                }
                dirty = true;
            }
            _ = tick.tick() => {
                app.tick_count += 1;
                // spinner + elapsed timer are only visible while working
                if matches!(app.mode, Mode::Busy) {
                    app.spin = (app.spin + 1) % SPINNER.len();
                    dirty = true;
                }
                // refresh the editor status about once a second
                if app.tick_count.is_multiple_of(10) {
                    let s = crate::editor::live_status();
                    if s != app.editor_status {
                        app.editor_status = s;
                        dirty = true;
                    }
                }
            }
        }
        if dirty {
            terminal.draw(|f| app.draw(f))?;
            dirty = false;
        }
    }

    if enhanced {
        // leaving it pushed would follow thoth out and change how the shell
        // after it reads keys
        let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    }
    let _ = execute!(
        std::io::stdout(),
        DisableBracketedPaste,
        DisableMouseCapture
    );
    ratatui::restore();
    Ok(())
}

impl App {
    fn new(
        session: Session,
        cmd_tx: mpsc::UnboundedSender<AgentCmd>,
        cancel_slot: Arc<Mutex<CancellationToken>>,
    ) -> Self {
        Self {
            perm_mode: PermMode::default(),
            mouse: true,
            model: session.model,
            base_url: session.base_url,
            profile: session.profile,
            api: session.api,
            models: Vec::new(),
            config: None,
            blocks: vec![ChatBlock::Banner],
            input: String::new(),
            cursor: 0,
            history: Vec::new(),
            hist_idx: None,
            draft: String::new(),
            picker: None,
            picker_off: false,
            mode: Mode::Input,
            scroll: None,
            max_scroll: 0,
            spin: 0,
            quit: false,
            session_start: std::time::Instant::now(),
            turn_start: None,
            editor_status: crate::editor::live_status(),
            tick_count: 0,
            window: session.window,
            expanded: false,
            cache: Vec::new(),
            cached_blocks: 0,
            cache_width: 0,
            ctx_tokens: 0,
            out_tokens: 0,
            cached_tokens: 0,
            spent: None,
            cmd_tx,
            cancel_slot,
        }
    }

    fn on_agent_event(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::Content(t) => match self.blocks.last_mut() {
                Some(ChatBlock::Assistant(s)) => s.push_str(&t),
                _ => {
                    let t = t.trim_start().to_string();
                    if !t.is_empty() {
                        self.blocks.push(ChatBlock::Assistant(t));
                    }
                }
            },
            AgentEvent::Reasoning(t) => match self.blocks.last_mut() {
                Some(ChatBlock::Reasoning(s)) => s.push_str(&t),
                _ => {
                    let t = t.trim_start().to_string();
                    if !t.is_empty() {
                        self.blocks.push(ChatBlock::Reasoning(t));
                    }
                }
            },
            AgentEvent::ToolStart { name, summary } => {
                self.blocks.push(ChatBlock::Tool {
                    name,
                    summary,
                    detail: None,
                    result: None,
                });
            }
            AgentEvent::ToolResult { content, is_error } => {
                if let Some(i) = self.pending_tool() {
                    if let ChatBlock::Tool { result, .. } = &mut self.blocks[i] {
                        *result = Some((content, is_error));
                    }
                    self.invalidate_from(i);
                }
            }
            AgentEvent::Permission { preview, reply, .. } => {
                self.attach_tool_detail(preview);
                self.mode = Mode::Perm(reply);
            }
            AgentEvent::Diff(t) => self.attach_tool_detail(t),
            AgentEvent::Info(t) => self.blocks.push(ChatBlock::Info(t)),
            AgentEvent::Error(t) => self.blocks.push(ChatBlock::Error(t)),
            AgentEvent::ModelChanged(m) => self.model = m,
            AgentEvent::Models(models) => {
                let list = models
                    .iter()
                    .enumerate()
                    .map(|(i, m)| {
                        format!(
                            "{:>3}. {m}{}",
                            i + 1,
                            if *m == self.model { "  (current)" } else { "" }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                self.blocks.push(ChatBlock::Info(format!(
                    "models on this server:\n{list}\n/model NAME or /model NUMBER to switch"
                )));
                self.models = models;
            }
            AgentEvent::Connected {
                profile,
                model,
                base_url,
                api,
                window,
            } => {
                self.profile = profile;
                self.model = model;
                self.base_url = base_url;
                self.api = api;
                self.window = window;
                // another server, another list: /model 3 must not resolve
                // against what the previous one offered
                self.models.clear();
            }
            AgentEvent::TurnStart => {
                if !matches!(self.mode, Mode::Perm(_)) {
                    self.mode = Mode::Busy;
                }
                if self.turn_start.is_none() {
                    self.turn_start = Some(std::time::Instant::now());
                }
            }
            AgentEvent::Usage { usage, cost } => {
                self.ctx_tokens = usage.prompt_tokens;
                self.out_tokens += usage.completion_tokens;
                self.cached_tokens += usage.cached_tokens;
                if let Some(c) = cost {
                    self.spent = Some(self.spent.unwrap_or(0.0) + c);
                }
            }
            AgentEvent::Choice {
                question,
                options,
                reply,
            } => {
                // only the question goes in the transcript. The options are
                // drawn live under the input, because one of them is
                // highlighted and that changes as the user moves
                self.blocks.push(ChatBlock::Info(question));
                self.scroll = None;
                self.mode = Mode::Choice {
                    options,
                    sel: 0,
                    top: 0,
                    typing: None,
                    reply,
                };
            }
            AgentEvent::PlanReady => {
                // the keys are on the state line one row down for as long as
                // the question is open, so saying them here too puts the same
                // sentence on screen twice
                self.blocks.push(ChatBlock::Info("plan ready".into()));
                self.mode = Mode::PlanChoice;
            }
            AgentEvent::TurnEnd => {
                if !matches!(
                    self.mode,
                    Mode::Perm(_) | Mode::PlanChoice | Mode::Choice { .. }
                ) {
                    self.mode = Mode::Input;
                }
                self.turn_start = None;
                self.trim_blocks();
            }
        }
    }

    /// Drops the oldest part of the transcript once it gets long. Scrollback
    /// is not worth an unbounded heap in a session that runs for hours.
    fn trim_blocks(&mut self) {
        if self.blocks.len() <= MAX_BLOCKS {
            return;
        }
        let drop = self.blocks.len() - MAX_BLOCKS * 3 / 4;
        self.blocks.drain(..drop);
        self.blocks.insert(
            0,
            ChatBlock::Info(format!("({drop} earlier messages dropped from the view)")),
        );
        self.invalidate_cache();
        self.scroll = None;
    }

    // ---- actions ----

    /// Index of the tool block still waiting for its result.
    fn pending_tool(&self) -> Option<usize> {
        self.blocks
            .iter()
            .rposition(|b| matches!(b, ChatBlock::Tool { result: None, .. }))
    }

    /// Puts a command/diff preview inside its tool block so the transcript
    /// reads header -> command -> result in order.
    fn attach_tool_detail(&mut self, text: String) {
        match self.pending_tool() {
            Some(i) => {
                if let ChatBlock::Tool { detail, .. } = &mut self.blocks[i] {
                    *detail = Some(text);
                }
                self.invalidate_from(i);
            }
            None => self.blocks.push(ChatBlock::Diff(text)),
        }
    }

    fn cancel(&self) {
        self.cancel_slot.lock().unwrap().cancel();
    }

    fn submit(&mut self) {
        // typing is allowed while the agent works, messages queue up; only a
        // question waiting on an answer blocks submitting, and while one is
        // being written enter has already been dealt with above
        if matches!(self.mode, Mode::Perm(_) | Mode::Choice { .. }) {
            return;
        }
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.input.clear();
        self.cursor = 0;
        self.hist_idx = None;
        self.draft.clear();
        if self.history.last() != Some(&text) {
            self.history.push(text.clone());
        }
        if let Some(rest) = text.strip_prefix('/') {
            self.command(rest.trim());
            return;
        }
        if let Some(cmd) = text.strip_prefix('!') {
            let cmd = cmd.trim();
            if cmd.is_empty() {
                return;
            }
            self.scroll = None;
            if matches!(self.mode, Mode::Input) {
                self.set_busy();
            } else {
                self.blocks.push(ChatBlock::Info("(queued)".into()));
            }
            let _ = self.cmd_tx.send(AgentCmd::Shell(cmd.to_string()));
            return;
        }
        self.blocks.push(ChatBlock::User(text.clone()));
        let (attachments, labels) = expand_mentions(&text, &cwd());
        for l in labels {
            self.blocks.push(ChatBlock::Info(l));
        }
        self.scroll = None;
        if matches!(self.mode, Mode::Input) {
            self.set_busy();
        } else {
            self.blocks.push(ChatBlock::Info("(queued)".into()));
        }
        let _ = self
            .cmd_tx
            .send(AgentCmd::UserInput(format!("{text}{attachments}")));
    }

    /// A question is on screen and the user is choosing between the rows,
    /// rather than writing an answer of their own. While they are writing,
    /// the input box works exactly as it always does.
    fn picking(&self) -> bool {
        matches!(&self.mode, Mode::Choice { typing, .. } if typing.is_none())
    }

    /// The input box is holding an answer to the model's question rather than
    /// the next message.
    fn answering(&self) -> bool {
        matches!(&self.mode, Mode::Choice { typing, .. } if typing.is_some())
    }

    fn move_choice(&mut self, i: usize) {
        if let Mode::Choice { sel, .. } = &mut self.mode {
            *sel = i;
        }
    }

    /// Taking a row of the chooser. The last one is not an answer, it is the
    /// way to write one, so it opens the input box instead of replying.
    fn take_choice(&mut self, i: usize) {
        let picked = match &self.mode {
            Mode::Choice { options, .. } if i + 1 < Mode::choice_rows(options) => {
                Some(options[i].clone())
            }
            Mode::Choice { .. } => None,
            _ => return,
        };
        match picked {
            Some(s) => self.answer_choice(Some(Answer::Picked(s))),
            None => {
                let draft = std::mem::take(&mut self.input);
                self.cursor = 0;
                if let Mode::Choice { sel, typing, .. } = &mut self.mode {
                    *sel = i;
                    *typing = Some(draft);
                }
            }
        }
    }

    /// Back from writing an answer to the list, with the draft put back the
    /// way it was found.
    fn stop_writing_answer(&mut self) {
        let Mode::Choice { typing, .. } = &mut self.mode else {
            return;
        };
        let Some(draft) = typing.take() else { return };
        self.input = draft;
        self.cursor = self.input.chars().count();
    }

    /// Sends the answer and writes it into the transcript: what the user
    /// decided belongs in the record they can scroll back to, not only in the
    /// model's context.
    fn answer_choice(&mut self, answer: Option<Answer>) {
        if !matches!(self.mode, Mode::Choice { .. }) {
            return;
        }
        let draft = match &mut self.mode {
            Mode::Choice { typing, .. } => typing.take(),
            _ => None,
        };
        let Mode::Choice { reply, .. } = std::mem::replace(&mut self.mode, Mode::Busy) else {
            return;
        };
        if let Some(d) = draft {
            self.input = d;
            self.cursor = self.input.chars().count();
        }
        self.blocks.push(ChatBlock::Info(match &answer {
            Some(Answer::Picked(s)) => format!("chose: {s}"),
            Some(Answer::Wrote(s)) => format!("answered: {s}"),
            None => "left unanswered".into(),
        }));
        let _ = reply.send(answer);
    }

    /// Hands the mouse to the terminal and takes it back. thoth wants it for
    /// the scroll wheel; the terminal wants it to let the user drag out a
    /// selection, and only one of them can have it.
    fn toggle_mouse(&mut self) {
        let want = !self.mouse;
        let mut out = std::io::stdout();
        let done = if want {
            execute!(out, EnableMouseCapture)
        } else {
            execute!(out, DisableMouseCapture)
        };
        if done.is_err() {
            // the flag is what the status line promises the user, so it only
            // moves once the terminal has actually agreed
            self.blocks
                .push(ChatBlock::Error("the terminal kept the mouse".into()));
            return;
        }
        self.mouse = want;
        self.blocks.push(ChatBlock::Info(
            if want {
                "mouse back to thoth: the wheel scrolls again"
            } else {
                "select with the mouse and copy the way you always do. ctrl+t when you are done"
            }
            .into(),
        ));
    }

    /// The last reply, or the whole conversation, onto the clipboard.
    fn copy_out(&mut self, all: bool) {
        let text = self.as_text(all);
        if text.is_empty() {
            self.blocks
                .push(ChatBlock::Info("nothing to copy yet".into()));
            return;
        }
        let lines = text.lines().count();
        self.blocks.push(match clipboard::copy(&text) {
            Err(e) => ChatBlock::Error(format!("could not copy: {e}")),
            Ok(false) => ChatBlock::Info(format!(
                "copied the first {} characters of {lines} lines: the rest is more than a \
                 terminal will take at once",
                clipboard::MAX_COPY_CHARS
            )),
            // it is the terminal that does the copying, and a terminal that
            // does not answer OSC 52 says nothing about it either way. Better
            // to name the way out than to claim a copy that may not have
            // happened
            Ok(true) => ChatBlock::Info(format!(
                "copied {lines} line(s). If nothing landed, this terminal does not take OSC 52: \
                 ctrl+t and select it by hand"
            )),
        });
    }

    /// The transcript as plain text. Reasoning is left out: it is thoth
    /// thinking out loud, not part of the answer anyone wants to paste.
    fn as_text(&self, all: bool) -> String {
        let mut out: Vec<String> = Vec::new();
        for b in &self.blocks {
            let piece = match b {
                ChatBlock::Banner | ChatBlock::Reasoning(_) => continue,
                ChatBlock::User(t) => format!("> {t}"),
                ChatBlock::Assistant(t) => t.clone(),
                ChatBlock::Tool {
                    name,
                    summary,
                    result,
                    ..
                } => {
                    let body = result.as_ref().map(|(c, _)| c.as_str()).unwrap_or("");
                    format!("[{name} {summary}]\n{body}").trim_end().to_string()
                }
                ChatBlock::Diff(t) | ChatBlock::Info(t) | ChatBlock::Error(t) => t.clone(),
            };
            if piece.trim().is_empty() {
                continue;
            }
            if all {
                out.push(piece);
            } else if matches!(b, ChatBlock::Assistant(_)) {
                // not the last block: the last *reply*, which a note or a
                // tool result after it does not stop being
                out = vec![piece];
            }
        }
        out.join("\n\n")
    }

    /// A request thoth sends on the user's behalf: it goes in the transcript
    /// like anything they typed, because it is a turn they will be charged
    /// for and have to read.
    fn send_input(&mut self, text: String) {
        self.blocks.push(ChatBlock::User(text.clone()));
        self.scroll = None;
        if matches!(self.mode, Mode::Input) {
            self.set_busy();
        }
        let _ = self.cmd_tx.send(AgentCmd::UserInput(text));
    }

    fn set_mode(&mut self, m: PermMode) {
        self.perm_mode = m;
        let _ = self.cmd_tx.send(AgentCmd::SetMode(m));
    }

    /// Rebuilds the `@path` picker for whatever the cursor sits in now. Called
    /// after every change to the input, so the list always matches the line.
    fn refresh_picker(&mut self) {
        let chars: Vec<char> = self.input.chars().collect();
        // a mention and a command cannot both be under the cursor: one starts
        // with @ anywhere, the other with / at the very start
        let found = match mention_at(&chars, self.cursor) {
            Some((at, typed)) => {
                let (dir, frag) = split_path_fragment(&typed);
                let items = complete_candidates(&cwd().join(dir), frag);
                Some((at, dir.to_string(), items, Vec::new()))
            }
            None => command_at(&chars, self.cursor).map(|(at, typed)| {
                let (items, notes) = complete_commands(&typed).into_iter().unzip();
                (at, String::new(), items, notes)
            }),
        };
        let Some((at, dir, items, notes)) = found else {
            self.picker = None;
            self.picker_off = false;
            return;
        };
        if self.picker_off {
            return;
        }
        if items.is_empty() {
            self.picker = None;
            return;
        }
        // keep the highlight where the user put it while the list is the same
        let keep = match &self.picker {
            Some(p) => p.at == at && p.dir == dir && p.items == items,
            None => false,
        };
        if keep {
            return;
        }
        self.picker = Some(Picker {
            at,
            dir,
            items,
            notes,
            sel: 0,
            top: 0,
        });
    }

    fn move_pick(&mut self, delta: isize) {
        if let Some(p) = &mut self.picker {
            p.move_sel(delta);
        }
    }

    /// Puts the highlighted entry into the line. Directories keep the picker
    /// open one level down; a file ends the mention with a space.
    fn accept_pick(&mut self) {
        let Some(p) = self.picker.take() else { return };
        let Some(item) = p.items.get(p.sel).cloned() else {
            return;
        };
        let chars: Vec<char> = self.input.chars().collect();
        let cur = self.cursor.min(chars.len());
        let mut next: String = chars[..p.at + 1].iter().collect();
        next.push_str(&p.dir);
        next.push_str(&item);
        if !item.ends_with('/') {
            next.push(' ');
        }
        self.cursor = next.chars().count();
        next.push_str(&chars[cur..].iter().collect::<String>());
        self.input = next;
        self.refresh_picker();
    }

    fn set_busy(&mut self) {
        self.mode = Mode::Busy;
        self.turn_start = Some(std::time::Instant::now());
    }

    fn command(&mut self, cmd: &str) {
        let (name, arg) = cmd
            .split_once(' ')
            .map(|(a, b)| (a, b.trim()))
            .unwrap_or((cmd, ""));
        match name {
            "help" | "h" => self.blocks.push(ChatBlock::Info(HELP.into())),
            "config" | "cfg" => {
                if matches!(self.mode, Mode::Perm(_) | Mode::Choice { .. }) {
                    self.blocks.push(ChatBlock::Info(
                        "answer the question on screen first".into(),
                    ));
                } else {
                    self.config = Some(ConfigScreen::new());
                }
            }
            "quit" | "exit" | "q" => self.quit = true,
            "clear" => {
                self.blocks.clear();
                self.blocks.push(ChatBlock::Banner);
                self.invalidate_cache();
                self.scroll = None;
                self.ctx_tokens = 0;
                self.set_busy();
                let _ = self.cmd_tx.send(AgentCmd::Clear);
            }
            "undo" => {
                let _ = self.cmd_tx.send(AgentCmd::Undo {
                    list: arg == "list",
                });
                self.scroll = None;
            }
            "copy" => self.copy_out(arg == "all"),
            "compact" => {
                self.set_busy();
                let _ = self.cmd_tx.send(AgentCmd::Compact);
            }
            "mode" => match PermMode::from_name(arg) {
                Some(m) => self.set_mode(m),
                None if arg.is_empty() => {
                    self.blocks.push(ChatBlock::Info(format!(
                        "{} mode: {}\n/mode manual | accept edits | auto | plan, or shift+tab to \
                         cycle",
                        self.perm_mode.name(),
                        self.perm_mode.note()
                    )));
                }
                None => self.blocks.push(ChatBlock::Info(format!(
                    "no mode called {arg}. manual, accept edits, auto or plan"
                ))),
            },
            "plan" => self.set_mode(PermMode::Plan),
            "recap" => {
                self.set_busy();
                let _ = self.cmd_tx.send(AgentCmd::Recap);
            }
            "memory" => {
                if arg == "clear" {
                    match crate::tools::memory::clear_memory() {
                        Ok(_) => self
                            .blocks
                            .push(ChatBlock::Info("project memory cleared".into())),
                        Err(e) => self.blocks.push(ChatBlock::Error(format!("{e:#}"))),
                    }
                } else {
                    match crate::tools::memory::load_memory() {
                        Some(m) => self.blocks.push(ChatBlock::Info(format!(
                            "project memory (.thoth/memory.md):\n{m}"
                        ))),
                        None => self.blocks.push(ChatBlock::Info(
                            "no project memory yet. the model saves facts with the remember tool"
                                .into(),
                        )),
                    }
                }
            }
            "allow" | "permissions" => {
                let _ = self.cmd_tx.send(AgentCmd::Permissions {
                    reset: arg == "reset" || arg == "clear",
                });
            }
            "status" => {
                let up = self.session_start.elapsed().as_secs();
                let mut out = format!(
                    "profile:  {}\nmodel:    {}\nserver:   {} ({} api)\ncwd:      {}\ncontext:  {} tokens (last request)\noutput:   {} tokens (session)",
                    self.profile.as_deref().unwrap_or("(none)"),
                    self.model,
                    self.base_url,
                    self.api,
                    std::env::current_dir()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default(),
                    self.ctx_tokens,
                    self.out_tokens,
                );
                if self.cached_tokens > 0 {
                    out.push_str(&format!(
                        "\ncached:   {} input tokens came from the provider's cache",
                        self.cached_tokens
                    ));
                }
                if let Some(s) = self.spent {
                    out.push_str(&format!("\ncost:     {} this session", fmt_usd(s)));
                }
                out.push_str(&format!("\nuptime:   {}m {}s", up / 60, up % 60));
                self.blocks.push(ChatBlock::Info(out));
            }
            "init" => {
                self.blocks.push(ChatBlock::User("/init".into()));
                self.set_busy();
                let _ = self.cmd_tx.send(AgentCmd::UserInput(
                    "Analyze this codebase: read the project config and key source files, then \
                     write THOTH.md documenting: project overview, build/run/test commands, \
                     architecture (main modules and what they do), and conventions. Keep it \
                     under 60 lines. If THOTH.md already exists, read the whole file first and \
                     update it in place, do not delete it. Exception: if THOTH.md is just a \
                     pointer to another instructions file (e.g. AGENTS.md), update that file \
                     instead and leave the pointer as is."
                        .into(),
                ));
            }
            "models" => {
                self.set_busy();
                let _ = self.cmd_tx.send(AgentCmd::ListModels);
            }
            "model" => {
                // a number picks from the last /models listing, which is
                // easier than retyping "qwen/qwen3-235b-a22b-thinking"
                let picked = arg
                    .parse::<usize>()
                    .ok()
                    .and_then(|n| self.models.get(n.wrapping_sub(1)).cloned());
                match (arg.is_empty(), picked) {
                    (true, _) => self
                        .blocks
                        .push(ChatBlock::Info(format!("current model: {}", self.model))),
                    (false, Some(name)) => {
                        self.set_busy();
                        let _ = self.cmd_tx.send(AgentCmd::SetModel(name));
                    }
                    (false, None) if arg.parse::<usize>().is_ok() => {
                        self.blocks.push(ChatBlock::Error(
                            "no model with that number. /models to list them".into(),
                        ));
                    }
                    (false, None) => {
                        self.set_busy();
                        let _ = self.cmd_tx.send(AgentCmd::SetModel(arg.to_string()));
                    }
                }
            }
            _ => self
                .blocks
                .push(ChatBlock::Error(format!("unknown command: /{name}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::PROMPT;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn app() -> App {
        let (cmd_tx, _rx) = mpsc::unbounded_channel();
        let mut a = App::new(
            Session {
                model: "qwen3:8b".into(),
                base_url: "http://localhost:11434/v1".into(),
                profile: None,
                api: "ollama native",
                window: Some(32768),
            },
            cmd_tx,
            Arc::new(Mutex::new(CancellationToken::new())),
        );
        // whatever the developer has open in VS Code must not leak into tests
        a.editor_status = None;
        a
    }

    /// Renders to an off-screen terminal and returns the visible text lines.
    fn screen(app: &mut App, w: u16, h: u16) -> Vec<String> {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn draws_header_input_box_and_status() {
        let mut a = app();
        a.ctx_tokens = 15_400;
        a.out_tokens = 820;
        a.input = "hello".into();
        a.cursor = 5;
        let s = screen(&mut a, 80, 12);

        assert!(s[0].contains("thoth"), "{:?}", s[0]);
        assert!(s[0].contains("qwen3:8b"));
        // the server url is shortened, not printed raw
        assert!(s[0].contains("localhost:11434") && !s[0].contains("http://"));
        // the input sits in a box of its own, above the status line
        assert!(s[8].starts_with('╭') && s[8].ends_with('╮'), "{:?}", s[8]);
        assert!(s[9].contains(PROMPT.trim()), "{:?}", s[9]);
        assert!(s[9].contains("hello"));
        assert!(
            s[10].starts_with('╰') && s[10].ends_with('╯'),
            "{:?}",
            s[10]
        );
        assert!(s[11].contains("/help"), "{:?}", s[11]);
        assert!(s[11].contains("ctx 15.4k/32.8k (46%)"), "{:?}", s[11]);
        assert!(s[11].contains("out 820"));
    }

    /// Enter sends, and there has to be a way to say something that takes
    /// more than one line. Shift+enter is the one people reach for; the
    /// others are there for terminals that never deliver it.
    #[test]
    fn a_message_can_be_written_over_several_lines() {
        let shift = |code| KeyEvent::new(code, KeyModifiers::SHIFT);
        let plain = |code| KeyEvent::new(code, KeyModifiers::NONE);

        let mut a = app();
        a.on_key(plain(KeyCode::Char('a')));
        a.on_key(shift(KeyCode::Enter));
        a.on_key(plain(KeyCode::Char('b')));
        assert_eq!(a.input, "a\nb", "shift+enter has to break the line");

        a.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
        a.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        assert_eq!(a.input, "a\nb\n\n");

        // a trailing backslash is the way through a terminal that delivers
        // none of the three
        a.on_key(plain(KeyCode::Char('\\')));
        a.on_key(plain(KeyCode::Enter));
        assert_eq!(a.input, "a\nb\n\n\n", "the backslash becomes the break");

        // plain enter still sends, with the line breaks intact
        a.input = "one\ntwo".into();
        a.cursor = 7;
        a.on_key(plain(KeyCode::Enter));
        assert!(a.input.is_empty(), "it has to send");
        assert_eq!(a.history.last().map(String::as_str), Some("one\ntwo"));

        // up and down walk the lines of the message, not the history
        a.input = "one\ntwo".into();
        a.cursor = 5; // on the second line
        a.on_key(plain(KeyCode::Up));
        assert_eq!(a.cursor, 1, "up goes to the same column one line above");
        a.on_key(plain(KeyCode::Down));
        assert_eq!(a.cursor, 5);
        // and on a single line they are the history again
        a.input = "solo".into();
        a.cursor = 4;
        a.on_key(plain(KeyCode::Up));
        assert_eq!(a.input, "one\ntwo", "one line means history");
    }

    /// The model can stop and ask, and the answer has to reach it. Esc is
    /// an answer too: it means decide for yourself.
    #[tokio::test]
    async fn a_question_from_the_model_is_picked_from_the_list() {
        let plain = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let ask = |a: &mut App| {
            let (tx, rx) = oneshot::channel();
            a.on_agent_event(AgentEvent::Choice {
                question: "ใช้ตัวไหนดี".into(),
                options: vec!["serde".into(), "miniserde".into()],
                reply: tx,
            });
            rx
        };
        let mut a = app();
        let rx = ask(&mut a);
        let s = screen(&mut a, 80, 16);
        assert!(
            s.iter().any(|l| l.contains("1  serde")),
            "the options have to be on screen: {s:?}"
        );
        assert!(
            s.iter().any(|l| l.contains("something else")),
            "and so does the way out of them: {s:?}"
        );
        assert!(s.iter().any(|l| l.contains("up/down")), "{s:?}");

        // the number is still the shortcut it always was
        a.on_key(plain(KeyCode::Char('2')));
        assert_eq!(
            rx.await.unwrap(),
            Some(Answer::Picked("miniserde".into())),
            "the second option was picked"
        );

        // and so is walking there: down wraps past the write-your-own row
        let rx = ask(&mut a);
        for _ in 0..4 {
            a.on_key(plain(KeyCode::Down));
        }
        a.on_key(plain(KeyCode::Enter));
        assert_eq!(rx.await.unwrap(), Some(Answer::Picked("miniserde".into())));

        // a number nobody offered does nothing at all: a 7 against two
        // options is a typo, and answering it as "no answer" loses the turn
        let mut rx = ask(&mut a);
        a.on_key(plain(KeyCode::Char('7')));
        assert_eq!(rx.try_recv(), Err(oneshot::error::TryRecvError::Empty));

        // esc hands the decision back
        a.on_key(plain(KeyCode::Esc));
        assert_eq!(rx.await.unwrap(), None);
    }

    /// The last row is not one of the model's options, it is the way past
    /// them: none of these, here is what I actually want.
    #[tokio::test]
    async fn the_last_row_lets_the_user_write_their_own_answer() {
        let plain = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let mut a = app();
        // a half-written message is sitting in the box when the question lands
        a.input = "draft".into();
        a.cursor = 5;
        let (tx, rx) = oneshot::channel();
        a.on_agent_event(AgentEvent::Choice {
            question: "which layout?".into(),
            options: vec!["routes/".into(), "mvc".into()],
            reply: tx,
        });

        a.on_key(plain(KeyCode::Up)); // wraps straight onto the last row
        a.on_key(plain(KeyCode::Enter));
        assert!(
            a.input.is_empty(),
            "the box is cleared to type the answer in"
        );
        let s = screen(&mut a, 80, 16);
        assert!(s.iter().any(|l| l.contains("type your answer")), "{s:?}");

        for c in "one file".chars() {
            a.on_key(plain(KeyCode::Char(c)));
        }
        a.on_key(plain(KeyCode::Enter));
        assert_eq!(rx.await.unwrap(), Some(Answer::Wrote("one file".into())));
        assert_eq!(a.input, "draft", "and the draft comes back afterwards");
    }

    /// What lands on the clipboard: the last reply on its own, or the
    /// conversation without the thinking-out-loud in it.
    #[test]
    fn copying_takes_the_reply_and_not_the_furniture() {
        let mut a = app();
        a.blocks = vec![
            ChatBlock::Banner,
            ChatBlock::User("add a health check".into()),
            ChatBlock::Reasoning("looking at the routes".into()),
            ChatBlock::Assistant("first".into()),
            ChatBlock::Assistant("Added `/healthz`.".into()),
            ChatBlock::Tool {
                name: "shell".into(),
                summary: "bun test".into(),
                detail: None,
                result: Some(("2 pass".into(), false)),
            },
            ChatBlock::Info("note: nothing was built".into()),
        ];
        // the last reply, not the last block: a note printed after it does
        // not stop the reply being the thing worth pasting
        assert_eq!(a.as_text(false), "Added `/healthz`.");

        let all = a.as_text(true);
        assert!(all.starts_with("> add a health check"), "{all}");
        assert!(all.contains("[shell bun test]\n2 pass"), "{all}");
        assert!(all.contains("note: nothing was built"), "{all}");
        assert!(!all.contains("looking at the routes"), "thinking: {all}");
        assert!(!all.contains("thoth v"), "no banner: {all}");

        // and with nothing said, it says so instead of copying an empty line
        a.blocks = vec![ChatBlock::Banner];
        assert_eq!(a.as_text(false), "");
        a.copy_out(false);
        assert!(
            matches!(a.blocks.last(), Some(ChatBlock::Info(t)) if t.contains("nothing to copy"))
        );
    }

    /// Nine options and the row after them want ten rows. A short terminal
    /// has fewer, and the ones it cannot show must still be reachable and
    /// still be admitted to.
    #[tokio::test]
    async fn a_chooser_taller_than_the_screen_scrolls() {
        let plain = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let mut a = app();
        let (tx, rx) = oneshot::channel();
        a.on_agent_event(AgentEvent::Choice {
            question: "which".into(),
            options: (1..=9).map(|i| format!("opt{i}")).collect(),
            reply: tx,
        });
        let s = screen(&mut a, 60, 12);
        assert!(s.iter().any(|l| l.contains("opt1")), "{s:?}");
        assert!(
            !s.iter().any(|l| l.contains("opt9")),
            "no room for it: {s:?}"
        );
        assert!(
            s.iter().any(|l| l.contains("more")),
            "and it has to say so: {s:?}"
        );
        // at least one row of what was asked survives the list
        assert!(s.iter().any(|l| l.contains("which")), "{s:?}");

        for _ in 0..8 {
            a.on_key(plain(KeyCode::Down));
        }
        let s = screen(&mut a, 60, 12);
        assert!(s.iter().any(|l| l.contains("opt9")), "walked to it: {s:?}");
        assert!(!s.iter().any(|l| l.contains("opt1")), "{s:?}");
        a.on_key(plain(KeyCode::Enter));
        assert_eq!(rx.await.unwrap(), Some(Answer::Picked("opt9".into())));
    }

    /// Esc while writing goes back to the list rather than answering: the
    /// user changed their mind about writing, not about the question.
    #[tokio::test]
    async fn escaping_out_of_a_written_answer_returns_to_the_options() {
        let plain = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let mut a = app();
        a.input = "draft".into();
        let (tx, mut rx) = oneshot::channel();
        a.on_agent_event(AgentEvent::Choice {
            question: "q".into(),
            options: vec!["a".into(), "b".into()],
            reply: tx,
        });
        a.on_key(plain(KeyCode::Char('3'))); // the write-your-own row
        a.on_key(plain(KeyCode::Char('x')));
        // the arrows must not reach for input history here: the box is
        // holding an answer, not the next message
        a.on_key(plain(KeyCode::Up));
        assert_eq!(a.input, "x");
        a.on_key(plain(KeyCode::Esc));
        assert_eq!(rx.try_recv(), Err(oneshot::error::TryRecvError::Empty));
        assert_eq!(a.input, "draft");
        a.on_key(plain(KeyCode::Char('1')));
        assert_eq!(rx.try_recv(), Ok(Some(Answer::Picked("a".into()))));
    }

    /// Skip is not no: one carries on without the step, the other stops.
    #[tokio::test]
    async fn a_permission_prompt_takes_four_answers() {
        let plain = |code| KeyEvent::new(code, KeyModifiers::NONE);
        for (key, want) in [
            ('y', "allowed"),
            ('a', "allowed"),
            ('s', "skipped"),
            ('n', "denied"),
        ] {
            let mut a = app();
            let (tx, rx) = oneshot::channel();
            a.mode = Mode::Perm(tx);
            a.on_key(plain(KeyCode::Char(key)));
            let got = rx.await.unwrap();
            let said = match got {
                PermReply::Yes | PermReply::Always => "allowed",
                PermReply::Skip => "skipped",
                PermReply::No => "denied",
            };
            assert_eq!(said, want, "key {key}");
        }
    }

    /// The input has to show what is in it: a message of four lines is four
    /// rows, and the cursor sits on the row it is really on.
    #[test]
    fn the_input_grows_to_the_message() {
        let mut a = app();
        a.input = "first\nsecond\nthird".into();
        a.cursor = a.input.chars().count();
        let s = screen(&mut a, 80, 14);

        let rows: Vec<&String> = s.iter().filter(|l| l.contains("second")).collect();
        assert_eq!(rows.len(), 1, "the middle line is drawn: {s:?}");
        // a message that ends in a break has an empty last line, and the
        // box has to have grown for the cursor to be on it
        let mut b = app();
        b.input = "one\n".into();
        b.cursor = 4;
        let s2 = screen(&mut b, 80, 14);
        let one = s2.iter().position(|l| l.contains("one")).unwrap();
        assert!(
            s2[one + 1].trim_matches(['│', ' ']).is_empty() && s2[one + 1].starts_with('│'),
            "the new line needs a row of its own, inside the box: {:?}",
            &s2[one..one + 2]
        );
        let first = s.iter().position(|l| l.contains("first")).unwrap();
        assert!(s[first].contains(PROMPT.trim()), "{:?}", s[first]);
        assert!(
            !s[first + 1].contains(PROMPT.trim()),
            "only the first row carries the mark: {:?}",
            s[first + 1]
        );
        assert!(s[first + 2].contains("third"), "{:?}", s[first + 2]);
    }

    #[test]
    fn busy_state_shows_spinner_and_elapsed() {
        let mut a = app();
        a.mode = Mode::Busy;
        a.turn_start = Some(std::time::Instant::now() - std::time::Duration::from_secs(75));
        let s = screen(&mut a, 80, 12);
        assert!(s[7].contains("working"), "{:?}", s[7]);
        assert!(s[7].contains("1m 15s"), "{:?}", s[7]);
        assert!(s[7].contains("esc to interrupt"));
    }

    #[test]
    fn permission_state_shows_the_three_keys() {
        let mut a = app();
        let (tx, _rx) = oneshot::channel();
        a.mode = Mode::Perm(tx);
        let s = screen(&mut a, 80, 12);
        assert!(s[7].contains("permission needed"), "{:?}", s[7]);
        assert!(s[7].contains("y once") && s[7].contains("a always") && s[7].contains("n no"));
    }

    #[test]
    fn banner_greets_with_the_logo_and_version() {
        let mut a = app();
        let s = screen(&mut a, 80, 24).join("\n");
        let name = format!("thoth v{}", env!("CARGO_PKG_VERSION"));
        assert!(s.contains(theme::LOGO[3]), "no logo:\n{s}");
        assert!(s.contains("/help"), "{s}");
        // the window size is startup information, not a transcript message
        assert!(s.contains("32.8k context window"), "{s}");
        // name, version and tagline belong to the header, not to the art
        let head = s.lines().next().unwrap();
        assert!(head.contains(&name), "{head:?}");
        assert!(head.contains("agentic coding"), "{head:?}");
        assert!(head.contains("qwen3:8b"), "{head:?}");
        assert_eq!(s.matches(&name).count(), 1, "name repeated:\n{s}");

        // the api on the banner is the one actually in use, not a guess
        assert!(s.contains("ollama native api"), "{s}");
        a.api = "anthropic native";
        a.window = Some(200_000);
        a.invalidate_cache();
        let s = screen(&mut a, 80, 24).join("\n");
        assert!(s.contains("anthropic native api  ·  200.0k"), "{s}");
        assert!(!s.contains("ollama"), "{s}");
        a.api = "ollama native";
        a.window = Some(32768);
        a.invalidate_cache();

        // narrow terminal: the art goes, and the tagline gives its room to the
        // model and server, which are the part worth keeping
        let s = screen(&mut a, 26, 24);
        assert!(!s.join("\n").contains(theme::LOGO[3]), "art still drawn");
        assert!(
            s[0].contains("thoth v") && s[0].contains("qwen3:8b"),
            "{:?}",
            s[0]
        );
        assert!(
            !s[0].contains("agentic coding"),
            "tagline held on: {:?}",
            s[0]
        );
    }

    // cargo runs tests from the crate root, so this repo is the fixture the
    // picker completes against
    #[test]
    fn at_path_picker_completes_a_file() {
        let mut a = app();
        a.input = "look at @src/conf".into();
        a.cursor = a.input.chars().count();
        a.refresh_picker();
        assert_eq!(
            a.picker.as_ref().expect("picker is open").items,
            ["config.rs"]
        );
        a.accept_pick();
        assert_eq!(a.input, "look at @src/config.rs ");
        assert!(a.picker.is_none(), "a finished file closes the picker");
    }

    #[test]
    fn at_path_picker_walks_into_a_directory() {
        let mut a = app();
        a.input = "@sr".into();
        a.cursor = 3;
        a.refresh_picker();
        a.accept_pick();
        assert_eq!(a.input, "@src/");
        let p = a.picker.as_ref().expect("still open one level down");
        assert!(
            p.items[0].ends_with('/'),
            "directories first: {:?}",
            p.items
        );
        assert!(p.items.iter().any(|i| i == "main.rs"), "{:?}", p.items);
        // the highlight wraps, and the window follows it
        a.move_pick(-1);
        let p = a.picker.as_ref().unwrap();
        assert_eq!(p.sel, p.items.len() - 1);
        assert!(p.sel < p.top + PICKER_ROWS && p.sel >= p.top);
    }

    #[test]
    fn picker_rows_sit_between_the_input_and_the_status_line() {
        let mut a = app();
        a.input = "@src/".into();
        a.cursor = 5;
        a.refresh_picker();
        let s = screen(&mut a, 80, 20);
        let input = s.iter().position(|l| l.contains("@src/")).expect("input");
        // the box closes under the input, and the candidates start below that
        assert!(s[input + 1].starts_with('╰'), "{:?}", s[input + 1]);
        assert!(
            s[input + 2].contains("agent/"),
            "first candidate: {:?}",
            s[input + 2]
        );
        assert!(s[19].contains("/help"), "status moved: {:?}", s[19]);
    }

    /// A command shown twice is noise, and a command shown by halves is a
    /// broken promise: the user answers a permission prompt on what is drawn.
    #[test]
    fn a_command_is_drawn_once_and_whole() {
        let short = "bun test";
        let mut a = app();
        a.blocks.push(ChatBlock::Tool {
            name: "shell".into(),
            summary: short.into(),
            detail: Some(format!("$ {short}")),
            result: None,
        });
        let s = screen(&mut a, 80, 14);
        assert_eq!(
            s.iter().filter(|l| l.contains(short)).count(),
            1,
            "once, on the header line: {s:?}"
        );

        // too long for the header, so the preview comes back, and all of it
        let long = "cargo test --workspace --all-features -- --nocapture --test-threads=1 \
                    --skip network";
        let mut b = app();
        b.blocks.push(ChatBlock::Tool {
            name: "shell".into(),
            summary: long.into(),
            detail: Some(format!("$ {long}")),
            result: None,
        });
        let s = screen(&mut b, 60, 14);
        let drawn: String = s
            .iter()
            .filter(|l| l.trim_start().starts_with('$') || l.starts_with("    "))
            .map(|l| l.trim().to_string())
            .collect::<Vec<_>>()
            .join(" ");
        for word in long.split_whitespace() {
            assert!(drawn.contains(word), "{word} is missing from {drawn:?}");
        }
        assert!(!drawn.contains('…'), "nothing was cut: {drawn:?}");
    }

    #[test]
    fn tool_output_collapses_until_expanded() {
        let mut a = app();
        a.blocks.push(ChatBlock::Tool {
            name: "grep".into(),
            summary: "fn main".into(),
            detail: None,
            result: Some(("1\n2\n3\n4\n5\n6\n7".into(), false)),
        });
        let s = screen(&mut a, 80, 20).join("\n");
        assert!(s.contains("grep  fn main"), "{s}");
        assert!(s.contains("+3 more lines  ctrl+o"), "{s}");
        a.expanded = true;
        a.invalidate_cache();
        let s = screen(&mut a, 80, 20).join("\n");
        assert!(
            s.contains("  7"),
            "expanded output should show every line:\n{s}"
        );
    }

    /// The startup screen with the @path picker open, for eyeballing colors:
    /// cargo test start_screen -- --ignored --nocapture
    #[test]
    #[ignore]
    fn start_screen_preview() {
        let mut a = app();
        a.input = "look at @src/".into();
        a.cursor = a.input.chars().count();
        a.refresh_picker();
        println!();
        for l in screen(&mut a, 100, 26) {
            println!("{l}");
        }
    }

    /// Prints a full mock screen for eyeballing layout changes:
    /// cargo test ui_preview -- --ignored --nocapture
    #[test]
    #[ignore]
    fn ui_preview() {
        let mut a = app();
        a.ctx_tokens = 15_400;
        a.out_tokens = 820;
        a.editor_status = Some("In main.rs, 7 lines selected".into());
        a.blocks = vec![
            ChatBlock::User("add a health check to the server".into()),
            ChatBlock::Reasoning("looking at the routes\nchecking the framework".into()),
            ChatBlock::Assistant("Adding a `/healthz` route next to the existing ones.".into()),
            ChatBlock::Tool {
                name: "grep".into(),
                summary: "app.get".into(),
                detail: None,
                result: Some(("src/server.ts:12: app.get(\"/\", home)".into(), false)),
            },
            ChatBlock::Tool {
                name: "edit_file".into(),
                summary: "src/server.ts".into(),
                detail: Some(
                    "src/server.ts\n  12   app.get(\"/\", home);\n+ 13   app.get(\"/healthz\", () => new Response(\"ok\"));"
                        .into(),
                ),
                result: Some(("Edited src/server.ts".into(), false)),
            },
            ChatBlock::Info("attached src/server.ts (48 lines)".into()),
        ];
        a.input = "run the tests".into();
        a.cursor = a.input.chars().count();
        println!();
        for l in screen(&mut a, 100, 24) {
            println!("{l}");
        }
    }

    /// The chooser, both halves of it:
    /// cargo test choice_preview -- --ignored --nocapture
    #[test]
    #[ignore]
    fn choice_preview() {
        let plain = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let mut a = app();
        a.blocks = vec![ChatBlock::User("จัดโครงสร้างโปรเจกต์ใหม่ให้หน่อย".into())];
        let (tx, _rx) = oneshot::channel();
        a.on_agent_event(AgentEvent::Choice {
            question: "โครงสร้างแบบไหนดีครับ".into(),
            options: vec![
                "src/routes/todos.ts + src/db.ts, one file per resource".into(),
                "keep index.ts, move only the handlers to src/handlers/".into(),
                "controllers / services / models".into(),
            ],
            reply: tx,
        });
        a.on_key(plain(KeyCode::Down));
        println!("\npicking:");
        for l in screen(&mut a, 100, 20) {
            println!("{l}");
        }
        a.on_key(plain(KeyCode::Up));
        a.on_key(plain(KeyCode::Up));
        a.on_key(plain(KeyCode::Enter));
        a.input = "แยกแค่ route ออกไป ที่เหลือไว้เหมือนเดิม".into();
        a.cursor = a.input.chars().count();
        println!("\nwriting an answer of their own:");
        for l in screen(&mut a, 100, 20) {
            println!("{l}");
        }
    }

    /// The wrap cache must not be thrown away when the block that changed is
    /// the last one, which is re-rendered every frame anyway.
    #[test]
    fn cache_survives_updates_to_the_live_block() {
        let mut a = app();
        a.blocks.push(ChatBlock::User("first".into()));
        a.blocks.push(ChatBlock::Tool {
            name: "shell".into(),
            summary: "ls".into(),
            detail: None,
            result: None,
        });
        screen(&mut a, 80, 20);
        let cached = a.cached_blocks;
        assert!(cached > 0, "nothing was cached");
        a.on_agent_event(AgentEvent::ToolResult {
            content: "done".into(),
            is_error: false,
        });
        assert_eq!(a.cached_blocks, cached, "the cache was dropped needlessly");
    }
}
