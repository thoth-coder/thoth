# Thoth

Agentic coding assistant (TUI) for self-hosted LLMs via Ollama or any
OpenAI-compatible server. Rust, edition 2024.

## Build and test

```sh
cargo build                 # debug build
cargo build --release       # -> target/release/thoth
cargo test                  # unit tests
cargo test -- --ignored     # network tests (need internet)
cargo clippy -- -D warnings # CI enforces zero warnings
```

## Architecture (src/)

- `main.rs`: CLI args, config resolution, model discovery, spawns the agent
  task, picks TUI or one-shot print mode (`-p`).
- `client.rs`: LLM transport. Two paths: OpenAI-compatible SSE
  (`/chat/completions`) and Ollama native (`/api/chat`, auto-detected via
  `/api/version`) which allows setting `num_ctx` per request. Streams
  content, thinking and tool-call deltas. `ThinkFilter` routes inline
  `<think>` tags to reasoning.
- `agent.rs`: the agentic loop (model call, tool calls, results, repeat).
  Owns conversation history, permission gating, duplicate-call breaker,
  auto-compact at 2/3 of the window, truncation recovery, /compact, /recap,
  editor-context injection.
- `tools/`: `fs.rs` (read/write/edit, read-before-write registry, unified
  diffs), `search.rs` (grep), `shell.rs` (foreground with timeout,
  background mode), `web.rs` (DuckDuckGo or Google CSE, html to text),
  `memory.rs` (project memory, session recap), `mod.rs` (tool schemas,
  dispatch, permission table).
- `editor.rs`: VS Code awareness. Reads state files written by the
  companion extension (thoth-for-vscode) from `~/.thoth/ide/`, falls back
  to window titles on Windows.
- `tui.rs`: ratatui interface. Transcript with an incremental wrap cache,
  streaming, permission prompts, input history, mouse scroll, ctrl+o.
- `prompt.rs`: system prompt. Environment, project scan, guardrail rules,
  instruction file (THOTH.md/AGENTS.md/CLAUDE.md, pointers followed),
  project memory.

## Conventions

- Guardrails go in code, not in the prompt, whenever possible. See the
  read registry in `tools/fs.rs`.
- Anything that touches the user's machine must be visible in the UI: full
  command lines, full diffs, even when auto-approved.
- Tool output sizes are budgeted for 16-32k token windows. Keep the caps in
  `tools/mod.rs` and `tools/fs.rs` in that spirit.
- Prefer runtime `cfg!(windows)` over `#[cfg]` so both branches compile on
  every platform. `#[cfg]` items need a non-Windows stub.
- No new heavyweight dependencies without a good reason.
