use crate::agent::{AgentCmd, AgentEvent, PermReply};
use anyhow::Result;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Position};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const RESULT_PREVIEW_LINES: usize = 4;

const HELP: &str = "commands:
  /help          show this help
  /clear         clear screen and reset conversation context
  /compact       summarize the conversation to free context space
  /recap         load the previous session's summary into context
  /memory        show project memory (/memory clear to wipe)
  /status        session info: model, tokens, uptime
  /init          analyze the project and generate THOTH.md
  /model NAME    switch model
  /models        list models available on the server
  /quit          exit
keys:
  enter          send
  esc            interrupt generation / clear input
  up / down      input history
  mouse wheel / pgup / pgdn   scroll transcript
  ctrl+o         expand / collapse long tool outputs
  ctrl+c         quit
tip: hold shift while dragging to select text with the mouse";

enum ChatBlock {
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
    blocks: Vec<ChatBlock>,
    input: String,
    /// Cursor position in the input, in chars.
    cursor: usize,
    history: Vec<String>,
    /// Position while browsing history; None = editing a fresh line.
    hist_idx: Option<usize>,
    /// The unfinished line stashed while browsing history.
    draft: String,
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
    /// Context window size, when known (Ollama native).
    num_ctx: Option<u32>,
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
    cmd_tx: mpsc::UnboundedSender<AgentCmd>,
    cancel_slot: Arc<Mutex<CancellationToken>>,
}

