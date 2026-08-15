//! The config screen: profiles on the left, the selected profile's settings
//! on the right. `thoth config` runs it on its own and `/config` opens it
//! over a session, so both go through the same struct and cannot drift.

use crate::config::{Config, Profile, Store, load_store, resolve, save_store};
use crate::ui::render::clip;
use crate::ui::theme;
use anyhow::Result;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;

/// What the host should do after handing a key to the screen.
pub enum ConfigAction {
    Stay,
    Close,
    /// Saved: switch the running session to this profile.
    Apply(String, Box<Config>),
}

#[derive(Clone, Copy, PartialEq)]
enum Field {
    Api,
    BaseUrl,
    Model,
    ApiKey,
    Headers,
    ContextWindow,
    MaxTokens,
    Think,
    Temperature,
    MaxTurns,
    PriceIn,
    PriceOut,
    PriceCached,
}

use Field::*;
const FIELDS: [Field; 13] = [
    Api,
    BaseUrl,
    Model,
    ApiKey,
    Headers,
    ContextWindow,
    MaxTokens,
    Think,
    Temperature,
    MaxTurns,
    PriceIn,
    PriceOut,
    PriceCached,
];

impl Field {
    fn label(self) -> &'static str {
        match self {
            Api => "api",
            BaseUrl => "base url",
            Model => "model",
            ApiKey => "api key",
            Headers => "headers",
            ContextWindow => "context window",
            MaxTokens => "max tokens",
            Think => "think",
            Temperature => "temperature",
            MaxTurns => "max turns",
            PriceIn => "$ / M in",
            PriceOut => "$ / M out",
            PriceCached => "$ / M cached",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Api => "wire protocol. auto reads the url and asks a local server",
            BaseUrl => "endpoint, e.g. http://localhost:11434/v1 or https://api.anthropic.com/v1",
            Model => "empty asks the server, which only works for a local one",
            ApiKey => "sent as a bearer token, or x-api-key on anthropic",
            Headers => "extra request headers. edit them in the config file",
            ContextWindow => "requested per call on ollama; elsewhere it is what auto-compact uses",
            MaxTokens => "cap on one reply. anthropic needs one, others default",
            Think => "force thinking on or off. off helps small windows",
            Temperature => "empty = whatever the server defaults to",
            MaxTurns => "tool calls allowed in one turn before thoth stops",
            PriceIn => "usd per million input tokens, for the running cost",
            PriceOut => "usd per million output tokens",
            PriceCached => "usd per million cached input tokens, when discounted",
        }
    }

    /// Current value, and false when it is a placeholder rather than a real
    /// setting, so the two can be styled apart. Keys come back masked; the
    /// real characters only appear while the field is being edited.
    fn value(self, p: &Profile) -> (String, bool) {
        let or_default = |v: &Option<String>, d: &str| match v {
            Some(s) if !s.is_empty() => (s.clone(), true),
            _ => (format!("({d})"), false),
        };
        let number = |v: Option<String>, d: &str| match v {
            Some(s) => (s, true),
            None => (format!("({d})"), false),
        };
        match self {
            Api => match p.api.unwrap_or_default() {
                crate::config::Api::Auto => ("(auto)".into(), false),
                crate::config::Api::Openai => ("openai".into(), true),
                crate::config::Api::Ollama => ("ollama".into(), true),
                crate::config::Api::Anthropic => ("anthropic".into(), true),
            },
            BaseUrl => or_default(&p.base_url, crate::config::DEFAULT_BASE_URL),
            Model => or_default(&p.model, "ask the server"),
            ApiKey => match &p.api_key {
                Some(k) if !k.is_empty() => ("•".repeat(k.chars().count().min(16)), true),
                _ => ("(not set)".into(), false),
            },
            Headers => match p.headers.len() {
                0 => ("(none)".into(), false),
                n => (format!("{n} set in the config file"), true),
            },
            ContextWindow => number(
                p.context_window.map(|n| n.to_string()),
                &format!(
                    "{} on ollama, unknown elsewhere",
                    crate::client::DEFAULT_NUM_CTX
                ),
            ),
            MaxTokens => number(p.max_tokens.map(|n| n.to_string()), "server default"),
            Think => match p.think {
                Some(true) => ("on".into(), true),
                Some(false) => ("off".into(), true),
                None => ("(model default)".into(), false),
            },
            Temperature => number(p.temperature.map(|t| t.to_string()), "server default"),
            MaxTurns => number(
                p.max_turns.map(|n| n.to_string()),
                &crate::config::DEFAULT_MAX_TURNS.to_string(),
            ),
            PriceIn => number(p.price_in.map(|v| v.to_string()), "no cost shown"),
            PriceOut => number(p.price_out.map(|v| v.to_string()), "no cost shown"),
            PriceCached => number(p.price_cached.map(|v| v.to_string()), "same as input"),
        }
    }

    /// Text to put in the editor when the field is opened.
    fn raw(self, p: &Profile) -> String {
        match self {
            BaseUrl => p.base_url.clone().unwrap_or_default(),
            Model => p.model.clone().unwrap_or_default(),
            ApiKey => p.api_key.clone().unwrap_or_default(),
            ContextWindow => p.context_window.map(|n| n.to_string()).unwrap_or_default(),
            MaxTokens => p.max_tokens.map(|n| n.to_string()).unwrap_or_default(),
            Temperature => p.temperature.map(|t| t.to_string()).unwrap_or_default(),
            MaxTurns => p.max_turns.map(|n| n.to_string()).unwrap_or_default(),
            PriceIn => p.price_in.map(|v| v.to_string()).unwrap_or_default(),
            PriceOut => p.price_out.map(|v| v.to_string()).unwrap_or_default(),
            PriceCached => p.price_cached.map(|v| v.to_string()).unwrap_or_default(),
            // cycled with space, or not editable here at all
            Api | Think | Headers => String::new(),
        }
    }

    /// Cycled with space instead of typed into.
    fn is_toggle(self) -> bool {
        matches!(self, Api | Think)
    }

    /// Writes an edited value back. An empty string unsets the field.
    fn set(self, p: &mut Profile, raw: &str) -> Result<(), String> {
        let raw = raw.trim();
        let text = (!raw.is_empty()).then(|| raw.to_string());
        let whole = |what: &str| -> Result<Option<u32>, String> {
            match text.as_deref() {
                None => Ok(None),
                Some(s) => s
                    .parse::<u32>()
                    .map(Some)
                    .map_err(|_| format!("{what} must be a whole number")),
            }
        };
        let money = |what: &str| -> Result<Option<f64>, String> {
            match text.as_deref() {
                None => Ok(None),
                Some(s) => s
                    .trim_start_matches('$')
                    .parse::<f64>()
                    .map(Some)
                    .map_err(|_| format!("{what} must be a number, e.g. 3 or 0.25")),
            }
        };
        match self {
            BaseUrl => p.base_url = text,
            Model => p.model = text,
            ApiKey => p.api_key = text,
            ContextWindow => p.context_window = whole("context window")?,
            MaxTokens => p.max_tokens = whole("max tokens")?,
            MaxTurns => p.max_turns = whole("max turns")?.map(|n| n.max(1) as usize),
            PriceIn => p.price_in = money("price")?,
            PriceOut => p.price_out = money("price")?,
            PriceCached => p.price_cached = money("price")?,
            Temperature => {
                p.temperature = match text.as_deref() {
                    None => None,
                    Some(s) => Some(
                        s.parse::<f32>()
                            .map_err(|_| "temperature must be a number, e.g. 0.7".to_string())?,
                    ),
                }
            }
            Api | Think | Headers => {}
        }
        Ok(())
    }

    /// Space on a toggle field: think is on/off/default, api rotates through
    /// the protocols.
    fn cycle(self, p: &mut Profile) {
        match self {
            Think => {
                p.think = match p.think {
                    None => Some(true),
                    Some(true) => Some(false),
                    Some(false) => None,
                }
            }
            Api => {
                use crate::config::Api as A;
                p.api = match p.api.unwrap_or_default() {
                    A::Auto => Some(A::Openai),
                    A::Openai => Some(A::Ollama),
                    A::Ollama => Some(A::Anthropic),
                    A::Anthropic => None,
                }
            }
            _ => {}
        }
    }
}

