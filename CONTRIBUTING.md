# Contributing

## Setup

You need stable Rust 1.85+ (edition 2024). `cargo build` and `cargo test`
should pass out of the box.

For end-to-end testing you need a local model server:

- Ollama: `ollama pull qwen3:8b`, then just run `thoth`
- llama.cpp: `llama-server -m model.gguf --jinja -c 16384`

Network tests are opt-in: `cargo test -- --ignored` (they hit DuckDuckGo and
example.com).

## Working on the interface

Most of thoth's screens only appear once something else has happened: a
permission prompt needs a tool that asks, the chooser needs the model to call
`ask_user`, the plan banner needs a whole plan-mode turn. Waiting on a local
model for each look is minutes per change, which is why bugs live in those
screens. So a debug build draws any of them on demand:

```sh
cargo run -- --view                          # what there is
cargo run -- --view choice                   # every frame of that screen
cargo run -- --view chat --view-size 46x20   # and at a size that hurts
```

The output is plain text, so it can be read, diffed and pasted into an issue.
`cargo test every_view_draws` walks all of them at two sizes, which is the
only test some of those screens have: a panic in one nobody opened today
still fails the build. A new screen means a new view in `src/ui/demo.rs`.
Views are refused in a release build.

## Before opening a PR

- `cargo fmt`, then `cargo clippy -- -D warnings` passes. CI fails on any
  warning.
- `cargo test` passes. Run the ignored tests too if you touched
  `tools/web.rs`.
- If you changed agent behavior, try it against a real local model at least
  once (`thoth -p "..."` in a scratch project) and say what you ran in the
  PR description.
- Add a line to `CHANGELOG.md` under Unreleased.

## Ground rules

Guardrails live in code, not in the prompt. If something must hold (read
before write, permission gating), enforce it in Rust.

Everything the agent does to the user's machine has to be visible: full
shell command lines, full diffs, even when auto-approved.

Everything sent on every request is budgeted against the context window,
not against a number someone once picked. Don't add an unbounded output
path, and a tool that cuts its own output has to say what it cut and how to
get the rest.

Windows, Linux and macOS all matter. Prefer runtime `cfg!(windows)` branches
so both sides get compile-checked everywhere.

## Related

The VS Code extension lives at
[thoth-for-vscode](https://github.com/thoth-coder/thoth-for-vscode). It
writes the editor state (active file, selection, diagnostics) that thoth
reads.

## License

Contributions are dual-licensed under Apache-2.0 and MIT, same as the
project.
