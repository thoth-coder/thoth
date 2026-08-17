//! What a key means.
//!
//! `mod.rs` holds the state and the agent's side of it, `screen.rs` turns that
//! state into rows, and this is the third side: a keypress or a mouse event
//! arriving from the terminal, and what it changes. The three are all `impl
//! App`, and a child module reads its parent's private fields, so the split
//! costs no visibility.
//!
//! The order the modes are tested in is the whole of the routing. A question
//! from the model owns the keys before the permission prompt does, the
//! permission prompt before the plan chooser, and the `@path` picker takes
//! only the keys it needs before the line editing underneath sees the rest.

use super::*;

impl App {
    // ---- events ----

    pub(super) fn on_term_event(&mut self, ev: Event) {
        match ev {
            Event::Key(k) if k.kind != KeyEventKind::Release => self.on_key(k),
            Event::Mouse(m) => match m.kind {
                MouseEventKind::ScrollUp => self.scroll_by(-3),
                MouseEventKind::ScrollDown => self.scroll_by(3),
                _ => {}
            },
            // a paste belongs in the input box, and while an answer is being
            // written that is exactly where it should land
            Event::Paste(s) if !matches!(self.mode, Mode::Perm(_)) && !self.picking() => {
                // pasted code keeps its lines now that the input has them.
                // A lone \r is a line break too, and leaving it in would
                // draw the rest of the line over the start of it. Everything
                // else a clipboard can carry (escape sequences from a
                // terminal scrollback, most of all) comes out here, so the
                // buffer holds only what the user can see, and the model is
                // sent only what the user meant
                let s = printable(&s.replace("\r\n", "\n").replace('\r', "\n"));
                self.insert_str(&s);
                self.picker_off = false;
                self.refresh_picker();
            }
            _ => {}
        }
    }

    pub(super) fn on_key(&mut self, k: KeyEvent) {
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

        // a question from the model owns the keys until it has an answer,
        // except while the user is writing one of their own: there the input
        // box is doing its ordinary job and only enter and esc mean something
        // different, so everything else falls through to the editing keys
        let asking = match &self.mode {
            Mode::Choice {
                options,
                sel,
                typing,
                ..
            } => Some((Mode::choice_rows(options), *sel, typing.is_some())),
            _ => None,
        };
        if let Some((rows, sel, writing)) = asking {
            let step = |d: isize| (sel as isize + d).rem_euclid(rows as isize) as usize;
            if writing {
                match k.code {
                    KeyCode::Esc => {
                        self.stop_writing_answer();
                        return;
                    }
                    KeyCode::Enter if !k.modifiers.contains(KeyModifiers::SHIFT) => {
                        let said = self.input.trim().to_string();
                        if !said.is_empty() {
                            self.input.clear();
                            self.cursor = 0;
                            self.answer_choice(Some(Answer::Wrote(said)));
                        }
                        return;
                    }
                    // and everything else is ordinary typing
                    _ => {}
                }
            } else {
                match k.code {
                    KeyCode::Up => self.move_choice(step(-1)),
                    KeyCode::Down => self.move_choice(step(1)),
                    KeyCode::Char('p') if ctrl => self.move_choice(step(-1)),
                    KeyCode::Char('n') if ctrl => self.move_choice(step(1)),
                    KeyCode::Char('c') if ctrl => self.quit = true,
                    // the numbers still work, and only for a row there is one
                    // of: a 7 against three options is a typo, not an answer
                    KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
                        let i = c as usize - '1' as usize;
                        if i < rows {
                            self.take_choice(i);
                        }
                    }
                    KeyCode::Enter => self.take_choice(sel),
                    KeyCode::Esc => self.answer_choice(None),
                    _ => {}
                }
                return;
            }
        }

        if matches!(self.mode, Mode::Perm(_)) {
            let reply = match k.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => Some(PermReply::Yes),
                KeyCode::Char('a') | KeyCode::Char('A') => Some(PermReply::Always),
                KeyCode::Char('s') | KeyCode::Char('S') => Some(PermReply::Skip),
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
                let said = match r {
                    PermReply::Yes | PermReply::Always => "allowed",
                    PermReply::Skip => "skipped, carrying on without it",
                    PermReply::No => "denied",
                };
                let _ = tx.send(r);
                self.blocks.push(ChatBlock::Info(said.into()));
            }
            return;
        }

        // the plan is on screen and the only question left is whether to
        // carry it out, so the keys mean that until it is answered
        if matches!(self.mode, Mode::PlanChoice) {
            let chosen = match k.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => Some(Some(PermMode::AcceptEdits)),
                KeyCode::Char('a') | KeyCode::Char('A') => Some(Some(PermMode::Manual)),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(None),
                KeyCode::Char('c') if ctrl => {
                    self.quit = true;
                    None
                }
                _ => None,
            };
            if let Some(choice) = chosen {
                self.mode = Mode::Input;
                match choice {
                    Some(m) => {
                        self.set_mode(m);
                        self.send_input("Carry out the plan you just described.".into());
                    }
                    None => self
                        .blocks
                        .push(ChatBlock::Info("still planning. say what to change".into())),
                }
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
            KeyCode::BackTab => self.set_mode(self.perm_mode.next()),
            KeyCode::Char('o') if ctrl => {
                self.expanded = !self.expanded;
                self.invalidate_cache();
            }
            KeyCode::Char('t') if ctrl => self.toggle_mouse(),
            KeyCode::Char('y') if ctrl => self.copy_out(false),
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
            // but not into the input history while an answer is being
            // written: the history belongs to the conversation, and pulling
            // an old message over a half-written answer loses it
            KeyCode::Up if !self.answering() => self.history_prev(),
            KeyCode::Down if !self.answering() => self.history_next(),
            KeyCode::PageUp => self.scroll_by(-10),
            KeyCode::PageDown => self.scroll_by(10),
            KeyCode::Char(ch) if !ctrl => self.insert_char(ch),
            _ => {}
        }
        self.refresh_picker();
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

    pub(super) fn scroll_by(&mut self, delta: i32) {
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