#[derive(PartialEq)]
enum Pane {
    Profiles,
    Fields,
}

/// What the inline editor is editing.
enum Target {
    Field(Field),
    /// Renaming, or naming a profile that was just created.
    Name {
        was: Option<String>,
    },
}

struct Editing {
    target: Target,
    buf: String,
    cursor: usize,
}

pub struct ConfigScreen {
    store: Store,
    names: Vec<String>,
    /// Index into `names`; one past the end is the "new profile" row.
    sel: usize,
    pane: Pane,
    field: usize,
    editing: Option<Editing>,
    /// Profile waiting for a y/n on deletion.
    confirm_delete: Option<String>,
    dirty: bool,
    /// Set when esc was pressed with unsaved changes: a second one discards.
    quit_armed: bool,
    status: Option<(String, bool)>,
}

impl ConfigScreen {
    pub fn new() -> Self {
        let store = load_store();
        let names: Vec<String> = store.profiles.keys().cloned().collect();
        let sel = store
            .active
            .as_ref()
            .and_then(|a| names.iter().position(|n| n == a))
            .unwrap_or(0);
        Self {
            store,
            names,
            sel,
            pane: Pane::Profiles,
            field: 0,
            editing: None,
            confirm_delete: None,
            dirty: false,
            quit_armed: false,
            status: None,
        }
    }

