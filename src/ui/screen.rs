//! Drawing. Everything that turns the app's state into rows on the screen
//! lives here; `mod.rs` keeps the state and what changes it. Both halves are
//! `impl App`, and a child module sees its parent's private fields, so the
//! split costs no visibility.

use super::{App, ChatBlock, Mode, RESULT_PREVIEW_LINES};
use crate::ui::input::cwd;
use crate::ui::render::{
    clip, fmt_elapsed, fmt_k, fmt_usd, home_relative, printable, render_diff_body, render_markdown,
    short_url, wrap_into,
};
use crate::ui::theme;
use crate::ui::theme::{PROMPT, RESULT, SPINNER, TOOL};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Position};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Paragraph};
use unicode_width::UnicodeWidthChar;

/// Rows the input may grow to before it scrolls instead. A message longer
/// than this is being written somewhere else and pasted in.
const INPUT_MAX_ROWS: u16 = 10;

impl App {
    // ---- drawing ----

    pub(super) fn draw(&mut self, f: &mut Frame) {
        if let Some(screen) = &self.config {
            screen.draw(f);
            return;
        }
        // one line of chrome at the top, one status line above the input and
        // one hint line below it: everything else belongs to the transcript.
        //
        // the input is as tall as the message written in it, up to a third of
        // the screen: past that the transcript it is a reply to matters more
        // split, not lines(): a message ending in a break has an empty last
        // line, and that is the line the cursor is sitting on
        let input_h = (self.input.split('\n').count().max(1) as u16)
            .min((f.area().height / 3).max(1))
            .min(INPUT_MAX_ROWS);
        // whatever it wants, it cannot have the rest of the screen: the fixed
        // rows and the input come first, and at least one row of transcript
        // stays, or the question is offering choices to someone who can no
        // longer see what was asked
        let room = f.area().height.saturating_sub(6 + input_h).max(1);
        // two sets of rows that offer something to take, zero of them most of
        // the time. They sit on opposite sides of the input because they mean
        // opposite things: the model's question is asked before anything is
        // typed, and `@path` completes what is being typed right now. Never
        // both at once, because a question owns every key the picker wants
        let ask_h = match &self.mode {
            Mode::Choice { options, .. } => (Mode::choice_rows(options) as u16).min(room),
            _ => 0,
        };
        let pick_h = self
            .picker
            .as_ref()
            .map(|p| p.height().min(room.saturating_sub(ask_h).max(1)))
            .unwrap_or(0);
        // and with the height known, the highlighted row is brought into it
        if let Mode::Choice { sel, top, .. } = &mut self.mode {
            let rows = ask_h as usize;
            *top = (*top).min(*sel).max((*sel + 1).saturating_sub(rows));
        }
        let [header_a, chat_a, state_a, ask_a, box_a, pick_a, status_a] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(ask_h),
            Constraint::Length(input_h + 2),
            Constraint::Length(pick_h),
            Constraint::Length(1),
        ])
        .areas(f.area());

        // header: name and version as one chip on the left, the server we are
        // talking to on the right. the tagline is the first thing to go when
        // the two sides do not both fit
        let chip = format!(" thoth v{} ", env!("CARGO_PKG_VERSION"));
        let tag = "  agentic coding";
        // the server names itself, and a name is a place to hide an escape
        // sequence in something drawn on every single frame
        let url = printable(&short_url(&self.base_url));
        let model = printable(&self.model);
        let hw = header_a.width as usize;
        // the profile name rides with the model and server: they are what it
        // decides, and they move together when it is switched
        let prof = match &self.profile {
            Some(p) => format!("{}  ", printable(p)),
            None => String::new(),
        };
        let right_len = prof.chars().count() + model.chars().count() + 2 + url.chars().count();
        let mut header = vec![Span::styled(
            chip.clone(),
            theme::accent().add_modifier(Modifier::REVERSED),
        )];
        let mut left_len = chip.chars().count();
        if left_len + tag.chars().count() + right_len + 2 <= hw {
            header.push(Span::styled(tag, theme::muted()));
            left_len += tag.chars().count();
        }
        header.push(Span::raw(
            " ".repeat(hw.saturating_sub(left_len + right_len)),
        ));
        header.push(Span::styled(prof, theme::muted_italic()));
        header.push(Span::styled(model, theme::accent()));
        header.push(Span::styled(format!("  {url}"), theme::muted()));
        f.render_widget(Paragraph::new(Line::from(header)), header_a);

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

        // state line: what thoth is doing right now
        let state = match &self.mode {
            Mode::Busy => Line::from(vec![
                Span::styled(SPINNER[self.spin], Style::default().fg(theme::BUSY)),
                Span::styled(
                    format!(
                        " working  {}  esc to interrupt",
                        fmt_elapsed(self.turn_start.map(|t| t.elapsed().as_secs()).unwrap_or(0))
                    ),
                    theme::muted(),
                ),
            ]),
            Mode::Perm(_) => Line::from(vec![
                Span::styled("permission needed  ", Style::default().fg(theme::BUSY)),
                Span::styled("y", theme::key()),
                Span::styled(" once   ", theme::muted()),
                Span::styled("a", theme::key()),
                Span::styled(" always   ", theme::muted()),
                Span::styled("s", theme::key()),
                Span::styled(" skip   ", theme::muted()),
                Span::styled("n", theme::key()),
                Span::styled(" no, stop", theme::muted()),
            ]),
            Mode::Choice {
                typing: Some(_), ..
            } => Line::from(vec![
                Span::styled("your call  ", Style::default().fg(theme::BUSY)),
                Span::styled("type your answer   ", theme::muted()),
                Span::styled("enter", theme::key()),
                Span::styled(" send it   ", theme::muted()),
                Span::styled("esc", theme::key()),
                Span::styled(" back to the list", theme::muted()),
            ]),
            Mode::Choice { .. } => Line::from(vec![
                Span::styled("your call  ", Style::default().fg(theme::BUSY)),
                Span::styled("up/down", theme::key()),
                Span::styled(" move   ", theme::muted()),
                Span::styled("enter", theme::key()),
                Span::styled(" take   ", theme::muted()),
                Span::styled("1-9", theme::key()),
                Span::styled(" pick   ", theme::muted()),
                Span::styled("esc", theme::key()),
                Span::styled(" let it decide", theme::muted()),
            ]),
            // the picker takes over some keys, so say which ones while it is up
            Mode::Input if self.picker.is_some() => Line::from(vec![
                Span::styled("up/down", theme::key()),
                Span::styled(" pick   ", theme::muted()),
                Span::styled("tab", theme::key()),
                Span::styled(" take   ", theme::muted()),
                Span::styled("esc", theme::key()),
                Span::styled(" close", theme::muted()),
            ]),
            Mode::PlanChoice => Line::from(vec![
                Span::styled("plan ready  ", Style::default().fg(theme::BUSY)),
                Span::styled("y", theme::key()),
                Span::styled(" do it   ", theme::muted()),
                Span::styled("a", theme::key()),
                Span::styled(" do it, ask each time   ", theme::muted()),
                Span::styled("n", theme::key()),
                Span::styled(" keep planning", theme::muted()),
            ]),
            Mode::Input => Line::default(),
        };
        f.render_widget(Paragraph::new(state), state_a);

        // the input sits in a box of its own. It is the one part of the
        // screen the user writes into rather than reads, and a line drawn
        // round it says so at a glance; the border also carries whether thoth
        // is listening, which saves saying it in words anywhere else
        let listening = matches!(self.mode, Mode::Input);
        f.render_widget(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(if listening {
                    theme::accent()
                } else {
                    theme::muted()
                }),
            box_a,
        );
        // a column of air inside the border on each side: the prompt mark
        // touching the box reads as part of it
        let input_a = box_a.inner(Margin::new(2, 1));

        // input: one row per typed line, scrolled so the row the cursor is
        // on is always among them
        let avail = input_a.width.saturating_sub(3) as usize;
        let style = if listening {
            Style::default()
        } else {
            theme::muted()
        };
        let (rows, cur_row, cx) = self.input_rows(avail, input_a.height as usize);
        let lines: Vec<Line> = rows
            .iter()
            .enumerate()
            .map(|(i, text)| {
                // only the first row of the message carries the prompt mark,
                // the rest line up under it
                let mark = if cur_row.1 == 0 && i == 0 {
                    PROMPT
                } else {
                    "  "
                };
                Line::from(vec![
                    Span::styled(mark, theme::accent()),
                    Span::styled(text.clone(), style),
                ])
            })
            .collect();
        f.render_widget(Paragraph::new(Text::from(lines)), input_a);
        if matches!(self.mode, Mode::Input) {
            f.set_cursor_position(Position::new(
                input_a.x + 2 + cx as u16,
                input_a.y + cur_row.0 as u16,
            ));
        }

        // the model's question: one row per option, then the row that opens
        // the input box for an answer nobody offered
        if let Mode::Choice {
            options,
            sel,
            top,
            typing,
            ..
        } = &self.mode
        {
            let width = ask_a.width.saturating_sub(7) as usize;
            let write_row = options.len();
            let shown = (*top + ask_h as usize).min(write_row + 1);
            let lines: Vec<Line> = (*top..shown)
                .map(|i| {
                    let text = if i == write_row {
                        match typing {
                            Some(_) => "something else, type it below".to_string(),
                            None => "something else, type your own answer".to_string(),
                        }
                    } else {
                        printable(&options[i])
                    };
                    // more rows below than fit: the last one on screen says
                    // so, in its own span, or on the highlighted row the
                    // marker would read as part of the option
                    let hidden = (write_row + 1).saturating_sub(shown);
                    let (text, more) = if hidden > 0 && i + 1 == shown {
                        (
                            clip(&text, width.saturating_sub(10)),
                            format!("  +{hidden} more"),
                        )
                    } else {
                        (text, String::new())
                    };
                    let style = if i == *sel {
                        theme::accent().add_modifier(Modifier::REVERSED)
                    } else if i == write_row {
                        theme::muted()
                    } else {
                        theme::accent()
                    };
                    Line::from(vec![
                        Span::styled(
                            if i == *sel { " > " } else { "   " },
                            Style::default().fg(theme::BUSY),
                        ),
                        Span::styled(format!("{}  ", i + 1), theme::muted()),
                        Span::styled(clip(&text, width), style),
                        Span::styled(more, theme::muted()),
                    ])
                })
                .collect();
            f.render_widget(Paragraph::new(Text::from(lines)), ask_a);
        }

        // @path picker: one row per candidate, the highlighted one reversed
        if let Some(p) = &self.picker {
            let rows = pick_h as usize;
            let last = p.top + rows;
            let width = pick_a.width.saturating_sub(2) as usize;
            let shown = &p.items[p.top..last.min(p.items.len())];
            // a command list gets a column: the names are short and what each
            // one does is the part being read, so it has to start in the same
            // place on every row. Paths carry no note and need no column
            let col = if p.notes.is_empty() {
                0
            } else {
                shown.iter().map(|i| i.chars().count()).max().unwrap_or(0)
            };
            let lines: Vec<Line> = shown
                .iter()
                .enumerate()
                .map(|(i, item)| {
                    let idx = p.top + i;
                    let more = p.items.len().saturating_sub(last);
                    let style = if idx == p.sel {
                        theme::accent().add_modifier(Modifier::REVERSED)
                    } else if item.ends_with('/') {
                        theme::accent()
                    } else {
                        theme::muted()
                    };
                    // a command says what it does; a path is its own answer.
                    // the bottom row also carries the "there is more" marker,
                    // at the end so it cannot be read as part of either
                    let mut note = p.notes.get(idx).cloned().unwrap_or_default();
                    if more > 0 && idx + 1 == last {
                        note.push_str(&format!("  +{more} more"));
                    }
                    let room = width.saturating_sub(note.chars().count() + 2);
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled(format!("{:<col$}", clip(item, room.max(8))), style),
                        Span::styled(
                            if note.is_empty() {
                                String::new()
                            } else {
                                format!("  {note}")
                            },
                            theme::muted(),
                        ),
                    ])
                })
                .collect();
            f.render_widget(Paragraph::new(Text::from(lines)), pick_a);
        }

        // status: hints on the left, live numbers and editor file on the right.
        // Two of the three things the left can say are not hints at all but
        // the answer to "why has this stopped behaving": they are worth the
        // row even when the numbers are not
        let (left_text, state) = if !self.mouse {
            (
                "selecting: drag and copy as usual  ·  ctrl+t gives the wheel back".to_string(),
                true,
            )
        } else if self.scroll.is_some() {
            ("scrolled up  ·  pgdn for the latest".to_string(), true)
        } else {
            (
                format!(
                    "/help  ·  shift+tab {}  ·  ctrl+o {}",
                    self.perm_mode.name(),
                    if self.expanded { "collapse" } else { "expand" }
                ),
                false,
            )
        };
        let mut right_parts: Vec<String> = Vec::new();
        if self.ctx_tokens > 0 {
            right_parts.push(match self.window {
                Some(max) => format!(
                    "ctx {}/{} ({}%)",
                    fmt_k(self.ctx_tokens),
                    fmt_k(max as u64),
                    (self.ctx_tokens * 100 / max.max(1) as u64).min(999)
                ),
                None => format!("ctx {}", fmt_k(self.ctx_tokens)),
            });
            right_parts.push(format!("out {}", fmt_k(self.out_tokens)));
        }
        if let Some(s) = self.spent {
            right_parts.push(fmt_usd(s));
        }
        if let Some(s) = &self.editor_status {
            // written by the editor extension into a file, so it arrives the
            // same way any other file's contents do
            right_parts.push(printable(s));
        }
        // the numbers win over the key hints, and lose to a state: a user who
        // cannot see they are scrolled up watches an empty screen and waits
        let w = status_a.width as usize;
        while state
            && !right_parts.is_empty()
            && left_text.chars().count() + right_parts.join("  ·  ").chars().count() + 2 > w
        {
            right_parts.pop();
        }
        let right = right_parts.join("  ·  ");
        let (left, pad) = if left_text.chars().count() + right.chars().count() + 2 > w {
            (String::new(), w.saturating_sub(right.chars().count()))
        } else {
            (
                left_text.clone(),
                w - left_text.chars().count() - right.chars().count(),
            )
        };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(left, theme::muted()),
                Span::raw(" ".repeat(pad)),
                Span::styled(right, theme::muted()),
            ])),
            status_a,
        );
    }

    /// The rows to draw, which row of them the cursor is on, and its column.
    /// The second half of `cur_row` is the line the top row is showing, so
    /// the caller knows whether it is looking at the start of the message.
    fn input_rows(&self, avail: usize, height: usize) -> (Vec<String>, (usize, usize), usize) {
        let lines: Vec<&str> = if self.input.is_empty() {
            vec![""]
        } else {
            self.input.split('\n').collect()
        };
        // which line the cursor is on, and where in it
        let mut seen = 0usize;
        let mut cur_line = 0usize;
        let mut col = 0usize;
        for (i, l) in lines.iter().enumerate() {
            let len = l.chars().count();
            if self.cursor <= seen + len {
                cur_line = i;
                col = self.cursor - seen;
                break;
            }
            seen += len + 1; // the newline itself
            cur_line = i;
            col = len;
        }
        // scroll so the cursor's line is the last one visible, never past
        // the top of the message
        let height = height.max(1);
        let top = (cur_line + 1).saturating_sub(height).min(cur_line);
        let mut cx = 0usize;
        let rows: Vec<String> = lines[top..(top + height).min(lines.len())]
            .iter()
            .enumerate()
            .map(|(i, l)| {
                let (text, x) =
                    Self::window_of_line(l, if top + i == cur_line { col } else { 0 }, avail);
                if top + i == cur_line {
                    cx = x;
                }
                text
            })
            .collect();
        (rows, (cur_line - top, top), cx)
    }

    /// The stretch of one line that fits `avail` columns with the cursor in
    /// it, and the cursor's column within that stretch.
    fn window_of_line(line: &str, cursor: usize, avail: usize) -> (String, usize) {
        if avail == 0 {
            return (String::new(), 0);
        }
        let chars: Vec<char> = line.chars().collect();
        let cur = cursor.min(chars.len());
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
    pub(super) fn ensure_cache(&mut self, width: usize) {
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

    pub(super) fn invalidate_cache(&mut self) {
        self.cache.clear();
        self.cached_blocks = 0;
    }

    /// Block `bi` changed. The last block is never cached, so mutating it
    /// costs nothing; anything older forces a re-wrap.
    pub(super) fn invalidate_from(&mut self, bi: usize) {
        if bi + 1 < self.blocks.len() {
            self.invalidate_cache();
        }
    }

    /// Startup screen: the logo, then where we are and what to type. The name
    /// and version live in the header, so nothing here repeats them. On
    /// terminals too narrow for the art the logo is dropped rather than
    /// wrapped to rubble.
    fn render_banner(&self, out: &mut Vec<Line<'static>>, width: usize) {
        if width >= theme::LOGO_WIDTH + 2 {
            for (row, art) in theme::LOGO.iter().enumerate() {
                out.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        *art,
                        Style::default()
                            .fg(theme::LOGO_RAMP[row])
                            .add_modifier(Modifier::BOLD),
                    ),
                ]));
            }
            out.push(Line::default());
        }
        out.push(Line::from(Span::styled(
            format!(
                "  {}",
                clip(&home_relative(&cwd()), width.saturating_sub(2))
            ),
            theme::muted_italic(),
        )));
        // which api, and the window it is measured against when one is known
        out.push(Line::from(Span::styled(
            match self.window {
                Some(n) => format!("  {} api  ·  {} context window", self.api, fmt_k(n as u64)),
                None => format!("  {} api", self.api),
            },
            theme::muted(),
        )));
        out.push(Line::default());
        // three things nobody finds by typing a sentence at it, on the one
        // screen that has room to say them
        for (key, what) in [
            (
                "/",
                "commands, with a list as you type. /help for all of it",
            ),
            ("@", "attach a file to the message"),
            ("shift+tab", "how much thoth asks before it acts"),
        ] {
            out.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{key:<9}  "), theme::key()),
                Span::styled(clip(what, width.saturating_sub(13)), theme::muted()),
            ]));
        }
    }

    fn render_block(&self, bi: usize, out: &mut Vec<Line<'static>>, width: usize) {
        let dim = theme::muted();
        let dim_italic = theme::muted_italic();
        let red = theme::danger();
        let is_last = bi + 1 == self.blocks.len();
        match &self.blocks[bi] {
            ChatBlock::Banner => self.render_banner(out, width),
            ChatBlock::User(t) => wrap_into(out, t, width, PROMPT, theme::accent(), theme::bold()),
            ChatBlock::Assistant(t) => render_markdown(out, t, width),
            ChatBlock::Reasoning(t) => {
                // while streaming (last block) show a live tail; else collapse
                if is_last && matches!(self.mode, Mode::Busy) {
                    out.push(Line::from(Span::styled("thinking", dim_italic)));
                    let tail: Vec<&str> = t.lines().rev().take(3).collect();
                    for l in tail.into_iter().rev() {
                        out.push(Line::from(Span::styled(
                            format!("  {}", clip(&printable(l), width.saturating_sub(2))),
                            dim_italic,
                        )));
                    }
                } else {
                    let n = t.lines().count();
                    out.push(Line::from(Span::styled(
                        format!("thought for {n} {}", if n == 1 { "line" } else { "lines" }),
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
                // one dot per call, coloured by how the call went, so a
                // screenful reads down the left edge without any of the words
                let waiting = is_last && matches!(self.mode, Mode::Perm(_));
                let dot = match result {
                    Some((_, true)) => theme::danger(),
                    Some(_) => Style::default().fg(theme::ADDED),
                    None => Style::default().fg(theme::BUSY),
                };
                let head = printable(summary);
                let room = width.saturating_sub(name.len() + 4);
                let shown = clip(&head, room);
                out.push(Line::from(vec![
                    Span::styled(format!("{TOOL} "), dot),
                    Span::styled(name.clone(), theme::accent().add_modifier(Modifier::BOLD)),
                    Span::raw("  "),
                    Span::styled(shown.clone(), dim),
                ]));
                if let Some(d) = detail {
                    // a shell call's preview is the command, which is already
                    // on the line above. Printing it twice is noise, but only
                    // dropping it when the line above really is the whole of
                    // it: a clipped summary is not the command line the user
                    // was promised they would see
                    let echo = shown == head && *d == format!("$ {head}");
                    if !echo {
                        render_diff_body(out, d, width);
                    }
                }
                // what came back hangs off the call on a corner, and the rest
                // of it lines up past that corner: a result and the next line
                // of prose are otherwise both just indented text
                let tie = |out: &mut Vec<Line<'static>>, first: bool, text: String, s: Style| {
                    out.push(Line::from(vec![
                        Span::styled(
                            if first {
                                format!("  {RESULT} ")
                            } else {
                                "    ".into()
                            },
                            dim,
                        ),
                        Span::styled(clip(&printable(&text), width.saturating_sub(4)), s),
                    ]));
                };
                match result {
                    // a call with a permission prompt open has not run and
                    // may never run: this is the block being asked about, and
                    // saying "running" under it is thoth claiming to have
                    // done the thing it is standing there asking permission for
                    None if waiting => tie(
                        out,
                        true,
                        "waiting for your answer".into(),
                        Style::default().fg(theme::BUSY),
                    ),
                    None => tie(out, true, "running".into(), dim),
                    Some((content, is_error)) => {
                        let style = if *is_error { red } else { dim };
                        let lines: Vec<&str> = content.lines().collect();
                        let shown = if self.expanded {
                            lines.len()
                        } else {
                            RESULT_PREVIEW_LINES
                        };
                        for (i, l) in lines.iter().take(shown).enumerate() {
                            tie(out, i == 0, (*l).to_string(), style);
                        }
                        if lines.len() > shown {
                            tie(
                                out,
                                lines.is_empty(),
                                format!("+{} more lines  ctrl+o", lines.len() - shown),
                                dim_italic,
                            );
                        }
                    }
                }
            }
            ChatBlock::Diff(t) => render_diff_body(out, t, width),
            ChatBlock::Info(t) => wrap_into(out, t, width, "  ", dim, dim),
            ChatBlock::Error(t) => wrap_into(out, t, width, "error  ", red, red),
        }
    }
}
