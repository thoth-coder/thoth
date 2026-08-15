//! The config screen: one column, one row at a time. The row you can see
//! decides what the keys do, so there is no invisible focus to keep track
//! of. `thoth config` runs it on its own and `/config` opens it over a
//! session, both through this struct, so the two cannot drift.

use crate::config::{Config, Profile, Store, load_store, resolve, save_store};
use crate::ui::input::byte_idx;
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

#[derive(Debug, Clone, Copy, PartialEq)]
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

/// Fields past this one are for people who know they need them.
const ESSENTIAL: usize = 4;

/// Endpoints worth offering by name, so a new profile does not start with an
/// empty url. Model and key stay for the user; those are theirs to know.
struct Preset {
    label: &'static str,
    api: crate::config::Api,
    base_url: &'static str,
    context_window: Option<u32>,
}

const PRESETS: [Preset; 6] = [
    Preset {
        label: "anthropic",
        api: crate::config::Api::Anthropic,
        base_url: "https://api.anthropic.com/v1",
        context_window: Some(200_000),
    },
    Preset {
        label: "openai",
        api: crate::config::Api::Openai,
        base_url: "https://api.openai.com/v1",
        context_window: None,
    },
    Preset {
        label: "google gemini",
        api: crate::config::Api::Openai,
        base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
        context_window: None,
    },
    Preset {
        label: "openrouter",
        api: crate::config::Api::Openai,
        base_url: "https://openrouter.ai/api/v1",
        context_window: None,
    },
    Preset {
        label: "ollama (this machine)",
        api: crate::config::Api::Auto,
        base_url: "http://localhost:11434/v1",
        context_window: None,
    },
    Preset {
        label: "start empty",
        api: crate::config::Api::Auto,
        base_url: "",
        context_window: None,
    },
];

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

/// A question that owns the keyboard until it is answered.
enum Ask {
    Delete(String),
    /// Leaving with unsaved changes.
    Leave,
    /// Which endpoint the profile just named should start from.
    Preset {
        name: String,
        sel: usize,
    },
}

/// Two pages: the profiles, and the settings of the one you opened. Each is
/// a single column, and the row you can see decides what the keys do, so
/// there is no invisible focus to keep track of.
#[derive(PartialEq)]
enum Page {
    List,
    Settings,
}

pub struct ConfigScreen {
    store: Store,
    names: Vec<String>,
    page: Page,
    /// Which profile the settings page belongs to.
    profile: usize,
    /// Cursor: a profile on the list page, a setting on the other.
    row: usize,
    editing: Option<Editing>,
    ask: Option<Ask>,
    dirty: bool,
    status: Option<(String, bool)>,
    /// The active profile and its settings when the screen opened. Saving an
    /// edit to some other profile must not restart the session for nothing.
    opened_as: Option<(String, Config)>,
}

impl ConfigScreen {
    pub fn new() -> Self {
        let store = load_store();
        let names: Vec<String> = store.profiles.keys().cloned().collect();
        let profile = store
            .active
            .as_ref()
            .and_then(|a| names.iter().position(|n| n == a))
            .unwrap_or(0);
        Self {
            opened_as: live_config(&store),
            store,
            names,
            page: Page::List,
            profile,
            row: profile,
            editing: None,
            ask: None,
            dirty: false,
            status: None,
        }
    }

