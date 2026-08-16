pub mod config;
pub mod input;
pub mod render;
mod screen;
pub mod theme;

use crate::agent::{AgentCmd, AgentEvent, PermReply};
use crate::ui::config::{ConfigAction, ConfigScreen};
use crate::ui::input::{
    byte_idx, complete_candidates, cwd, expand_mentions, mention_at, split_path_fragment,
};
use crate::ui::render::fmt_usd;
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
  /allow         tools always allowed here (/allow reset to clear)
  /status        session info: profile, model, api, tokens, cost, uptime
  /init          analyze the project and generate THOTH.md
  /model NAME    switch model
  /models        list models available on the server
  /quit          exit
input:
  @path          attach a file to your message (a file picker opens as you type)
  !command       run a command yourself; its output goes into the context
keys:
  enter          send
  tab / enter    take the highlighted path while the picker is open
  esc            close the picker / interrupt generation / clear input
  up / down      move in the picker, otherwise input history
  mouse wheel / pgup / pgdn   scroll transcript
  ctrl+o         expand / collapse long tool outputs
  ctrl+c         quit
tips: hold shift while dragging to select text with the mouse
      start thoth with --continue to resume this project's last conversation";

/// Rows the `@path` picker may take from the transcript.
const PICKER_ROWS: usize = 8;

/// The list of paths shown under the input while an `@path` is being typed.
struct Picker {
    /// Char index of the `@` that opened it.
    at: usize,
    /// Directory part already typed, e.g. "src/". Kept so accepting an entry
    /// can rebuild the whole mention.
    dir: String,
    items: Vec<String>,
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
    session_start: std::time::Instant,
    turn_start: Option<std::time::Instant>,
    /// Live "In file.rs, N lines selected" label from the IDE extension.
    editor_status: Option<String>,
    tick_count: u64,
    /// Context window, when the api or the profile says what it is.
    window: Option<u32>,
    /// Show tool outputs in full instead of a short preview (ctrl+o).
    expanded: bool,
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

    // ---- events ----

    fn on_term_event(&mut self, ev: Event) {
        match ev {
            Event::Key(k) if k.kind != KeyEventKind::Release => self.on_key(k),
            Event::Mouse(m) => match m.kind {
                MouseEventKind::ScrollUp => self.scroll_by(-3),
                MouseEventKind::ScrollDown => self.scroll_by(3),
                _ => {}
            },
            Event::Paste(s) if !matches!(self.mode, Mode::Perm(_)) => {
                // pasted code keeps its lines now that the input has them.
                // A lone \r is a line break too, and leaving it in would
                // draw the rest of the line over the start of it
                let s = s.replace("\r\n", "\n").replace('\r', "\n");
                self.insert_str(&s);
                self.picker_off = false;
                self.refresh_picker();
            }
            _ => {}
        }
    }