pub async fn run(
    model: String,
    base_url: String,
    num_ctx: Option<u32>,
    cmd_tx: mpsc::UnboundedSender<AgentCmd>,
    mut ev_rx: mpsc::UnboundedReceiver<AgentEvent>,
    cancel_slot: Arc<Mutex<CancellationToken>>,
) -> Result<()> {
    let mut terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableBracketedPaste, EnableMouseCapture);

    // Blocking reader thread feeding terminal events into the async loop.
    let (key_tx, mut key_rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        while let Ok(ev) = event::read() {
            if key_tx.send(ev).is_err() {
                break;
            }
        }
    });

    let mut app = App {
        model,
        base_url,
        blocks: vec![ChatBlock::Info("/help for commands".into())],
        input: String::new(),
        cursor: 0,
        history: Vec::new(),
        hist_idx: None,
        draft: String::new(),
        mode: Mode::Input,
        scroll: None,
        max_scroll: 0,
        spin: 0,
        quit: false,
        session_start: std::time::Instant::now(),
        turn_start: None,
        editor_status: crate::editor::live_status(),
        tick_count: 0,
        num_ctx,
        expanded: false,
        cache: Vec::new(),
        cached_blocks: 0,
        cache_width: 0,
        ctx_tokens: 0,
        out_tokens: 0,
        cmd_tx,
        cancel_slot,
    };

    let mut tick = tokio::time::interval(std::time::Duration::from_millis(100));
    let mut dirty = true;
    while !app.quit {
        tokio::select! {
            Some(ev) = key_rx.recv() => {
                app.on_term_event(ev);
                while let Ok(ev) = key_rx.try_recv() { app.on_term_event(ev); }
                dirty = true;
            }
            Some(ev) = ev_rx.recv() => {
                app.on_agent_event(ev);
                while let Ok(ev) = ev_rx.try_recv() { app.on_agent_event(ev); }
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

    let _ = execute!(std::io::stdout(), DisableBracketedPaste, DisableMouseCapture);
    ratatui::restore();
    Ok(())
}

impl App {
    // ---- events ----

    fn on_term_event(&mut self, ev: Event) {
        match ev {
            Event::Key(k) if k.kind != KeyEventKind::Release => self.on_key(k),
            Event::Mouse(m) => match m.kind {
                MouseEventKind::ScrollUp => self.scroll_by(-3),
                MouseEventKind::ScrollDown => self.scroll_by(3),
                _ => {}
            },
            Event::Paste(s)
                if !matches!(self.mode, Mode::Perm(_)) => {
                    let s = s.replace("\r\n", " ").replace(['\r', '\n'], " ");
                    self.insert_str(&s);
                }
            _ => {}
        }
    }

    fn on_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);

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
                && let Mode::Perm(tx) = std::mem::replace(&mut self.mode, Mode::Busy) {
                    let denied = matches!(r, PermReply::No);
                    let _ = tx.send(r);
                    self.blocks.push(ChatBlock::Info(
                        if denied { "denied" } else { "allowed" }.into(),
                    ));
                }
            return;
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
            KeyCode::Enter => self.submit(),
            KeyCode::Backspace
                if self.cursor > 0 => {
                    self.cursor -= 1;
                    let i = self.byte_idx(self.cursor);
                    self.input.remove(i);
                }
            KeyCode::Delete
                if self.cursor < self.input.chars().count() => {
                    let i = self.byte_idx(self.cursor);
                    self.input.remove(i);
                }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.input.chars().count()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.chars().count(),
            KeyCode::Up => self.history_prev(),
            KeyCode::Down => self.history_next(),
            KeyCode::PageUp => self.scroll_by(-10),
            KeyCode::PageDown => self.scroll_by(10),
            KeyCode::Char(ch) if !ctrl => self.insert_char(ch),
            _ => {}
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
                if let Some(ChatBlock::Tool { result, .. }) = self
                    .blocks
                    .iter_mut()
                    .rev()
                    .find(|b| matches!(b, ChatBlock::Tool { result: None, .. }))
                {
                    *result = Some((content, is_error));
                }
                // may have mutated an already-cached block
                self.invalidate_cache();
            }
            AgentEvent::Permission { preview, reply, .. } => {
                self.attach_tool_detail(preview);
                self.mode = Mode::Perm(reply);
                self.invalidate_cache();
            }
            AgentEvent::Diff(t) => {
                self.attach_tool_detail(t);
                self.invalidate_cache();
            }
            AgentEvent::Info(t) => self.blocks.push(ChatBlock::Info(t)),
            AgentEvent::Error(t) => self.blocks.push(ChatBlock::Error(t)),
            AgentEvent::ModelChanged(m) => self.model = m,
            AgentEvent::TurnStart => {
                if !matches!(self.mode, Mode::Perm(_)) {
                    self.mode = Mode::Busy;
                }
                if self.turn_start.is_none() {
                    self.turn_start = Some(std::time::Instant::now());
                }
            }
            AgentEvent::Usage(u) => {
                self.ctx_tokens = u.prompt_tokens;
                self.out_tokens += u.completion_tokens;
            }
            AgentEvent::TurnEnd => {
                if !matches!(self.mode, Mode::Perm(_)) {
                    self.mode = Mode::Input;
                }
                self.turn_start = None;
            }
        }
    }

    // ---- actions ----

    /// Puts a command/diff preview inside its tool block so the transcript
    /// reads header -> command -> result in order.
    fn attach_tool_detail(&mut self, text: String) {
        if let Some(ChatBlock::Tool { detail, .. }) = self
            .blocks
            .iter_mut()
            .rev()
            .find(|b| matches!(b, ChatBlock::Tool { result: None, .. }))
        {
            *detail = Some(text);
        } else {
            self.blocks.push(ChatBlock::Diff(text));
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
        self.blocks.push(ChatBlock::User(text.clone()));
        self.scroll = None;
        if matches!(self.mode, Mode::Input) {
            self.set_busy();
        } else {
            self.blocks.push(ChatBlock::Info("(queued)".into()));
        }
        let _ = self.cmd_tx.send(AgentCmd::UserInput(text));
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
            "quit" | "exit" | "q" => self.quit = true,
            "clear" => {
                self.blocks.clear();
                self.invalidate_cache();
                self.scroll = None;
                self.ctx_tokens = 0;
                self.set_busy();
                let _ = self.cmd_tx.send(AgentCmd::Clear);
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
                        Ok(_) => self.blocks.push(ChatBlock::Info("project memory cleared".into())),
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
            "status" => {
                let up = self.session_start.elapsed().as_secs();
                self.blocks.push(ChatBlock::Info(format!(
                    "model:    {}\nserver:   {}\ncwd:      {}\ncontext:  {} tokens (last request)\noutput:   {} tokens (session)\nuptime:   {}m {}s",
                    self.model,
                    self.base_url,
                    std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default(),
                    self.ctx_tokens,
                    self.out_tokens,
                    up / 60,
                    up % 60,
                )));
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
                if arg.is_empty() {
                    self.blocks
                        .push(ChatBlock::Info(format!("current model: {}", self.model)));
                } else {
                    self.set_busy();
                    let _ = self.cmd_tx.send(AgentCmd::SetModel(arg.to_string()));
                }
            }
            _ => self
                .blocks
                .push(ChatBlock::Error(format!("unknown command: /{name}"))),
        }
    }

    // ---- input editing ----

    fn byte_idx(&self, char_idx: usize) -> usize {
        self.input
            .char_indices()
            .nth(char_idx)
            .map(|(i, _)| i)
            .unwrap_or(self.input.len())
    }

    fn insert_char(&mut self, ch: char) {
        let i = self.byte_idx(self.cursor);
        self.input.insert(i, ch);
        self.cursor += 1;
    }

    fn insert_str(&mut self, s: &str) {
        let i = self.byte_idx(self.cursor);
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
        self.scroll = if new >= self.max_scroll { None } else { Some(new) };
    }

    // ---- drawing ----

    fn draw(&mut self, f: &mut Frame) {
        let [header_a, chat_a, input_a, status_a] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .areas(f.area());

        // header
        let header = Line::from(vec![
            Span::styled(
                " thoth ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(self.model.clone(), Style::default().fg(Color::Cyan)),
            Span::styled(
                format!(" · {}", self.base_url),
                Style::default().fg(Color::DarkGray),
            ),
        ]);
        f.render_widget(Paragraph::new(header), header_a);

        // transcript: cached prefix + freshly rendered last block
        let width = chat_a.width.max(4) as usize;
        self.ensure_cache(width);
        let mut last_lines: Vec<Line> = Vec::new();
        if !self.blocks.is_empty() {
            self.render_block(self.blocks.len() - 1, &mut last_lines, width);
        }
        let sep = usize::from(!self.cache.is_empty() && !last_lines.is_empty());
        let total = self.cache.len() + sep + last_lines.len();
        let vh = chat_a.height as usize;
        self.max_scroll = total.saturating_sub(vh);
        let offset = self
            .scroll
            .map(|s| s.min(self.max_scroll))
            .unwrap_or(self.max_scroll);
        let mut visible: Vec<Line> = Vec::with_capacity(vh.min(total));
        for i in offset..total.min(offset + vh) {
            if i < self.cache.len() {
                visible.push(self.cache[i].clone());
            } else if sep == 1 && i == self.cache.len() {
                visible.push(Line::default());
            } else {
                visible.push(last_lines[i - self.cache.len() - sep].clone());
            }
        }
        f.render_widget(Paragraph::new(Text::from(visible)), chat_a);

        // input box
        let (title, border_style) = match &self.mode {
            Mode::Busy => (
                format!(
                    " {} working {}s - esc to interrupt ",
                    SPINNER[self.spin],
                    self.turn_start.map(|t| t.elapsed().as_secs()).unwrap_or(0)
                ),
                Style::default().fg(Color::Yellow),
            ),
            Mode::Perm(_) => (
                " permission required ".to_string(),
                Style::default().fg(Color::Yellow),
            ),
            Mode::Input => (String::new(), Style::default().fg(Color::DarkGray)),
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(border_style)
            .title(title);
        let inner = block.inner(input_a);
        f.render_widget(block, input_a);

        if matches!(self.mode, Mode::Perm(_)) {
            let key = Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD);
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("(y)", key),
                    Span::raw(" allow once   "),
                    Span::styled("(a)", key),
                    Span::raw(" always allow this tool   "),
                    Span::styled("(n)", key),
                    Span::raw(" deny"),
                ])),
                inner,
            );
        } else {
            let avail = inner.width.saturating_sub(3) as usize;
            let (view, cx) = self.input_window(avail);
            let style = if matches!(self.mode, Mode::Busy) {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
            };
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("> ", Style::default().fg(Color::Cyan)),
                    Span::styled(view, style),
                ])),
                inner,
            );
            if matches!(self.mode, Mode::Input) {
                f.set_cursor_position(Position::new(inner.x + 2 + cx as u16, inner.y));
            }
        }

        // status
        let mut status = format!(
            " enter send | wheel/pgup scroll | ctrl+o {} | /help | ctrl+c quit",
            if self.expanded { "collapse" } else { "expand" }
        );
        if self.scroll.is_some() {
            status.push_str("  [scrolled up - pgdn for latest]");
        }
        let mut right_parts: Vec<String> = Vec::new();
        if self.ctx_tokens > 0 {
            let ctx = match self.num_ctx {
                Some(max) => format!("ctx {}/{}", fmt_k(self.ctx_tokens), fmt_k(max as u64)),
                None => format!("ctx {}", fmt_k(self.ctx_tokens)),
            };
            right_parts.push(format!("{ctx} | out {}", fmt_k(self.out_tokens)));
        }
        if let Some(s) = &self.editor_status {
            right_parts.push(s.clone());
        }
        let right = if right_parts.is_empty() {
            String::new()
        } else {
            format!("{} ", right_parts.join(" | "))
        };
        // the right side (tokens + editor file) wins over the key hints
        let w = status_a.width as usize;
        let (left, pad) = if status.chars().count() + right.chars().count() > w {
            (String::new(), w.saturating_sub(right.chars().count()))
        } else {
            let pad = w - status.chars().count() - right.chars().count();
            (status, pad)
        };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(left, Style::default().fg(Color::DarkGray)),
                Span::raw(" ".repeat(pad)),
                Span::styled(right, Style::default().fg(Color::DarkGray)),
            ])),
            status_a,
        );
    }

    fn input_window(&self, avail: usize) -> (String, usize) {
        if avail == 0 {
            return (String::new(), 0);
        }
        let chars: Vec<char> = self.input.chars().collect();
        let cur = self.cursor.min(chars.len());
        let cw = |c: char| UnicodeWidthChar::width(c).unwrap_or(1);
        // widest suffix ending at the cursor that fits
        let mut start = cur;
        let mut w = 0usize;
        while start > 0 {
            let c = cw(chars[start - 1]);
            if w + c > avail.saturating_sub(1) {
                break;
            }
            w += c;
            start -= 1;
        }
        let cx = w;
        // extend past the cursor to fill the row
        let mut end = cur;
        let mut tw = w;
        while end < chars.len() {
            let c = cw(chars[end]);
            if tw + c > avail {
                break;
            }
            tw += c;
            end += 1;
        }
        (chars[start..end].iter().collect(), cx)
    }

    /// Incrementally caches wrapped lines for finished blocks; only the last
    /// block (the one that changes while streaming) is re-wrapped per frame.
    fn ensure_cache(&mut self, width: usize) {
        if self.cache_width != width {
            self.cache_width = width;
            self.invalidate_cache();
        }
        let stable = self.blocks.len().saturating_sub(1);
        if self.cached_blocks > stable {
            self.invalidate_cache();
        }
        if self.cached_blocks < stable {
            let mut cache = std::mem::take(&mut self.cache);
            for i in self.cached_blocks..stable {
                if !cache.is_empty() {
                    cache.push(Line::default());
                }
                self.render_block(i, &mut cache, width);
            }
            self.cache = cache;
            self.cached_blocks = stable;
        }
    }

    fn invalidate_cache(&mut self) {
        self.cache.clear();
        self.cached_blocks = 0;
    }

    fn render_block(&self, bi: usize, out: &mut Vec<Line<'static>>, width: usize) {
        let dim = Style::default().fg(Color::DarkGray);
        let dim_italic = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC);
        let red = Style::default().fg(Color::Red);
        let is_last = bi + 1 == self.blocks.len();
        {
            match &self.blocks[bi] {
                ChatBlock::User(t) => wrap_into(
                    out,
                    t,
                    width,
                    "> ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                ChatBlock::Assistant(t) => render_markdown(out, t, width),
                ChatBlock::Reasoning(t) => {
                    // while streaming (last block) show a live tail; else collapse
                    if is_last && matches!(self.mode, Mode::Busy) {
                        out.push(Line::from(Span::styled("thinking...", dim_italic)));
                        let tail: Vec<&str> = t.lines().rev().take(3).collect();
                        for l in tail.into_iter().rev() {
                            out.push(Line::from(Span::styled(
                                format!("  {}", clip(&expand_tabs(l), width.saturating_sub(2))),
                                dim_italic,
                            )));
                        }
                    } else {
                        let n = t.lines().count();
                        out.push(Line::from(Span::styled(
                            format!("(thought, {n} {})", if n == 1 { "line" } else { "lines" }),
                            dim_italic,
                        )));
                    }
                }
                ChatBlock::Tool {
                    name,
                    summary,
                    detail,
                    result,
                } => {
                    out.push(Line::from(vec![
                        Span::styled("[", dim),
                        Span::styled(name.clone(), Style::default().fg(Color::Cyan)),
                        Span::styled("] ", dim),
                        Span::styled(summary.clone(), Style::default()),
                    ]));
                    if let Some(d) = detail {
                        render_diff_body(out, d, width);
                    }
                    match result {
                        None => out.push(Line::from(Span::styled("  ...", dim))),
                        Some((content, is_error)) => {
                            let style = if *is_error { red } else { dim };
                            let lines: Vec<&str> = content.lines().collect();
                            let shown = if self.expanded {
                                lines.len()
                            } else {
                                RESULT_PREVIEW_LINES
                            };
                            for l in lines.iter().take(shown) {
                                out.push(Line::from(Span::styled(
                                    format!("  {}", clip(&expand_tabs(l), width.saturating_sub(2))),
                                    style,
                                )));
                            }
                            if lines.len() > shown {
                                out.push(Line::from(Span::styled(
                                    format!(
                                        "  ... +{} lines (ctrl+o to expand)",
                                        lines.len() - shown
                                    ),
                                    dim,
                                )));
                            }
                        }
                    }
                }
                ChatBlock::Diff(t) => render_diff_body(out, t, width),
                ChatBlock::Info(t) => wrap_into(out, t, width, "", dim, dim),
                ChatBlock::Error(t) => wrap_into(out, t, width, "error: ", red, red),
            }
        }
    }
}

