//! Fake sessions, so the interface can be looked at without a model behind
//! it. `thoth --view` lists them, `thoth --view <name>` prints the frames.
//!
//! Most of thoth's screens only appear when something else happens first: a
//! permission prompt needs a tool that asks, the chooser needs the model to
//! call `ask_user`, the plan banner needs a whole plan-mode turn. Waiting for
//! a local model to produce those is minutes per look, which is why the bugs
//! in them survive. Here every state is one command away, and `every_view`
//! walks all of them on `cargo test`, so a panic in a screen nobody opened
//! today still fails the build.
//!
//! A child module of `ui`, so it can build an `App` and reach its fields
//! without any of them being made public.

use super::*;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

pub struct View {
    pub name: &'static str,
    pub about: &'static str,
}

pub const VIEWS: &[View] = &[
    View {
        name: "start",
        about: "the start screen, before anything has been said",
    },
    View {
        name: "chat",
        about: "a session mid-task: reasoning, tools, a diff, a note",
    },
    View {
        name: "permission",
        about: "a tool asking to run, with the command in full",
    },
    View {
        name: "choice",
        about: "the model's question: picking a row, then writing an answer",
    },
    View {
        name: "plan",
        about: "plan mode, and what it offers when the plan is ready",
    },
    View {
        name: "picker",
        about: "@path completion and /command completion",
    },
    View {
        name: "modes",
        about: "the status line in each of the four permission modes",
    },
    View {
        name: "narrow",
        about: "the whole thing at 46 columns, where wrapping goes wrong",
    },
    View {
        name: "config",
        about: "the profile screen (thoth config, /config)",
    },
    View {
        name: "stress",
        about: "content that fights the renderer: wide chars, tabs, escapes, no spaces",
    },
    View {
        name: "markdown",
        about: "headings, fences, tables, lists, inline code",
    },
    View {
        name: "nine",
        about: "the most options ask_user allows, on a screen with no room",
    },
    View {
        name: "empty",
        about: "everything absent: no profile, no window, no blocks, no text",
    },
];

/// One frame: a caption and the lines of the terminal.
struct Frame {
    caption: String,
    lines: Vec<String>,
}

pub fn render(name: &str, width: u16, height: u16) -> anyhow::Result<String> {
    let frames = match name {
        "start" => start(width, height),
        "chat" => chat(width, height),
        "permission" => permission(width, height),
        "choice" => choice(width, height),
        "plan" => plan(width, height),
        "picker" => picker(width, height),
        "modes" => modes(width, height),
        "narrow" => chat(46, height.max(20)),
        "config" => config(width, height),
        "stress" => stress(width, height),
        "markdown" => markdown(width, height),
        "nine" => nine(width, height),
        "empty" => empty(width, height),
        other => anyhow::bail!(
            "no view called {other}. try one of: {}",
            VIEWS.iter().map(|v| v.name).collect::<Vec<_>>().join(", ")
        ),
    };
    let mut out = String::new();
    for f in frames {
        out.push_str(&format!("\n{}\n", f.caption));
        for l in f.lines {
            out.push_str(&l);
            out.push('\n');
        }
    }
    Ok(out)
}

// ---- the fake session ----

fn app() -> App {
    // the channel is dropped on the spot: nothing is listening, and every
    // send in the App is already `let _ =`, because the agent can end first
    let (cmd_tx, rx) = mpsc::unbounded_channel();
    drop(rx);
    let mut a = App::new(
        Session {
            model: "qwen3.6:35b".into(),
            base_url: "http://localhost:11434/v1".into(),
            profile: Some("local".into()),
            api: "ollama native",
            window: Some(32_768),
        },
        cmd_tx,
        Arc::new(Mutex::new(CancellationToken::new())),
    );
    a.ctx_tokens = 15_400;
    a.out_tokens = 820;
    // whatever the developer has open in their editor must not wander into a
    // view: the point is that the same input draws the same screen
    a.editor_status = None;
    a
}

