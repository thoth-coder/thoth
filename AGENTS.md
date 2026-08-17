# Thoth

Agentic coding assistant (TUI). Talks to self-hosted models through Ollama
or any OpenAI-compatible server, and to hosted apis (Anthropic natively,
OpenAI/Google/OpenRouter over the OpenAI api). Rust, edition 2024.

## Build and test

```sh
cargo build                 # debug build
cargo build --release       # -> target/release/thoth
cargo fmt                   # rustfmt defaults, no config
cargo test                  # unit tests
cargo test -- --ignored     # network tests (need internet)
cargo clippy -- -D warnings # CI enforces zero warnings
```

Stable Rust 1.85+ (edition 2024). CI runs `build --release`, `test` and
`clippy` on Linux, macOS and Windows, so a `cfg!(windows)` branch that only
compiles on one side fails the matrix.

Tests live in a `#[cfg(test)] mod tests` at the bottom of the file they
cover. There is no `tests/` directory, don't add one.

A file past roughly a thousand lines is a sign it holds two things: split it
into a module directory (`client/`) or a sibling (`ui/screen.rs`) rather than
letting it grow. A child module sees its parent's private items, so a split
along those lines needs no new `pub`.

## Architecture (src/)

- `main.rs`: CLI args and subcommands, config resolution, model discovery,
  spawns the agent task, picks TUI or one-shot print mode (`-p`).
- `config.rs`: named profiles in `~/.thoth/config.toml` plus the resolution
  order: CLI flag, then `THOTH_*` env var, then the active profile, then the
  built-in default. Owns `thoth_home()`, the one definition of `~/.thoth`
  that session state and the editor bridge also use. Reads the pre-0.3 file
  (flat, in the platform config dir) as a profile called `default`. A new setting means all of `Config`, `Profile`,
  `resolve`, the field list in `ui/config.rs` and `docs/configuration.md`.
- `client/`: LLM transport, one module per wire protocol. `mod.rs` holds
  the message shape, the `Client` and the transport choice: `auto` picks by
  url and only probes `/api/version` on a local address, because a paid
  endpoint must never see a request to a path we guessed. `openai.rs` is
  OpenAI-compatible SSE (`/chat/completions`), `ollama.rs` the native
  `/api/chat` (which takes the context window per request), `anthropic.rs`
  the native `/v1/messages` (which allows prompt caching). `stream.rs` is
  what both text protocols need: `ThinkFilter` routes inline `<think>` tags
  to reasoning, `ToolTextFilter` reads a tool call a model wrote as text,
  and `TextStream` is the pair of them, so neither transport grows its own
  copy.
- `agent/mod.rs`: the agentic loop (model call, tool calls, results, repeat).
  Owns conversation history, permission gating, duplicate-call breaker,
  auto-compact at 2/3 of the window, truncation recovery, /compact, /recap,
  editor-context injection, `!command` runs. Also keeps the history from
  growing copies of itself: an identical read-only call drops the older
  result (`drop_stale_copy`), and identical is the whole safety condition.
- `agent/prompt.rs`: system prompt. Environment (cwd, os, date, git branch),
  project scan, guardrail rules, instruction file (THOTH.md/AGENTS.md/
  CLAUDE.md, pointers followed), project memory.
- `agent/stack.rs`: what kind of project this is and what checks it. One
  table, one row per stack (marker files, extensions, the check command, the
  words that mean a check ran, and whether running the code already was that
  check). The prompt reads it to name the command, the agent loop reads it to
  notice code that was changed and never checked. Supporting another language
  is adding a row: nothing else in thoth may name a language or a tool.