fn expand_tabs(s: &str) -> String {
    s.replace('\t', "    ")
}

fn fmt_k(n: u64) -> String {
    if n < 1000 {
        n.to_string()
    } else {
        format!("{:.1}k", n as f64 / 1000.0)
    }
}

/// Renders the body of a diff/permission preview. Lines are colored by their
/// first character: '+' insert, '-' delete, ' ' context, '$' command, other = header.
fn render_diff_body(out: &mut Vec<Line<'static>>, text: &str, width: usize) {
    let dim = Style::default().fg(Color::DarkGray);
    for l in text.lines() {
        let l = expand_tabs(l);
        let style = match l.chars().next() {
            Some('+') => Style::default().fg(Color::Green),
            Some('-') => Style::default().fg(Color::Red),
            Some(' ') | Some('.') => dim,
            Some('$') => Style::default().add_modifier(Modifier::BOLD),
            _ => Style::default().fg(Color::Yellow),
        };
        out.push(Line::from(Span::styled(
            format!("  {}", clip(&l, width.saturating_sub(2))),
            style,
        )));
    }
}

/// Lightweight markdown rendering: code fences, headings, quotes,
/// `inline code` and **bold**.
fn render_markdown(out: &mut Vec<Line<'static>>, text: &str, width: usize) {
    let mut in_fence = false;
    for src in text.lines() {
        let trimmed = src.trim_start();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            out.push(Line::from(Span::styled(
                clip(&expand_tabs(src), width),
                Style::default().fg(Color::DarkGray),
            )));
            continue;
        }
        if in_fence {
            out.push(Line::from(Span::styled(
                format!("  {}", clip(&expand_tabs(src), width.saturating_sub(2))),
                Style::default().fg(Color::LightGreen),
            )));
            continue;
        }
        if src.is_empty() {
            out.push(Line::default());
            continue;
        }
        let base = if trimmed.starts_with('#') {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else if trimmed.starts_with('>') {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default()
        };
        for piece in textwrap::wrap(&expand_tabs(src), width.max(2)) {
            out.push(Line::from(inline_spans(&piece, base)));
        }
    }
}

