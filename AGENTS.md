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

## Architecture (src/)

- `main.rs`: CLI args and subcommands, config resolution, model discovery,
  spawns the agent task, picks TUI or one-shot print mode (`-p`).
- `config.rs`: named profiles in `~/.thoth/config.toml` plus the resolution
  order: CLI flag, then `THOTH_*` env var, then the active profile, then the
  built-in default. Owns `thoth_home()`, the one definition of `~/.thoth`
  that session state and the editor bridge also use. Reads the pre-0.3 file
  (flat, in the platform config dir) as a profile called `default`. A new setting means all of `Config`, `Profile`,
  `resolve`, the field list in `ui/config.rs` and `docs/configuration.md`.
- `client.rs`: LLM transport. Three paths: OpenAI-compatible SSE
  (`/chat/completions`), Ollama native (`/api/chat`, which allows setting
  the context window per request) and Anthropic native (`/v1/messages`,
  which allows prompt caching). `auto` picks by url, and only probes
  `/api/version` on a local address: a paid endpoint must never see a
  request to a path we guessed. Streams content, thinking and tool-call
  deltas. `ThinkFilter` routes inline `<think>` tags to reasoning.
- `agent/mod.rs`: the agentic loop (model call, tool calls, results, repeat).
  Owns conversation history, permission gating, duplicate-call breaker,
  auto-compact at 2/3 of the window, truncation recovery, /compact, /recap,
  editor-context injection, `!command` runs.
- `agent/prompt.rs`: system prompt. Environment (cwd, os, date, git branch),
  project scan, guardrail rules, instruction file (THOTH.md/AGENTS.md/
  CLAUDE.md, pointers followed), project memory.
- `agent/session.rs`: per-project state under
  `~/.thoth/projects/<key>/`: saved transcript for `--continue` and the
  persistent permission allowlist. Written atomically, owner-only on unix.
- `tools/`: `fs.rs` (read/write/edit, read-coverage registry, unified
  diffs), `search.rs` (grep with optional context lines), `shell.rs`
  (foreground with timeout, background mode), `web.rs` (DuckDuckGo search,
  html to text), `memory.rs` (project memory, session recap, project key),
  `mod.rs` (tool schemas, dispatch, permission rules).
- `editor.rs`: VS Code awareness. Reads state files written by the
  companion extension (thoth-for-vscode) from `~/.thoth/ide/`, falls back
  to window titles on Windows.
- `ui/mod.rs`: ratatui interface. Transcript with an incremental wrap cache,
  streaming, permission prompts, input history, mouse scroll, ctrl+o.
  `ui/render.rs` turns blocks into styled lines, `ui/theme.rs` holds colors
  and glyphs, `ui/input.rs` handles `@path` attachments and completion.
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
- Anything that touches the user's machine must be visible in the UI: full
  command lines, full diffs, even when auto-approved.
- "Always allow" is scoped, never a blanket grant: shell is keyed by
  program (`shell:cargo`), web_fetch by host. See `permission_key` in
  `tools/mod.rs`.
- Tool output sizes are budgeted for 16-32k token windows. Keep the caps in
  `tools/mod.rs` and `tools/fs.rs` in that spirit.
- Anything parsed from the network or from a file can be hostile: no
  slicing by byte offset, no unvalidated value in a filesystem path.
- Prefer runtime `cfg!(windows)` over `#[cfg]` so both branches compile on
  every platform. `#[cfg]` items need a non-Windows stub.
- No new heavyweight dependencies without a good reason.