    fn on_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);

        // the config screen is modal: it gets every key until it closes
        if let Some(screen) = &mut self.config {
            match screen.on_key(k) {
                ConfigAction::Stay => {}
                ConfigAction::Close => self.config = None,
                ConfigAction::Apply(name, cfg) => {
                    let _ = self.cmd_tx.send(AgentCmd::UseProfile { name, cfg });
                }
            }
            return;
        }

        if matches!(self.mode, Mode::Perm(_)) {
            let reply = match k.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => Some(PermReply::Yes),
                KeyCode::Char('a') | KeyCode::Char('A') => Some(PermReply::Always),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(PermReply::No),
                KeyCode::Char('c') if ctrl => {
                    self.quit = true;
                    None
                }
                _ => None,
            };
            if let Some(r) = reply
                && let Mode::Perm(tx) = std::mem::replace(&mut self.mode, Mode::Busy)
            {
                let denied = matches!(r, PermReply::No);
                let _ = tx.send(r);
                self.blocks.push(ChatBlock::Info(
                    if denied { "denied" } else { "allowed" }.into(),
                ));
            }
            return;
        }

        // typing anywhere re-opens a picker that was dismissed with esc
        if matches!(
            k.code,
            KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Delete
        ) {
            self.picker_off = false;
        }
        // while the @path picker is up it owns the keys it needs, and only
        // those: everything else still edits the line underneath
        if self.picker.is_some() && !ctrl {
            match k.code {
                KeyCode::Up => {
                    self.move_pick(-1);
                    return;
                }
                KeyCode::Down => {
                    self.move_pick(1);
                    return;
                }
                KeyCode::Tab | KeyCode::Enter => {
                    self.accept_pick();
                    return;
                }
                KeyCode::Esc => {
                    self.picker = None;
                    self.picker_off = true;
                    return;
                }
                _ => {}
            }
        }

        match k.code {
            KeyCode::Char('c') if ctrl => {
                if matches!(self.mode, Mode::Busy) {
                    self.cancel();
                } else {
                    self.quit = true;
                }
            }
            KeyCode::Char('d') if ctrl && self.input.is_empty() => self.quit = true,
            KeyCode::Char('u') if ctrl => {
                self.input.clear();
                self.cursor = 0;
            }
            KeyCode::Char('o') if ctrl => {
                self.expanded = !self.expanded;
                self.invalidate_cache();
            }
            KeyCode::Esc => {
                if matches!(self.mode, Mode::Busy) {
                    self.cancel();
                } else {
                    self.input.clear();
                    self.cursor = 0;
                }
            }
            // a newline in the input, by every route a terminal offers.
            // shift+enter is what people try first and what modern terminals
            // send; alt+enter and ctrl+j are what the older ones can manage,
            // and a line ending in a backslash works even where none of the
            // three survive the emulator
            KeyCode::Enter
                if k.modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
            {
                self.insert_char('\n')
            }
            KeyCode::Char('j') if ctrl => self.insert_char('\n'),
            KeyCode::Enter
                if self.cursor > 0 && self.input.chars().nth(self.cursor - 1) == Some('\\') =>
            {
                self.cursor -= 1;
                let i = byte_idx(&self.input, self.cursor);
                self.input.remove(i);
                self.insert_char('\n');
            }
            KeyCode::Enter => self.submit(),
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                let i = byte_idx(&self.input, self.cursor);
                self.input.remove(i);
            }
            KeyCode::Delete if self.cursor < self.input.chars().count() => {
                let i = byte_idx(&self.input, self.cursor);
                self.input.remove(i);
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.input.chars().count()),
            KeyCode::Home => self.cursor = self.line_start(self.cursor),
            KeyCode::End => self.cursor = self.line_end(self.cursor),
            // in a written-out multi-line message the arrows belong to the
            // message; history is what they mean when there is one line
            KeyCode::Up if self.line_start(self.cursor) > 0 => self.move_line(-1),
            KeyCode::Down if self.line_end(self.cursor) < self.input.chars().count() => {
                self.move_line(1)
            }
            KeyCode::Up => self.history_prev(),
            KeyCode::Down => self.history_next(),
            KeyCode::PageUp => self.scroll_by(-10),
            KeyCode::PageDown => self.scroll_by(10),
            KeyCode::Char(ch) if !ctrl => self.insert_char(ch),
            _ => {}
        }
        self.refresh_picker();
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
            AgentEvent::TurnEnd => {
                if !matches!(self.mode, Mode::Perm(_)) {
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
        // typing is allowed while the agent works — messages queue up;
        // only the permission prompt blocks submitting
        if matches!(self.mode, Mode::Perm(_)) {
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

    /// Rebuilds the `@path` picker for whatever the cursor sits in now. Called
    /// after every change to the input, so the list always matches the line.
    fn refresh_picker(&mut self) {
        let chars: Vec<char> = self.input.chars().collect();
        let Some((at, typed)) = mention_at(&chars, self.cursor) else {
            self.picker = None;
            self.picker_off = false;
            return;
        };
        if self.picker_off {
            return;
        }
        let (dir, frag) = split_path_fragment(&typed);
        let items = complete_candidates(&cwd().join(dir), frag);
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
            dir: dir.to_string(),
            items,
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
                if matches!(self.mode, Mode::Perm(_)) {
                    self.blocks
                        .push(ChatBlock::Info("answer the permission prompt first".into()));
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
            "compact" => {
                self.set_busy();
                let _ = self.cmd_tx.send(AgentCmd::Compact);
            }
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

    // ---- input editing ----

    fn insert_char(&mut self, ch: char) {
        let i = byte_idx(&self.input, self.cursor);
        self.input.insert(i, ch);
        self.cursor += 1;
    }

    /// Char index of the first character on the line `at` sits on. Lines are
    /// what the user typed, not what the width wrapped: a message written
    /// over four lines moves as four lines however wide the terminal is.
    fn line_start(&self, at: usize) -> usize {
        self.input
            .chars()
            .take(at)
            .collect::<Vec<_>>()
            .iter()
            .rposition(|c| *c == '\n')
            .map(|i| i + 1)
            .unwrap_or(0)
    }

    fn line_end(&self, at: usize) -> usize {
        let chars: Vec<char> = self.input.chars().collect();
        chars[at.min(chars.len())..]
            .iter()
            .position(|c| *c == '\n')
            .map(|i| at + i)
            .unwrap_or(chars.len())
    }

    /// Up or down one typed line, keeping the column where it can.
    fn move_line(&mut self, delta: isize) {
        let col = self.cursor - self.line_start(self.cursor);
        let (start, end) = if delta < 0 {
            let prev_end = self.line_start(self.cursor).saturating_sub(1);
            (self.line_start(prev_end), prev_end)
        } else {
            let next_start = (self.line_end(self.cursor) + 1).min(self.input.chars().count());
            (next_start, self.line_end(next_start))
        };
        self.cursor = (start + col).min(end);
    }

    fn insert_str(&mut self, s: &str) {
        let i = byte_idx(&self.input, self.cursor);
        self.input.insert_str(i, s);
        self.cursor += s.chars().count();
    }

    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let idx = match self.hist_idx {
            None => {
                self.draft = self.input.clone();
                self.history.len() - 1
            }
            Some(i) => i.saturating_sub(1),
        };
        self.hist_idx = Some(idx);
        self.input = self.history[idx].clone();
        self.cursor = self.input.chars().count();
    }

    fn history_next(&mut self) {
        let Some(i) = self.hist_idx else { return };
        if i + 1 < self.history.len() {
            self.hist_idx = Some(i + 1);
            self.input = self.history[i + 1].clone();
        } else {
            self.hist_idx = None;
            self.input = std::mem::take(&mut self.draft);
        }
        self.cursor = self.input.chars().count();
    }

    fn scroll_by(&mut self, delta: i32) {
        let cur = self.scroll.unwrap_or(self.max_scroll);
        let new = if delta < 0 {
            cur.saturating_sub((-delta) as usize)
        } else {
            cur.saturating_add(delta as usize)
        };
        self.scroll = if new >= self.max_scroll {
            None
        } else {
            Some(new)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::{PROMPT, RULE};
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
    fn draws_header_rule_and_prompt() {
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
        // the input sits between a full-width rule and the status line
        assert_eq!(s[9], RULE.repeat(80));
        assert!(s[10].starts_with(PROMPT), "{:?}", s[10]);
        assert!(s[10].contains("hello"));
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
            s2[one + 1].is_empty() && !s2[one + 1].contains(RULE),
            "the new line needs a row of its own: {:?}",
            &s2[one..one + 2]
        );
        let first = s.iter().position(|l| l.contains("first")).unwrap();
        assert!(s[first].starts_with(PROMPT), "{:?}", s[first]);
        assert!(
            !s[first + 1].starts_with(PROMPT),
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
        assert!(s[8].contains("working"), "{:?}", s[8]);
        assert!(s[8].contains("1m 15s"), "{:?}", s[8]);
        assert!(s[8].contains("esc to interrupt"));
    }

    #[test]
    fn permission_state_shows_the_three_keys() {
        let mut a = app();
        let (tx, _rx) = oneshot::channel();
        a.mode = Mode::Perm(tx);
        let s = screen(&mut a, 80, 12);
        assert!(s[8].contains("permission needed"), "{:?}", s[8]);
        assert!(s[8].contains("y once") && s[8].contains("a always") && s[8].contains("n no"));
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
        assert!(s[10].contains("@src/"), "input line: {:?}", s[10]);
        assert!(s[11].contains("agent/"), "first candidate: {:?}", s[11]);
        assert!(s[19].contains("/help"), "status moved: {:?}", s[19]);
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