    fn current(&self) -> Option<&String> {
        self.names.get(self.profile)
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

    fn edit_profile(&mut self, f: impl FnOnce(&mut Profile)) {
        let Some(name) = self.current().cloned() else {
            return;
        };
        f(self.store.profiles.entry(name).or_default());
        self.dirty = true;
    }

    pub fn on_key(&mut self, k: KeyEvent) -> ConfigAction {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        self.status = None;
        if self.editing.is_some() {
            return self.edit_key(k);
        }
        if self.ask.is_some() {
            return self.ask_key(k);
        }
        match k.code {
            KeyCode::Char('s') if ctrl => self.save(),
            KeyCode::Char('c') if ctrl => ConfigAction::Close,
            _ if self.page == Page::List => self.list_key(k, ctrl),
            _ => self.settings_key(k, ctrl),
        }
    }

    /// The first page: nothing but the profiles.
    fn list_key(&mut self, k: KeyEvent, ctrl: bool) -> ConfigAction {
        match k.code {
            KeyCode::Esc => {
                if self.dirty {
                    self.ask = Some(Ask::Leave);
                    return ConfigAction::Stay;
                }
                return ConfigAction::Close;
            }
            KeyCode::Up => self.go(self.row.saturating_sub(1)),
            KeyCode::Down => self.go((self.row + 1).min(self.names.len())),
            KeyCode::Enter | KeyCode::Tab | KeyCode::Right if self.on_new_row() => {
                self.new_profile()
            }
            KeyCode::Enter | KeyCode::Tab | KeyCode::Right => self.page = Page::Settings,
            KeyCode::Char(c) if !ctrl => match c {
                'n' => self.new_profile(),
                'r' => self.rename(),
                'd' => {
                    if let Some(n) = self.current().cloned() {
                        self.ask = Some(Ask::Delete(n));
                    }
                }
                'a' => self.activate(),
                _ => {}
            },
            _ => {}
        }
        ConfigAction::Stay
    }

    /// The second page: the settings of the profile that was opened.
    fn settings_key(&mut self, k: KeyEvent, ctrl: bool) -> ConfigAction {
        match k.code {
            // back to the list, which is also where leaving happens
            KeyCode::Esc | KeyCode::Left => self.back_to_list(),
            KeyCode::Up => self.row = self.row.saturating_sub(1),
            KeyCode::Down => self.row = (self.row + 1).min(FIELDS.len() - 1),
            KeyCode::Tab => {
                self.row = if self.row + 1 >= FIELDS.len() {
                    0
                } else {
                    self.row + 1
                }
            }
            KeyCode::Enter => self.open_editor(false),
            KeyCode::Char(' ') => match self.field() {
                Some(f) if f.is_toggle() => self.edit_profile(|p| f.cycle(p)),
                _ => self.open_editor(false),
            },
            KeyCode::Char(c) if !ctrl => {
                self.open_editor(true);
                if let Some(e) = &mut self.editing {
                    e.buf.push(c);
                    e.cursor = e.buf.chars().count();
                }
            }
            _ => {}
        }
        ConfigAction::Stay
    }

    fn back_to_list(&mut self) {
        self.page = Page::List;
        self.row = self.profile;
    }

    /// The last row of the list is the one that adds a profile.
    fn on_new_row(&self) -> bool {
        self.row >= self.names.len()
    }

    /// The setting the cursor is on, on the settings page.
    fn field(&self) -> Option<Field> {
        match self.page {
            Page::List => None,
            Page::Settings => FIELDS.get(self.row).copied(),
        }
    }

    /// Moves the cursor in the list. Landing on a profile is what opens it,
    /// so the settings page always belongs to the name you stood on.
    fn go(&mut self, row: usize) {
        self.row = row.min(self.names.len());
        if self.row < self.names.len() {
            self.profile = self.row;
        }
    }

    /// Opens the inline editor on the selected field. `fresh` starts from an
    /// empty buffer, which is what typing over a value means.
    fn open_editor(&mut self, fresh: bool) {
        let Some(field) = self.field() else {
            return;
        };
        if self.current().is_none() {
            self.fail("no profile yet: press n on the profile row");
            return;
        }
        if field.is_toggle() {
            self.edit_profile(|p| field.cycle(p));
            return;
        }
        if field == Headers {
            self.fail("headers are edited in the config file");
            return;
        }
        let raw = if fresh {
            String::new()
        } else {
            field.raw(&self.profile())
        };
        self.editing = Some(Editing {
            target: Target::Field(field),
            cursor: raw.chars().count(),
            buf: raw,
        });
    }

    fn activate(&mut self) {
        if let Some(n) = self.current().cloned() {
            self.store.active = Some(n.clone());
            self.dirty = true;
            self.note(&format!(
                "{n} is now the active profile, the one thoth starts with"
            ));
        }
    }

    fn new_profile(&mut self) {
        let name = self.store.free_name();
        self.editing = Some(Editing {
            target: Target::Name { was: None },
            cursor: name.chars().count(),
            buf: name,
        });
    }

    fn rename(&mut self) {
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

    fn ask_key(&mut self, k: KeyEvent) -> ConfigAction {
        let Some(ask) = self.ask.take() else {
            return ConfigAction::Stay;
        };
        match ask {
            Ask::Delete(name) => match k.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.store.profiles.remove(&name);
                    if self.store.active.as_ref() == Some(&name) {
                        self.store.active = None;
                    }
                    self.names.retain(|n| n != &name);
                    self.profile = self.profile.min(self.names.len().saturating_sub(1));
                    self.dirty = true;
                    self.note(&format!("deleted {name}"));
                }
                _ => self.note("kept it"),
            },
            Ask::Leave => match k.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => return self.save_and_close(),
                KeyCode::Char('n') | KeyCode::Char('N') => return ConfigAction::Close,
                // anything else goes back to editing
                _ => self.note("still editing"),
            },
            Ask::Preset { name, sel } => match k.code {
                KeyCode::Up => {
                    self.ask = Some(Ask::Preset {
                        name,
                        sel: sel.saturating_sub(1),
                    })
                }
                KeyCode::Down => {
                    self.ask = Some(Ask::Preset {
                        name,
                        sel: (sel + 1).min(PRESETS.len() - 1),
                    })
                }
                KeyCode::Enter => self.apply_preset(&name, sel),
                KeyCode::Esc => self.apply_preset(&name, PRESETS.len() - 1),
                _ => self.ask = Some(Ask::Preset { name, sel }),
            },
        }
        ConfigAction::Stay
    }

    fn apply_preset(&mut self, name: &str, sel: usize) {
        let preset = &PRESETS[sel.min(PRESETS.len() - 1)];
        let p = self.store.profiles.entry(name.to_string()).or_default();
        if !preset.base_url.is_empty() {
            p.base_url = Some(preset.base_url.to_string());
            p.api = Some(preset.api);
            p.context_window = preset.context_window;
        }
        self.dirty = true;
        // straight into the settings, on the model: the one thing a preset
        // cannot know
        self.page = Page::Settings;
        self.row = FIELDS.iter().position(|f| *f == Model).unwrap_or(0);
        self.note("now set the model, and the api key if the server wants one");
    }

    fn edit_key(&mut self, k: KeyEvent) -> ConfigAction {
        let Some(e) = &mut self.editing else {
            return ConfigAction::Stay;
        };
        match k.code {
            KeyCode::Esc => self.editing = None,
            KeyCode::Enter | KeyCode::Tab => {
                let down = k.code == KeyCode::Tab;
                let e = self.editing.take().expect("checked above");
                self.commit(e);
                if down && self.editing.is_none() {
                    self.row = (self.row + 1).min(FIELDS.len());
                }
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

    /// Applies an edit. Anything the user has to fix leaves the editor open
    /// with what they typed still in it.
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
                    Err(msg) => {
                        self.fail(&msg);
                        self.editing = Some(e);
                    }
                }
            }
            Target::Name { was } => {
                let name = e.buf.trim().to_string();
                let problem = if name.is_empty() || name.contains(['/', '\\', ' ', '.', '"']) {
                    Some("name must be one word, without / \\ . or quotes".to_string())
                } else if Some(&name) != was.as_ref() && self.store.profiles.contains_key(&name) {
                    Some(format!("there is already a profile called {name}"))
                } else {
                    None
                };
                if let Some(msg) = problem {
                    self.fail(&msg);
                    self.editing = Some(Editing {
                        target: Target::Name { was },
                        cursor: e.cursor,
                        buf: e.buf,
                    });
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
                self.profile = self.names.iter().position(|n| n == &name).unwrap_or(0);
                self.dirty = true;
                if was.is_none() {
                    self.ask = Some(Ask::Preset { name, sel: 0 });
                }
            }
        }
    }

    fn save_and_close(&mut self) -> ConfigAction {
        match self.save() {
            ConfigAction::Apply(name, cfg) => ConfigAction::Apply(name, cfg),
            // a failed write keeps the screen open with the error
            _ if self.dirty => ConfigAction::Stay,
            _ => ConfigAction::Close,
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
        self.applied()
    }

    /// What the host should do now that the file is written. Kept apart from
    /// the write so it can be tested without touching the real config.
    fn applied(&mut self) -> ConfigAction {
        let now = live_config(&self.store);
        if now == self.opened_as {
            // nothing the running session cares about moved
            self.note("saved");
            return ConfigAction::Stay;
        }
        self.opened_as = now.clone();
        match now {
            Some((name, cfg)) => {
                self.note(&format!("saved. using {name}"));
                ConfigAction::Apply(name, Box::new(cfg))
            }
            None => {
                self.note("saved");
                ConfigAction::Stay
            }
        }
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

        let left = " config ";
        let dirty = if self.dirty { "  ● unsaved" } else { "" };
        let right = "ctrl+s save   esc back";
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(left, theme::accent().add_modifier(Modifier::REVERSED)),
                Span::styled(dirty, theme::danger()),
                Span::raw(
                    " ".repeat(w.saturating_sub(left.len() + dirty.chars().count() + right.len())),
                ),
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

        match (&self.ask, &self.page) {
            (Some(Ask::Preset { name, sel }), _) => self.draw_presets(f, body_a, name, *sel),
            (_, Page::List) => self.draw_list(f, body_a),
            (_, Page::Settings) => self.draw_settings(f, body_a),
        }
        f.render_widget(Paragraph::new(self.hint(hint_a.width as usize)), hint_a);
    }

    fn draw_presets(&self, f: &mut Frame, area: Rect, name: &str, sel: usize) {
        let width = area.width as usize;
        let mut lines = vec![
            Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("new profile \"{name}\""), theme::bold()),
                Span::styled("   what does it talk to?", theme::muted()),
            ]),
            Line::default(),
        ];
        for (i, p) in PRESETS.iter().enumerate() {
            let style = if i == sel {
                theme::accent().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{:<22}", p.label), style),
                Span::styled(clip(p.base_url, width.saturating_sub(26)), theme::muted()),
            ]));
        }
        f.render_widget(Paragraph::new(Text::from(lines)), area);
    }

    /// The first page: the profiles, and nothing else.
    fn draw_list(&self, f: &mut Frame, area: Rect) {
        let mut lines = self.profile_rows_lines(area.width as usize);
        if self.names.is_empty() {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                "  a profile is a server, a model and a key. thoth starts with the active one",
                theme::muted_italic(),
            )));
        }
        f.render_widget(Paragraph::new(Text::from(lines)), area);
    }

    fn draw_settings(&self, f: &mut Frame, area: Rect) {
        let width = area.width as usize;
        let value_width = width.saturating_sub(20);
        let whose = match self.current() {
            Some(n) => n.clone(),
            None => "(none)".to_string(),
        };
        let active = self.store.active.as_deref() == self.current().map(|n| n.as_str());
        let mut lines: Vec<Line> = vec![
            Line::from(vec![
                Span::raw("  "),
                Span::styled(whose, theme::accent().add_modifier(Modifier::BOLD)),
                Span::styled(if active { "  active" } else { "" }, theme::muted_italic()),
            ]),
            Line::default(),
        ];

        let profile = self.profile();
        let editing_field = match &self.editing {
            Some(Editing {
                target: Target::Field(f),
                ..
            }) => Some(*f),
            _ => None,
        };
        // drop settings off the top when a short terminal cannot hold them
        let room = (area.height as usize).saturating_sub(lines.len() + 3);
        let first = (self.row + 1).saturating_sub(room.max(1));
        for (i, field) in FIELDS.iter().enumerate().skip(first) {
            if i == ESSENTIAL {
                lines.push(Line::default());
                lines.push(Line::from(Span::styled(
                    format!(
                        "  ─ advanced {}",
                        theme::RULE.repeat(width.saturating_sub(15))
                    ),
                    theme::muted(),
                )));
            }
            let picked = self.field() == Some(*field);
            if editing_field == Some(*field) {
                let e = self.editing.as_ref().expect("editing");
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(format!("{:<15}", field.label()), theme::muted()),
                    Span::styled(
                        clip(&e.buf, value_width),
                        Style::default().add_modifier(Modifier::UNDERLINED),
                    ),
                    Span::styled("▏", theme::accent()),
                ]));
                continue;
            }
            let (value, is_set) = field.value(&profile);
            lines.push(Line::from(vec![
                Span::styled(if picked { "▸ " } else { "  " }, theme::accent()),
                Span::styled(format!("{:<15}", field.label()), theme::muted()),
                Span::styled(
                    clip(&value, value_width),
                    match (picked, is_set) {
                        (true, _) => theme::accent().add_modifier(Modifier::REVERSED),
                        (false, true) => Style::default(),
                        (false, false) => theme::muted_italic(),
                    },
                ),
            ]));
        }
        f.render_widget(Paragraph::new(Text::from(lines)), area);
    }

    /// Every profile, then the row that adds one. The label sits on the
    /// first line only, like a form field whose value happens to be a list.
    fn profile_rows_lines(&self, width: usize) -> Vec<Line<'static>> {
        let naming = match &self.editing {
            Some(
                e @ Editing {
                    target: Target::Name { was },
                    ..
                },
            ) => Some((e, was.is_some())),
            _ => None,
        };
        let label = |i: usize| format!("{:<15}", if i == 0 { "profiles" } else { "" });
        let mut lines: Vec<Line> = Vec::new();
        for (i, name) in self.names.iter().enumerate() {
            let picked = self.row == i;
            // a rename happens in place, on the row being renamed
            if let Some((e, true)) = naming
                && i == self.profile
            {
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(label(i), theme::muted()),
                    Span::styled(
                        clip(&e.buf, width.saturating_sub(20)),
                        Style::default().add_modifier(Modifier::UNDERLINED),
                    ),
                    Span::styled("▏", theme::accent()),
                ]));
                continue;
            }
            let active = self.store.active.as_deref() == Some(name.as_str());
            lines.push(Line::from(vec![
                Span::styled(if picked { "▸ " } else { "  " }, theme::accent()),
                Span::styled(label(i), theme::muted()),
                Span::styled(
                    clip(name, width.saturating_sub(30)),
                    match (picked, i == self.profile) {
                        (true, _) => theme::accent().add_modifier(Modifier::REVERSED),
                        (false, true) => theme::accent(),
                        (false, false) => Style::default(),
                    },
                ),
                Span::styled(if active { "  active" } else { "" }, theme::muted_italic()),
            ]));
        }
        // naming a brand new profile: the row does not exist yet
        if let Some((e, false)) = naming {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(label(lines.len()), theme::muted()),
                Span::styled(
                    clip(&e.buf, width.saturating_sub(20)),
                    Style::default().add_modifier(Modifier::UNDERLINED),
                ),
                Span::styled("▏", theme::accent()),
            ]));
        } else {
            let picked = self.on_new_row();
            lines.push(Line::from(vec![
                Span::styled(if picked { "▸ " } else { "  " }, theme::accent()),
                Span::styled(label(lines.len()), theme::muted()),
                Span::styled(
                    "+ new profile",
                    if picked {
                        theme::accent().add_modifier(Modifier::REVERSED)
                    } else {
                        theme::muted()
                    },
                ),
            ]));
        }
        lines
    }

    /// The bottom line: what the current row does, or whatever just happened.
    fn hint(&self, width: usize) -> Line<'static> {
        if let Some((msg, is_err)) = &self.status {
            return Line::from(Span::styled(
                format!("  {msg}"),
                if *is_err {
                    theme::danger()
                } else {
                    theme::muted()
                },
            ));
        }
        match &self.ask {
            Some(Ask::Delete(name)) => {
                return keys(&[("y", &format!("delete {name}")), ("n", "keep it")]);
            }
            Some(Ask::Leave) => {
                return keys(&[
                    ("y", "save and close"),
                    ("n", "discard"),
                    ("esc", "keep editing"),
                ]);
            }
            Some(Ask::Preset { .. }) => {
                return keys(&[("↑↓", "choose"), ("enter", "use it"), ("esc", "empty")]);
            }
            None => {}
        }
        if self.editing.is_some() {
            return keys(&[
                ("enter", "keep"),
                ("tab", "keep and next"),
                ("esc", "cancel"),
            ]);
        }
        match self.field() {
            None if self.on_new_row() => keys(&[("enter", "add a profile")]),
            None => keys(&[
                ("↑↓", "profile"),
                ("tab", "settings"),
                ("a", "make active"),
                ("n", "new"),
                ("r", "rename"),
                ("d", "delete"),
            ]),
            Some(f) if f.is_toggle() => Line::from(vec![
                Span::raw("  "),
                Span::styled("space", theme::key()),
                Span::styled(
                    format!(" changes it   {}", clip(f.hint(), width.saturating_sub(19))),
                    theme::muted(),
                ),
            ]),
            Some(f) => Line::from(vec![
                Span::raw("  "),
                Span::styled("type or enter", theme::key()),
                Span::styled(
                    format!(" to edit   {}", clip(f.hint(), width.saturating_sub(28))),
                    theme::muted(),
                ),
            ]),
        }
    }
}

