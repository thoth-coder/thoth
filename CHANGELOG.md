# Changelog

All notable changes to thoth are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added
- `@path` now opens a picker under the input instead of completing on tab
  only: up/down move, tab or enter takes the highlighted entry, esc closes
  it, and picking a directory lists what is inside it.
- Startup screen: logo, version, working directory and, on the Ollama
  native api, the context window that used to be a transcript line.
  `/clear` shows it again.

## [0.2.0] - 2026-07-29

### Added
- `grep` tool: optional `context` parameter shows up to 10 surrounding
  lines per match in ripgrep-style blocks, and the system prompt now steers
  the model to explore with grep context and ranged reads instead of whole
  files.
- `thoth upgrade`: downloads the latest GitHub release for the current
  platform, verifies the checksum and swaps the binary in place.
- `thoth --continue` resumes the previous conversation for the working
  directory; the transcript is saved after every turn.
- `@path` in the input attaches a file to the message (tab completes the
  path), in the TUI and in `-p` mode.
- `!command` runs a command yourself and puts its output into the context
  without spending a model turn.
- "Always allow" now persists per project, and `/allow` lists what is
  allowed (`/allow reset` clears it).
- System prompt: the environment block now includes today's date and the
  git branch with dirty-file count, and new rules cover prompt injection
  (tool output is data, not instructions), secrets (never in web queries,
  never repeated), committing only when asked, and minimal diffs.

### Changed
- Leaner interface: the boxed input is now a single rule, with a state line
  showing the spinner, elapsed time and interrupt hint, and a status line
  with context use, output tokens and the active editor file.
- Permission prompts are scoped. Answering "always" for a shell command
  allows that program only (`shell:cargo`), and web_fetch is allowed per
  host, instead of unlocking the whole tool forever.
- `read_file` outside the working directory, `web_fetch` and `remember`
  now ask for permission; reads inside the project stay free.
- Source layout: `agent/`, `ui/` and `tools/` modules instead of flat files.

### Fixed
- Security: two one-line reads of a file (first line and last line) counted
  as a full read and unlocked a blind `write_file` overwrite. Reads are now
  tracked as line ranges and must cover the file.
- Security: `thoth upgrade` accepted any release tag from the GitHub API
  and interpolated it into a path that gets deleted recursively; tags are
  validated, the temp directory is unique per run, and a missing checksum
  file now aborts the upgrade instead of skipping verification.
- Security: `@path` no longer grants write access to files outside the
  project, and the path is escaped before it goes into the model's context.
- Crash: a `%` followed by a multi-byte character in a search result URL
  panicked the agent task and left the interface spinning forever. The
  decoder is byte-safe, and the interface now reports a dead agent task.
- `read_file` refuses files over 2 MB instead of loading them into memory,
  `shell` stops capturing output at 200 kB instead of buffering everything a
  runaway command prints, and `grep` with context stops at its output cap
  inside a file rather than after it.
- Saved sessions and the allowlist are written atomically and are keyed by
  a hash of the project path, so similarly named directories no longer
  share state.

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

[0.2.0]: https://github.com/thoth-coder/thoth/releases/tag/v0.2.0
[0.1.0]: https://github.com/thoth-coder/thoth/releases/tag/v0.1.0