/// Splits a single line into styled spans for `code` and **bold** markers.
fn inline_spans(s: &str, base: Style) -> Vec<Span<'static>> {
    let code = base.fg(Color::Yellow);
    let bold = base.add_modifier(Modifier::BOLD);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    let flush = |spans: &mut Vec<Span<'static>>, cur: &mut String| {
        if !cur.is_empty() {
            spans.push(Span::styled(std::mem::take(cur), base));
        }
    };
    while let Some(c) = chars.next() {
        if c == '`' {
            let mut inner = String::new();
            let mut closed = false;
            for c2 in chars.by_ref() {
                if c2 == '`' {
                    closed = true;
                    break;
                }
                inner.push(c2);
            }
            if closed && !inner.is_empty() {
                flush(&mut spans, &mut cur);
                spans.push(Span::styled(inner, code));
            } else {
                cur.push('`');
                cur.push_str(&inner);
            }
        } else if c == '*' && chars.peek() == Some(&'*') {
            chars.next();
            let mut inner = String::new();
            let mut closed = false;
            while let Some(c2) = chars.next() {
                if c2 == '*' && chars.peek() == Some(&'*') {
                    chars.next();
                    closed = true;
                    break;
                }
                inner.push(c2);
            }
            if closed && !inner.is_empty() {
                flush(&mut spans, &mut cur);
                spans.push(Span::styled(inner, bold));
            } else {
                cur.push_str("**");
                cur.push_str(&inner);
            }
        } else {
            cur.push(c);
        }
    }
    flush(&mut spans, &mut cur);
    if spans.is_empty() {
        spans.push(Span::styled(String::new(), base));
    }
    spans
}

fn clip(s: &str, max: usize) -> String {
    if UnicodeWidthStr::width(s) <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(1);
        if w + cw > max.saturating_sub(1) {
            break;
        }
        w += cw;
        out.push(c);
    }
    out.push('…');
    out
}

fn wrap_into(
    out: &mut Vec<Line<'static>>,
    text: &str,
    width: usize,
    prefix: &str,
    prefix_style: Style,
    style: Style,
) {
    let pw = UnicodeWidthStr::width(prefix);
    let w = width.saturating_sub(pw).max(2);
    let mut first = true;
    for src in text.lines() {
        if src.is_empty() {
            out.push(Line::default());
            continue;
        }
        for piece in textwrap::wrap(src, w) {
            let mut spans = Vec::new();
            if first {
                spans.push(Span::styled(prefix.to_string(), prefix_style));
                first = false;
            } else if pw > 0 {
                spans.push(Span::raw(" ".repeat(pw)));
            }
            spans.push(Span::styled(piece.into_owned(), style));
            out.push(Line::from(spans));
        }
    }
    if first {
        out.push(Line::from(Span::styled(prefix.to_string(), prefix_style)));
    }
}