/// The active profile and what it resolves to: what a running session would
/// actually be using.
fn live_config(store: &Store) -> Option<(String, Config)> {
    let name = store.active.clone()?;
    if !store.profiles.contains_key(&name) {
        return None;
    }
    Some((name.clone(), resolve(&store.get(&name))))
}

fn keys(pairs: &[(&str, &str)]) -> Line<'static> {
    let mut spans = vec![Span::raw("  ")];
    for (k, what) in pairs {
        spans.push(Span::styled(k.to_string(), theme::key()));
        spans.push(Span::styled(format!(" {what}   "), theme::muted()));
    }
    Line::from(spans)
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

    /// Where a setting sits now that the profile list is above it.
    fn row_of(f: Field) -> usize {
        FIELDS.iter().position(|x| *x == f).unwrap()
    }

    /// Opens the settings page of the selected profile, as enter would.
    fn settings(s: &mut ConfigScreen, f: Field) {
        s.page = Page::Settings;
        s.row = row_of(f);
    }

    fn typed(s: &mut ConfigScreen, text: &str) {
        for c in text.chars() {
            s.on_key(key(KeyCode::Char(c)));
        }
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
        s.profile = 0;
        s.opened_as = live_config(&s.store);
        s
    }

    fn render(s: &ConfigScreen) -> String {
        let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
        term.draw(|f| s.draw(f)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..20)
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
        let mut s = screen(&["claude", "local"]);
        s.store.profiles.insert(
            "claude".into(),
            Profile {
                api: Some(crate::config::Api::Anthropic),
                base_url: Some("https://api.anthropic.com/v1".into()),
                model: Some("claude-sonnet-4-5".into()),
                api_key: Some("sk-ant-secret".into()),
                context_window: Some(200_000),
                price_in: Some(3.0),
                price_out: Some(15.0),
                ..Default::default()
            },
        );
        s.dirty = true;
        println!("\n{}", render(&s));
        s.page = Page::Settings;
        s.row = 1;
        println!("\n{}", render(&s));
        s.ask = Some(Ask::Preset {
            name: "gpt".into(),
            sel: 1,
        });
        println!("\n{}", render(&s));
    }

    #[test]
    fn the_first_page_is_only_the_profiles() {
        let mut s = screen(&["local", "other"]);
        let out = render(&s);
        assert!(out.contains("local"), "{out}");
        assert!(out.contains("other"), "{out}");
        assert!(out.contains("active"), "{out}");
        assert!(out.contains("+ new profile"), "{out}");
        // no settings until one is opened
        assert!(!out.contains("base url"), "{out}");
        assert!(!out.contains("advanced"), "{out}");
        // and the keys for the row you are on
        assert!(out.contains("n new"), "{out}");

        // enter opens the one under the cursor, esc comes back
        s.on_key(key(KeyCode::Down));
        s.on_key(key(KeyCode::Enter));
        let out = render(&s);
        assert!(out.contains("base url"), "{out}");
        assert!(out.contains("advanced"), "{out}");
        assert!(out.contains("other"), "the page says whose settings: {out}");
        assert!(!out.contains("+ new profile"), "{out}");
        s.on_key(key(KeyCode::Esc));
        assert!(render(&s).contains("+ new profile"), "esc should go back");
    }

    #[test]
    fn typing_on_a_field_replaces_it_without_pressing_enter_first() {
        let mut s = screen(&["local"]);
        settings(&mut s, BaseUrl);
        typed(&mut s, "http://box:11434/v1");
        assert!(s.editing.is_some(), "typing should have opened the editor");
        s.on_key(key(KeyCode::Enter));
        assert_eq!(
            s.store.get("local").base_url.as_deref(),
            Some("http://box:11434/v1")
        );
        assert!(s.dirty);
        assert!(render(&s).contains("unsaved"), "no unsaved marker");
    }

    #[test]
    fn enter_edits_the_value_that_is_already_there() {
        let mut s = screen(&["local"]);
        Model
            .set(s.store.profiles.get_mut("local").unwrap(), "qwen3:8b")
            .unwrap();
        settings(&mut s, Model);
        s.on_key(key(KeyCode::Enter));
        assert_eq!(s.editing.as_ref().unwrap().buf, "qwen3:8b");
        typed(&mut s, "-instruct");
        s.on_key(key(KeyCode::Enter));
        assert_eq!(
            s.store.get("local").model.as_deref(),
            Some("qwen3:8b-instruct")
        );
    }

    #[test]
    fn letters_are_commands_only_on_the_profile_row() {
        let mut s = screen(&["local"]);
        // on a setting, n is just the letter n
        settings(&mut s, Model);
        s.on_key(key(KeyCode::Char('n')));
        assert_eq!(s.editing.as_ref().unwrap().buf, "n");
        s.on_key(key(KeyCode::Esc));
        // in the profile list it makes a profile
        s.back_to_list();
        s.on_key(key(KeyCode::Char('n')));
        assert!(matches!(
            s.editing.as_ref().unwrap().target,
            Target::Name { was: None }
        ));
    }

    #[test]
    fn a_new_profile_starts_from_a_preset() {
        let mut s = screen(&["local"]);
        s.on_key(key(KeyCode::Char('n')));
        for _ in 0..8 {
            s.on_key(key(KeyCode::Backspace));
        }
        typed(&mut s, "claude");
        s.on_key(key(KeyCode::Enter));
        // named, and now asked what it talks to
        assert!(matches!(s.ask, Some(Ask::Preset { .. })), "no preset ask");
        assert!(render(&s).contains("anthropic"), "{}", render(&s));
        s.on_key(key(KeyCode::Enter));
        let p = s.store.get("claude");
        assert_eq!(p.base_url.as_deref(), Some("https://api.anthropic.com/v1"));
        assert_eq!(p.api, Some(crate::config::Api::Anthropic));
        assert_eq!(p.context_window, Some(200_000));
        // and it lands on the one thing the preset cannot know
        assert_eq!(s.field(), Some(Model));
    }

    #[test]
    fn space_cycles_a_toggle_and_never_types_into_it() {
        let mut s = screen(&["local"]);
        settings(&mut s, Api);
        assert_eq!(s.store.get("local").api, None);
        s.on_key(key(KeyCode::Char(' ')));
        assert_eq!(s.store.get("local").api, Some(crate::config::Api::Openai));
        s.on_key(key(KeyCode::Char('x')));
        assert!(s.editing.is_none(), "a toggle must not open an editor");
    }

    #[test]
    fn leaving_with_changes_asks_what_to_do() {
        let mut s = screen(&["local"]);
        s.dirty = true;
        assert!(matches!(s.on_key(key(KeyCode::Esc)), ConfigAction::Stay));
        assert!(render(&s).contains("discard"), "{}", render(&s));
        assert!(matches!(
            s.on_key(key(KeyCode::Char('n'))),
            ConfigAction::Close
        ));
    }

    #[test]
    fn delete_asks_first() {
        let mut s = screen(&["local", "other"]);
        s.on_key(key(KeyCode::Char('d')));
        assert!(render(&s).contains("delete local"), "{}", render(&s));
        s.on_key(key(KeyCode::Char('n')));
        assert!(s.store.profiles.contains_key("local"));
        s.on_key(key(KeyCode::Char('d')));
        s.on_key(key(KeyCode::Char('y')));
        assert!(!s.store.profiles.contains_key("local"));
        assert_eq!(s.store.active, None, "the active profile went with it");
    }

    #[test]
    fn a_bad_value_keeps_what_was_typed() {
        let mut s = screen(&["local"]);
        settings(&mut s, ContextWindow);
        typed(&mut s, "lots");
        s.on_key(key(KeyCode::Enter));
        assert_eq!(s.store.get("local").context_window, None);
        assert!(
            s.status.as_ref().unwrap().1,
            "should be flagged as an error"
        );
        assert!(s.editing.is_some(), "the editor closed on a bad value");
        assert!(render(&s).contains("whole number"));
    }

    #[test]
    fn saving_only_restarts_the_session_when_the_live_profile_moved() {
        // applied() rather than save(), which would write the real file
        let mut s = screen(&["local", "other"]);
        Model
            .set(s.store.profiles.get_mut("other").unwrap(), "m")
            .unwrap();
        assert!(matches!(s.applied(), ConfigAction::Stay));
        Model
            .set(s.store.profiles.get_mut("local").unwrap(), "m2")
            .unwrap();
        match s.applied() {
            ConfigAction::Apply(name, cfg) => {
                assert_eq!(name, "local");
                assert_eq!(cfg.model.as_deref(), Some("m2"));
            }
            _ => panic!("the running profile changed and was not applied"),
        }
    }
}