/// The transcript most of the views sit on top of.
fn working(a: &mut App) {
    a.blocks = vec![
        ChatBlock::User("เพิ่ม health check ใน server หน่อย".into()),
        ChatBlock::Reasoning(
            "the routes are in src/server.ts\nchecking what the project runs its tests with".into(),
        ),
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
                "src/server.ts\n  12   app.get(\"/\", home);\n\
                 + 13   app.get(\"/healthz\", () => new Response(\"ok\"));"
                    .into(),
            ),
            result: Some(("Edited src/server.ts".into(), false)),
        },
        ChatBlock::Tool {
            name: "shell".into(),
            summary: "bunx tsc --noEmit".into(),
            detail: Some("$ bunx tsc --noEmit".into()),
            result: Some(("(command succeeded, exit code 0, no output)".into(), false)),
        },
        ChatBlock::Info("attached src/server.ts (48 lines)".into()),
    ];
}

fn shot(a: &mut App, caption: &str, w: u16, h: u16) -> Frame {
    let mut term = Terminal::new(TestBackend::new(w, h)).expect("test backend");
    term.draw(|f| a.draw(f)).expect("draw");
    let buf = term.backend().buffer().clone();
    let lines = (0..h)
        .map(|y| {
            (0..w)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect();
    Frame {
        caption: caption.to_string(),
        lines,
    }
}

// ---- the views ----

fn start(w: u16, h: u16) -> Vec<Frame> {
    let mut a = app();
    // a session that has said nothing has spent nothing, and a start screen
    // quoting 15k of context is a screen thoth never draws
    a.ctx_tokens = 0;
    a.out_tokens = 0;
    vec![shot(&mut a, "nothing said yet", w, h)]
}

fn chat(w: u16, h: u16) -> Vec<Frame> {
    let mut a = app();
    working(&mut a);
    let idle = shot(&mut a, "a finished task", w, h);
    a.input = "run the tests".into();
    a.cursor = a.input.chars().count();
    let typed = shot(&mut a, "with a message half written", w, h);
    a.input = "explain what\nchanged, and why\nthe second call was needed".into();
    a.cursor = a.input.chars().count();
    let multi = shot(&mut a, "a multi-line message: the box grows to it", w, h);
    a.input.clear();
    a.cursor = 0;
    a.set_busy();
    let busy = shot(&mut a, "working", w, h);
    vec![idle, typed, multi, busy]
}

fn permission(w: u16, h: u16) -> Vec<Frame> {
    let mut a = app();
    working(&mut a);
    let (tx, rx) = oneshot::channel();
    drop(rx);
    // the header comes first in a real run, and the preview hangs under it:
    // a view that skips it would draw a screen thoth never shows
    a.on_agent_event(AgentEvent::ToolStart {
        name: "shell".into(),
        summary: "rm -rf build/ && bun run build".into(),
    });
    a.on_agent_event(AgentEvent::Permission {
        tool: "shell".into(),
        preview: "$ rm -rf build/ && bun run build".into(),
        reply: tx,
    });
    vec![shot(&mut a, "a command waiting on an answer", w, h)]
}

fn choice(w: u16, h: u16) -> Vec<Frame> {
    let plain = |code| KeyEvent::new(code, KeyModifiers::NONE);
    let mut a = app();
    a.blocks = vec![ChatBlock::User("จัดโครงสร้างโปรเจกต์ใหม่ให้หน่อย".into())];
    let (tx, rx) = oneshot::channel();
    drop(rx);
    a.on_agent_event(AgentEvent::Choice {
        question: "โครงสร้างแบบไหนดีครับ".into(),
        options: vec![
            "src/routes/todos.ts + src/db.ts, one file per resource".into(),
            "keep index.ts, move only the handlers to src/handlers/".into(),
            "controllers / services / models".into(),
        ],
        reply: tx,
    });
    let first = shot(&mut a, "the question, first row highlighted", w, h);
    a.on_key(plain(KeyCode::Down));
    let moved = shot(&mut a, "after one down", w, h);
    a.on_key(plain(KeyCode::Up));
    a.on_key(plain(KeyCode::Up));
    let last = shot(&mut a, "wrapped up onto the write-your-own row", w, h);
    a.on_key(plain(KeyCode::Enter));
    a.input = "แยกแค่ route ออกไป ที่เหลือไว้เหมือนเดิม".into();
    a.cursor = a.input.chars().count();
    let writing = shot(&mut a, "writing an answer it did not offer", w, h);
    vec![first, moved, last, writing]
}

fn plan(w: u16, h: u16) -> Vec<Frame> {
    let mut a = app();
    working(&mut a);
    a.set_mode(PermMode::Plan);
    let planning = shot(&mut a, "plan mode: nothing is changed", w, h);
    a.on_agent_event(AgentEvent::PlanReady);
    vec![planning, shot(&mut a, "the plan is ready", w, h)]
}

fn picker(w: u16, h: u16) -> Vec<Frame> {
    let mut a = app();
    working(&mut a);
    a.input = "look at @src".into();
    a.cursor = a.input.chars().count();
    a.refresh_picker();
    let paths = shot(&mut a, "@path completion", w, h);
    a.input = "/m".into();
    a.cursor = 2;
    a.refresh_picker();
    let cmds = shot(&mut a, "/command completion", w, h);
    a.input = "/".into();
    a.cursor = 1;
    a.refresh_picker();
    vec![paths, cmds, shot(&mut a, "every command", w, h)]
}

fn modes(w: u16, h: u16) -> Vec<Frame> {
    let mut out = Vec::new();
    for m in [
        PermMode::Manual,
        PermMode::AcceptEdits,
        PermMode::Auto,
        PermMode::Plan,
    ] {
        let mut a = app();
        working(&mut a);
        a.set_mode(m);
        out.push(shot(&mut a, m.name(), w, h.min(12)));
    }
    out
}

/// Everything that has ever made a terminal draw the wrong thing, in one
/// transcript. None of it is unusual: a minified bundle is one long token, a
/// stack trace is full of tabs, and any program with colours puts escapes in
/// its output.
fn stress(w: u16, h: u16) -> Vec<Frame> {
    let mut a = app();
    a.blocks = vec![
        ChatBlock::User(format!("why does {} fail", "x".repeat(300))),
        // a wide char takes two cells: cutting between them leaves half a
        // character on the screen and everything after it one column out
        ChatBlock::Assistant(
            "ตัวอักษรไทยสระบนสระล่าง กิ่งไม้ ปู่ย่า, 日本語と漢字, 한국어, emoji 🚀🔥👨‍👩‍👧‍👦 and \
             combining a\u{0301}e\u{0301}i\u{0301}"
                .into(),
        ),
        ChatBlock::Tool {
            name: "shell".into(),
            summary: "cargo test".into(),
            detail: Some("$ cargo test".into()),
            // tabs, an ansi colour sequence, a bare carriage return and a
            // bell: all of them arrive from real programs
            result: Some((
                "\tat main.rs:12\n\u{1b}[31merror\u{1b}[0m: failed\r\nprogress\u{7}done\n\
                 no-spaces-at-all-in-this-line-which-is-also-very-long-and-cannot-be-broken-anywhere"
                    .into(),
                true,
            )),
        },
        ChatBlock::Tool {
            name: "read_file".into(),
            summary: "a/very/deep/path/that/keeps/going/on/and/on/src/components/Button.tsx".into(),
            detail: None,
            result: Some((("one line. ".repeat(200)).to_string(), false)),
        },
        ChatBlock::Diff(
            "src/x.rs\n- 1   let a = 1;\n+ 1   let a = 2;\n+ 2   // ตรงนี้เพิ่มมา".into(),
        ),
        ChatBlock::Error("failed: \tconnection reset\u{1b}[0m".into()),
        // nothing at all, which a stream that dies early leaves behind
        ChatBlock::Assistant(String::new()),
        ChatBlock::Info(String::new()),
    ];
    let plain = shot(&mut a, "hostile content", w, h);
    a.expanded = true;
    let open = shot(&mut a, "the same with ctrl+o expanded", w, h);
    a.expanded = false;
    a.input = format!("@{} and ตัวอักษรไทย", "src/".repeat(30));
    a.cursor = a.input.chars().count();
    vec![
        plain,
        open,
        shot(&mut a, "an input longer than the box", w, h),
    ]
}

fn markdown(w: u16, h: u16) -> Vec<Frame> {
    let mut a = app();
    a.blocks = vec![
        ChatBlock::User("อธิบายหน่อย".into()),
        ChatBlock::Assistant(
            "# Heading\n\
             Some **bold** and *italic* and `inline_code()` in one line.\n\n\
             ## Lists\n\
             - first\n\
             - second with a `path/to/file.rs:41` and enough words after it \
             that the line has to wrap, which is where a list marker either \
             holds its column or does not\n\
             \x20 - nested\n\
             \x20   - nested deeper\n\
             1. numbered\n\
             2. also numbered\n\n\
             ```rust\n\
             fn main() {\n    println!(\"hello\");\n}\n\
             ```\n\n\
             | column | another | a third one that is quite wide |\n\
             |---|---|---|\n\
             | a | b | c |\n\n\
             > a quote\n\n\
             a [link](https://example.com) and a bare https://example.com/very/long/path\n\
             ---\n\
             an unclosed fence follows\n\
             ```\n\
             still inside it"
                .into(),
        ),
    ];
    vec![shot(
        &mut a,
        "markdown, including a fence nobody closed",
        w,
        h,
    )]
}

/// Nine options is what `ask_user` allows, so ten rows is what the chooser
/// can want. On a short terminal that is more than the whole screen.
fn nine(w: u16, h: u16) -> Vec<Frame> {
    let mut a = app();
    working(&mut a);
    let (tx, rx) = oneshot::channel();
    drop(rx);
    a.on_agent_event(AgentEvent::Choice {
        question: "which one".into(),
        options: (1..=9)
            .map(|i| format!("option number {i}, described at some length"))
            .collect(),
        reply: tx,
    });
    let tall = shot(&mut a, "room for all of them", w, h.max(24));
    let short = shot(&mut a, "and not enough room", w, h.min(12));
    for _ in 0..8 {
        a.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    }
    vec![
        tall,
        short,
        shot(&mut a, "walked down past the bottom of it", w, h.min(12)),
    ]
}

/// The other end: a session that knows nothing about itself. No profile, no
/// context window to measure against, nothing said, nothing typed.
fn empty(w: u16, h: u16) -> Vec<Frame> {
    let (cmd_tx, rx) = mpsc::unbounded_channel();
    drop(rx);
    let mut a = App::new(
        Session {
            model: String::new(),
            base_url: String::new(),
            profile: None,
            api: "",
            window: None,
        },
        cmd_tx,
        Arc::new(Mutex::new(CancellationToken::new())),
    );
    a.editor_status = None;
    a.blocks.clear();
    vec![shot(&mut a, "nothing known and nothing said", w, h)]
}

fn config(w: u16, h: u16) -> Vec<Frame> {
    let mut a = app();
    a.command("config");
    vec![shot(&mut a, "the profile screen", w, h.max(20))]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every view at every size worth being afraid of. It is the only test
    /// some of these screens have, and a panic in one is a panic a user would
    /// have hit; the sizes are where the arithmetic goes wrong, not where
    /// people work.
    #[test]
    fn every_view_draws() {
        const SIZES: [(u16, u16); 8] = [
            (100, 30), // a normal terminal
            (200, 60), // a large one
            (40, 10),  // small
            (20, 6),   // the smallest --view will accept
            (46, 20),  // narrow and tall
            (160, 7),  // wide and short: fewer rows than the chrome wants
            (100, 8),  // shorter than a nine-option chooser needs
            (31, 9),   // odd, to catch anything that assumes even widths
        ];
        for v in VIEWS {
            for (w, h) in SIZES {
                let out =
                    render(v.name, w, h).unwrap_or_else(|e| panic!("{} at {w}x{h}: {e}", v.name));
                assert!(!out.trim().is_empty(), "{} drew nothing at {w}x{h}", v.name);
            }
        }
    }

    #[test]
    fn an_unknown_view_says_what_there_is() {
        let e = render("nope", 80, 24).unwrap_err().to_string();
        assert!(e.contains("no view called nope"), "{e}");
        assert!(e.contains("choice"), "it has to list them: {e}");
    }
}