- `agent/undo.rs`: what a file looked like before thoth changed it. Each
  request is one checkpoint under `~/.thoth/projects/<key>/undo/`; `/undo`
  puts the newest one back and leaves alone anything that changed since.
  Recording is armed by `main.rs`, so the file tools stay inert outside a
  real session (which also keeps the tests off the user's state).
- `agent/session.rs`: per-project state under
  `~/.thoth/projects/<key>/`: saved transcript for `--continue` and the
  persistent permission allowlist. Written atomically, owner-only on unix.
- `tools/`: `fs.rs` (read/write/edit/multi_edit/move/delete, read-coverage
  registry, unified diffs, line-ending alignment), `search.rs` (grep with
  optional context lines), `shell.rs` (foreground with timeout, background
  mode), `web.rs` (DuckDuckGo search, html to text), `memory.rs` (project
  memory, session recap, project key), `todo.rs` (the plan for the task),
  `mod.rs` (tool schemas, dispatch, permission rules, `output_cap`).
- `editor.rs`: VS Code awareness. Reads state files written by the
  companion extension (thoth-for-vscode) from `~/.thoth/ide/`, falls back
  to window titles on Windows.
- `ui/mod.rs`: ratatui interface. The state and everything that changes it:
  terminal and agent events, keys, slash commands, the transcript blocks.
  `ui/screen.rs` is the other half, the drawing, including the incremental
  wrap cache; it is a child module, so it reads `App`'s private fields
  without any of them being made public. `ui/render.rs` turns text into
  styled lines (markdown, diffs, wrapping), `ui/theme.rs` holds colors and
  glyphs, `ui/input.rs` handles `@path` attachments and completion.
- `ui/config.rs`: the profile screen. One struct drives both `thoth config`
  (its own event loop) and `/config` (the App holds it and forwards keys),
  so the two cannot drift. Saving hands back a `Config` that the agent
  applies to the live session.
- `upgrade.rs`: `thoth upgrade`, replacing the running binary with the
  latest verified GitHub release.

## Everything else

- `docs/`: the user-facing docs (getting-started, usage, configuration,
  tools). A new flag, tool or slash command is not done until the matching
  page says so.
- `CHANGELOG.md`: one line under Unreleased for anything user-visible.
- `scripts/`: install and uninstall, sh and PowerShell. They must stay in
  step with the release asset names in `.github/workflows/release.yml` and
  with `upgrade.rs`.
- `THOTH.md` and `CLAUDE.md` are pointers to this file. Keep them that way,
  put the content here.
- `note.txt` is gitignored personal notes. Leave it alone.

## Conventions

- Guardrails go in code, not in the prompt, whenever possible. See the
  read registry in `tools/fs.rs`: write_file needs every line of a file
  covered by earlier reads, not just "a read happened".
- thoth is not a TypeScript tool or a Rust tool. No language, framework,
  file extension or shell command belongs anywhere but a table: `tsc`,
  `cargo`, `php -l` and the rest live in `agent/stack.rs` and nowhere else.
  If a fix wants a rule about one language, the fix is a row plus a rule
  written in terms of the row's fields. Anything else is a stack that
  happens to be the one being tested that day, and it strands every other
  language the user works in.
- Anything that touches the user's machine must be visible in the UI: full
  command lines, full diffs, even when auto-approved.
- "Always allow" is scoped, never a blanket grant: shell is keyed by
  program (`shell:cargo`), web_fetch by host. See `permission_key` in
  `tools/mod.rs`.
- Context is the scarce resource. Everything sent on every request is
  budgeted against the window: `tools::output_cap` sizes tool results,
  `prompt::situational_rules` drops rules for things that are not there, and
  the instruction file is capped the same way. `cargo test prompt_budget --
  --ignored --nocapture` prints what a request costs before the conversation
  starts; check it after touching the prompt or the tool schemas.
- A tool that truncates its own output says what was cut and how to get the
  rest. Never let a caller blindly cut a result that had a footer.
- Anything parsed from the network or from a file can be hostile: no
  slicing by byte offset, no unvalidated value in a filesystem path.
- Prefer runtime `cfg!(windows)` over `#[cfg]` so both branches compile on
  every platform. `#[cfg]` items need a non-Windows stub.
- No new heavyweight dependencies without a good reason.
