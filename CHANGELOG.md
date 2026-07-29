# Changelog

All notable changes to thoth are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

## [0.1.0] - 2026-07-29

### Added
- Agentic loop over local LLMs: OpenAI-compatible SSE transport plus native
  Ollama `/api/chat` transport (auto-detected) with per-request `num_ctx`
  (default 32768) and separated thinking output.
- Tools: `read_file`, `write_file`, `edit_file`, `list_dir`, `glob`, `grep`,
  `shell` (foreground with timeout, or `background=true` with pid + log
  file), `web_search` (DuckDuckGo, optional Google CSE with fallback),
  `web_fetch`, `problems` (live IDE diagnostics), `remember`.
- ratatui TUI: streaming transcript with incremental wrap cache, markdown
  rendering, collapsible reasoning, unified diffs with line numbers, full
  command-line display, permission prompts (yes / always / no), input
  history, message queueing while busy, mouse-wheel scroll, ctrl+o
  expand/collapse, live token counters (`ctx x/y | out n`), turn timer,
  editor status ("In file.rs, 7 lines selected").
- Guardrails enforced in code: read-before-edit, full-read-before-overwrite,
  duplicate-tool-call breaker, output size caps.
- Context management: auto-compact at 2/3 of the window, context-limit
  recovery (compact and continue), `/compact`, `/clear` with system-prompt
  rebuild, max-turns safety pause.
- Memory and recap: project memory in `.thoth/memory.md` (via `remember`,
  loaded into the system prompt), session recap in
  `~/.thoth/projects/<encoded-path>/last-session.md`, `/recap`, `/memory`.
- Project awareness: startup scan (top-level files + stack detection),
  instruction files (`THOTH.md` > `AGENTS.md` > `CLAUDE.md`, short pointer
  files followed automatically), `/init` generates THOTH.md.
- VS Code integration via the companion extension (thoth-for-vscode):
  active file, selection with line numbers, and Problems injected as
  context; Windows window-title fallback without the extension.
- One-shot mode (`thoth -p "..."`), `/status`, `/model`, `/models`,
  config file + env vars + CLI flags, GitHub Actions CI (Linux/macOS/
  Windows), rustls TLS (no OpenSSL requirement).
- Release automation: tag-triggered workflow building binaries for five
  targets (Linux x86_64/arm64 musl, macOS Intel/Apple Silicon, Windows),
  with install/uninstall scripts for curl | sh and irm | iex.

[0.1.0]: https://github.com/thoth-coder/thoth/releases/tag/v0.1.0