    fn current(&self) -> Option<&String> {
        self.names.get(self.sel)
    }

    fn profile(&self) -> Profile {
        self.current()
            .map(|n| self.store.get(n))
            .unwrap_or_default()
    }

    fn note(&mut self, text: &str) {
        self.status = Some((text.to_string(), false));
    }

    fn fail(&mut self, text: &str) {
        self.status = Some((text.to_string(), true));
    }

    pub fn on_key(&mut self, k: KeyEvent) -> ConfigAction {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        self.status = None;
        if !matches!(k.code, KeyCode::Esc) {
            self.quit_armed = false;
        }

        if self.editing.is_some() {
            return self.edit_key(k);
        }
        if let Some(name) = self.confirm_delete.clone() {
            match k.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.store.profiles.remove(&name);
                    if self.store.active.as_ref() == Some(&name) {
                        self.store.active = None;
                    }
                    self.names.retain(|n| n != &name);
                    self.sel = self.sel.min(self.names.len().saturating_sub(1));
                    self.dirty = true;
                    self.note(&format!("deleted {name}"));
                }
                _ => self.note("kept it"),
            }
            self.confirm_delete = None;
            return ConfigAction::Stay;
        }

        match k.code {
            KeyCode::Char('s') if ctrl => return self.save(),
            KeyCode::Char('c') if ctrl => return ConfigAction::Close,
            KeyCode::Esc => {
                if self.dirty && !self.quit_armed {
                    self.quit_armed = true;
                    self.fail("unsaved changes: ctrl+s to save, esc again to discard");
                    return ConfigAction::Stay;
                }
                return ConfigAction::Close;
            }
            KeyCode::Tab => {
                self.pane = if self.pane == Pane::Profiles && self.current().is_some() {
                    Pane::Fields
                } else {
                    Pane::Profiles
                };
            }
            KeyCode::Up => self.move_sel(-1),
            KeyCode::Down => self.move_sel(1),
            KeyCode::Enter => return self.enter(),
            KeyCode::Char(' ') if self.pane == Pane::Fields => self.cycle_field(),
            KeyCode::Char('a') if self.pane == Pane::Profiles => {
                if let Some(n) = self.current().cloned() {
                    self.store.active = Some(n.clone());
                    self.dirty = true;
                    self.note(&format!("{n} is now the profile thoth starts with"));
                }
            }
            KeyCode::Char('n') if self.pane == Pane::Profiles => self.new_profile(),
            KeyCode::Char('r') if self.pane == Pane::Profiles => {
                if let Some(n) = self.current().cloned() {
                    self.editing = Some(Editing {
                        target: Target::Name {
                            was: Some(n.clone()),
                        },
                        cursor: n.chars().count(),
                        buf: n,
                    });
                }
            }
            KeyCode::Char('d') if self.pane == Pane::Profiles => {
                if let Some(n) = self.current().cloned() {
                    self.confirm_delete = Some(n);
                }
            }
            _ => {}
        }
        ConfigAction::Stay
    }

    fn move_sel(&mut self, delta: isize) {
        match self.pane {
            // one row past the end is "+ new profile"
            Pane::Profiles => {
                let n = self.names.len() as isize + 1;
                self.sel = (self.sel as isize + delta).rem_euclid(n) as usize;
                self.field = 0;
            }
            Pane::Fields => {
                let n = FIELDS.len() as isize;
                self.field = (self.field as isize + delta).rem_euclid(n) as usize;
            }
        }
    }

    fn enter(&mut self) -> ConfigAction {
        match self.pane {
            Pane::Profiles => {
                if self.current().is_none() {
                    self.new_profile();
                } else {
                    self.pane = Pane::Fields;
                    self.field = 0;
                }
            }
            Pane::Fields => {
                let field = FIELDS[self.field];
                if field == Headers {
                    self.fail("headers are edited in the config file");
                } else if field.is_toggle() {
                    self.cycle_field();
                } else {
                    let raw = field.raw(&self.profile());
                    self.editing = Some(Editing {
                        target: Target::Field(field),
                        cursor: raw.chars().count(),
                        buf: raw,
                    });
                }
            }
        }
        ConfigAction::Stay
    }

    fn new_profile(&mut self) {
        let name = self.store.free_name();
        self.editing = Some(Editing {
            target: Target::Name { was: None },
            cursor: name.chars().count(),
            buf: name,
        });
    }

    fn cycle_field(&mut self) {
        let field = FIELDS[self.field];
        if !field.is_toggle() {
            return;
        }
        let Some(name) = self.current().cloned() else {
            return;
        };
        field.cycle(self.store.profiles.entry(name).or_default());
        self.dirty = true;
    }

    fn edit_key(&mut self, k: KeyEvent) -> ConfigAction {
        let Some(e) = &mut self.editing else {
            return ConfigAction::Stay;
        };
        match k.code {
            KeyCode::Esc => {
                self.editing = None;
            }
            KeyCode::Enter => {
                let e = self.editing.take().expect("checked above");
                self.commit(e);
            }
            KeyCode::Backspace if e.cursor > 0 => {
                e.cursor -= 1;
                let i = byte_idx(&e.buf, e.cursor);
                e.buf.remove(i);
            }
            KeyCode::Delete if e.cursor < e.buf.chars().count() => {
                let i = byte_idx(&e.buf, e.cursor);
                e.buf.remove(i);
            }
            KeyCode::Left => e.cursor = e.cursor.saturating_sub(1),
            KeyCode::Right => e.cursor = (e.cursor + 1).min(e.buf.chars().count()),
            KeyCode::Home => e.cursor = 0,
            KeyCode::End => e.cursor = e.buf.chars().count(),
            KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                let i = byte_idx(&e.buf, e.cursor);
                e.buf.insert(i, c);
                e.cursor += 1;
            }
            _ => {}
        }
        ConfigAction::Stay
    }

    fn commit(&mut self, e: Editing) {
        match e.target {
            Target::Field(field) => {
                let Some(name) = self.current().cloned() else {
                    return;
                };
                let mut p = self.store.get(&name);
                match field.set(&mut p, &e.buf) {
                    Ok(()) => {
                        self.store.profiles.insert(name, p);
                        self.dirty = true;
                    }
                    Err(msg) => self.fail(&msg),
                }
            }
            Target::Name { was } => {
                let name = e.buf.trim().to_string();
                if name.is_empty() || name.contains(['/', '\\', ' ', '.', '"']) {
                    self.fail("name must be one word, without / \\ . or quotes");
                    return;
                }
                if Some(&name) != was.as_ref() && self.store.profiles.contains_key(&name) {
                    self.fail(&format!("there is already a profile called {name}"));
                    return;
                }
                let profile = match &was {
                    Some(old) => self.store.profiles.remove(old).unwrap_or_default(),
                    None => Profile::default(),
                };
                if let Some(old) = &was
                    && self.store.active.as_ref() == Some(old)
                {
                    self.store.active = Some(name.clone());
                }
                self.store.profiles.insert(name.clone(), profile);
                self.names = self.store.profiles.keys().cloned().collect();
                self.sel = self.names.iter().position(|n| n == &name).unwrap_or(0);
                self.dirty = true;
                if was.is_none() {
                    self.pane = Pane::Fields;
                    self.field = 0;
                }
            }
        }
    }

    fn save(&mut self) -> ConfigAction {
        // a profile the user just made is the one they want to use
        if self.store.active.is_none()
            && let Some(first) = self.names.first().cloned()
        {
            self.store.active = Some(first);
        }
        if let Err(e) = save_store(&self.store) {
            self.fail(&format!("could not save: {e:#}"));
            return ConfigAction::Stay;
        }
        self.dirty = false;
        let Some(name) = self.store.active.clone() else {
            self.note("saved");
            return ConfigAction::Stay;
        };
        self.note(&format!("saved. using {name}"));
        ConfigAction::Apply(name.clone(), Box::new(resolve(&self.store.get(&name))))
    }

    pub fn draw(&self, f: &mut Frame) {
        let [title_a, rule_a, body_a, foot_rule_a, hint_a] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(f.area());
        let w = title_a.width as usize;

        let right = "ctrl+s save   esc back";
        let left = " config ";
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(left, theme::accent().add_modifier(Modifier::REVERSED)),
                Span::raw(" ".repeat(w.saturating_sub(left.len() + right.len()))),
                Span::styled(right, theme::muted()),
            ])),
            title_a,
        );
        for area in [rule_a, foot_rule_a] {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    theme::RULE.repeat(area.width as usize),
                    theme::muted(),
                ))),
                area,
            );
        }

        let [list_a, detail_a] =
            Layout::horizontal([Constraint::Length(24), Constraint::Min(20)]).areas(body_a);
        self.draw_profiles(f, list_a);
        self.draw_fields(f, detail_a);

        let hint = match (
            &self.status,
            &self.confirm_delete,
            self.pane == Pane::Fields,
        ) {
            (Some((msg, is_err)), _, _) => Line::from(Span::styled(
                format!("  {msg}"),
                if *is_err {
                    theme::danger()
                } else {
                    theme::muted()
                },
            )),
            (_, Some(name), _) => Line::from(vec![
                Span::styled(format!("  delete {name}? "), theme::danger()),
                Span::styled("y", theme::key()),
                Span::styled(" / ", theme::muted()),
                Span::styled("n", theme::key()),
            ]),
            (_, _, true) => keys(&[
                ("↑↓", "field"),
                ("enter", "edit"),
                ("space", "toggle"),
                ("tab", "profiles"),
            ]),
            _ => keys(&[
                ("↑↓", "profile"),
                ("enter", "open"),
                ("a", "activate"),
                ("n", "new"),
                ("r", "rename"),
                ("d", "delete"),
            ]),
        };
        f.render_widget(Paragraph::new(hint), hint_a);
    }

    fn draw_profiles(&self, f: &mut Frame, area: Rect) {
        let width = area.width.saturating_sub(2) as usize;
        let mut lines: Vec<Line> = Vec::new();
        let naming = matches!(
            self.editing,
            Some(Editing {
                target: Target::Name { .. },
                ..
            })
        );
        for (i, name) in self.names.iter().enumerate() {
            let picked = i == self.sel;
            if picked && naming {
                lines.push(edit_line(self.editing.as_ref().expect("naming"), width));
                continue;
            }
            let active = self.store.active.as_ref() == Some(name);
            let style = match (picked, self.pane == Pane::Profiles) {
                (true, true) => theme::accent().add_modifier(Modifier::REVERSED),
                (true, false) => theme::accent(),
                _ => Style::default(),
            };
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(clip(name, width.saturating_sub(9)), style),
                Span::styled(if active { "  active" } else { "" }, theme::muted_italic()),
            ]));
        }
        let picked = self.sel >= self.names.len();
        if picked && naming {
            lines.push(edit_line(self.editing.as_ref().expect("naming"), width));
        } else {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    "+ new profile",
                    if picked && self.pane == Pane::Profiles {
                        theme::accent().add_modifier(Modifier::REVERSED)
                    } else {
                        theme::muted()
                    },
                ),
            ]));
        }
        f.render_widget(Paragraph::new(Text::from(lines)), area);
    }

    fn draw_fields(&self, f: &mut Frame, area: Rect) {
        let width = area.width as usize;
        if self.names.is_empty() {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "no profiles yet. enter on \"+ new profile\" makes one",
                    theme::muted_italic(),
                ))),
                area,
            );
            return;
        }
        let profile = self.profile();
        let editing_field = match &self.editing {
            Some(e) => match e.target {
                Target::Field(field) => Some(field),
                _ => None,
            },
            None => None,
        };
        let mut lines: Vec<Line> = Vec::new();
        for (i, field) in FIELDS.iter().enumerate() {
            let picked = i == self.field && self.pane == Pane::Fields;
            if editing_field == Some(*field) {
                let e = self.editing.as_ref().expect("editing");
                lines.push(Line::from(vec![
                    Span::styled(format!("{:<15}", field.label()), theme::muted()),
                    Span::styled(
                        clip(&e.buf, width.saturating_sub(17)),
                        Style::default().add_modifier(Modifier::UNDERLINED),
                    ),
                    Span::styled("_", theme::accent()),
                ]));
                continue;
            }
            let (value, is_set) = field.value(&profile);
            let value_style = match (picked, is_set) {
                (true, _) => theme::accent().add_modifier(Modifier::REVERSED),
                (false, true) => Style::default(),
                (false, false) => theme::muted_italic(),
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{:<15}", field.label()), theme::muted()),
                Span::styled(clip(&value, width.saturating_sub(16)), value_style),
            ]));
        }
        if self.pane == Pane::Fields {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                clip(FIELDS[self.field].hint(), width),
                theme::muted_italic(),
            )));
        }
        f.render_widget(Paragraph::new(Text::from(lines)), area);
    }
}

fn edit_line(e: &Editing, width: usize) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(
            clip(&e.buf, width.saturating_sub(1)),
            Style::default().add_modifier(Modifier::UNDERLINED),
        ),
        Span::styled("_", theme::accent()),
    ])
}

fn keys(pairs: &[(&str, &str)]) -> Line<'static> {
    let mut spans = vec![Span::raw("  ")];
    for (k, what) in pairs {
        spans.push(Span::styled(k.to_string(), theme::key()));
        spans.push(Span::styled(format!(" {what}   "), theme::muted()));
    }
    Line::from(spans)
}

fn byte_idx(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

/// `thoth config`: the same screen with its own terminal and event loop.
pub fn run_standalone() -> Result<()> {
    let mut terminal = ratatui::init();
    let mut screen = ConfigScreen::new();
    let result = loop {
        if let Err(e) = terminal.draw(|f| screen.draw(f)) {
            break Err(e.into());
        }
        match event::read() {
            Ok(Event::Key(k)) if k.kind != KeyEventKind::Release => {
                if matches!(screen.on_key(k), ConfigAction::Close) {
                    break Ok(());
                }
            }
            Ok(_) => {}
            Err(e) => break Err(e.into()),
        }
    };
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn screen(profiles: &[&str]) -> ConfigScreen {
        let mut s = ConfigScreen::new();
        // never touch the developer's real config file in tests
        s.store = Store::default();
        for name in profiles {
            s.store
                .profiles
                .insert((*name).to_string(), Profile::default());
        }
        s.store.active = profiles.first().map(|n| (*n).to_string());
        s.names = s.store.profiles.keys().cloned().collect();
        s.sel = 0;
        s
    }

    fn render(s: &ConfigScreen) -> String {
        let mut term = Terminal::new(TestBackend::new(80, 18)).unwrap();
        term.draw(|f| s.draw(f)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..18)
            .map(|y| {
                (0..80)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// cargo test config_preview -- --ignored --nocapture
    #[test]
    #[ignore]
    fn config_preview() {
        let mut s = screen(&["local", "workstation", "openrouter"]);
        s.store.profiles.insert(
            "workstation".into(),
            Profile {
                base_url: Some("https://api.anthropic.com/v1".into()),
                api: Some(crate::config::Api::Anthropic),
                api_key: Some("sk-ant-secret".into()),
                model: Some("claude-sonnet-4-5".into()),
                context_window: Some(200_000),
                price_in: Some(3.0),
                price_out: Some(15.0),
                price_cached: Some(0.3),
                ..Default::default()
            },
        );
        s.sel = 2;
        s.pane = Pane::Fields;
        s.field = 5;
        println!("\n{}", render(&s));
    }

    #[test]
    fn shows_profiles_and_which_one_is_active() {
        let s = screen(&["big", "local"]);
        let out = render(&s);
        assert!(out.contains("big"), "{out}");
        assert!(out.contains("active"), "{out}");
        assert!(out.contains("+ new profile"), "{out}");
        assert!(out.contains("base url"), "{out}");
    }

    #[test]
    fn editing_a_field_writes_it_back() {
        let mut s = screen(&["local"]);
        s.on_key(key(KeyCode::Tab)); // into the fields pane
        s.on_key(key(KeyCode::Down)); // past the api toggle, onto base url
        s.on_key(key(KeyCode::Enter));
        for c in "http://box:11434/v1".chars() {
            s.on_key(key(KeyCode::Char(c)));
        }
        s.on_key(key(KeyCode::Enter));
        assert_eq!(
            s.store.get("local").base_url.as_deref(),
            Some("http://box:11434/v1")
        );
        assert!(s.dirty);
        assert!(render(&s).contains("http://box:11434/v1"));
    }

    #[test]
    fn rejects_a_number_field_that_is_not_a_number() {
        let mut s = screen(&["local"]);
        s.pane = Pane::Fields;
        s.field = FIELDS.iter().position(|f| *f == ContextWindow).unwrap();
        s.on_key(key(KeyCode::Enter));
        for c in "lots".chars() {
            s.on_key(key(KeyCode::Char(c)));
        }
        s.on_key(key(KeyCode::Enter));
        assert_eq!(s.store.get("local").context_window, None);
        assert!(
            s.status.as_ref().unwrap().1,
            "should be flagged as an error"
        );
        assert!(render(&s).contains("whole number"));
    }

    #[test]
    fn think_cycles_through_three_states() {
        let mut s = screen(&["local"]);
        s.pane = Pane::Fields;
        s.field = FIELDS.iter().position(|f| *f == Think).unwrap();
        assert_eq!(s.store.get("local").think, None);
        s.on_key(key(KeyCode::Char(' ')));
        assert_eq!(s.store.get("local").think, Some(true));
        s.on_key(key(KeyCode::Char(' ')));
        assert_eq!(s.store.get("local").think, Some(false));
        s.on_key(key(KeyCode::Char(' ')));
        assert_eq!(s.store.get("local").think, None);
    }

    #[test]
    fn renaming_carries_the_active_flag() {
        let mut s = screen(&["local"]);
        s.on_key(key(KeyCode::Char('r')));
        for _ in 0.."local".len() {
            s.on_key(key(KeyCode::Backspace));
        }
        for c in "laptop".chars() {
            s.on_key(key(KeyCode::Char(c)));
        }
        s.on_key(key(KeyCode::Enter));
        assert!(s.store.profiles.contains_key("laptop"));
        assert!(!s.store.profiles.contains_key("local"));
        assert_eq!(s.store.active.as_deref(), Some("laptop"));
    }

    #[test]
    fn delete_asks_first() {
        let mut s = screen(&["local", "other"]);
        s.on_key(key(KeyCode::Char('d')));
        assert!(render(&s).contains("delete local?"));
        s.on_key(key(KeyCode::Char('n')));
        assert!(s.store.profiles.contains_key("local"));
        s.on_key(key(KeyCode::Char('d')));
        s.on_key(key(KeyCode::Char('y')));
        assert!(!s.store.profiles.contains_key("local"));
        assert_eq!(s.store.active, None, "the active profile is gone with it");
    }

    #[test]
    fn esc_with_unsaved_changes_asks_twice() {
        let mut s = screen(&["local"]);
        s.dirty = true;
        assert!(matches!(s.on_key(key(KeyCode::Esc)), ConfigAction::Stay));
        assert!(render(&s).contains("unsaved changes"));
        assert!(matches!(s.on_key(key(KeyCode::Esc)), ConfigAction::Close));
    }
}
