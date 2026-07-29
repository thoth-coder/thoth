# Usage

## Writing a message

- `@path` attaches a file to your message. `tab` completes the path, and a
  file attached whole counts as read, so the model can edit it right away.
  Works in `-p` mode too: `thoth -p "explain @src/main.rs"`.
- `!command` runs a command yourself. Its output goes into the conversation
  as context without spending a model turn, which is the cheap way to show
  the agent a build error or a git log.
- `thoth --continue` (or `-c`) picks up this project's previous
  conversation, tools and all.

## Keys

- `enter` sends. While the agent is working, messages queue up.
- `tab` completes an `@path`.
- `esc` interrupts generation, or clears the input when idle.
- `up` / `down` move through input history.
- Mouse wheel, `pgup` / `pgdn` scroll the transcript. Hold `shift` while
  dragging to select text.
- `ctrl+o` expands or collapses long tool outputs.
- `y` / `a` / `n` answer permission prompts: once, always, deny.
- `ctrl+c` quits.

## Slash commands

| command | effect |
|---|---|
| `/clear` | reset the conversation (the system prompt is rebuilt) |
| `/compact` | summarize the conversation to free context space |
| `/recap` | load the previous session's summary into context |
| `/memory` | show project memory, `/memory clear` wipes it |
| `/allow` | list what is always allowed here, `/allow reset` clears it |
| `/status` | model, server, tokens, uptime |
| `/init` | analyze the project and generate THOTH.md |
| `/model NAME` | switch model, `/models` lists what the server has |
| `/help`, `/quit` | help and exit |

## Permissions

Writing files, editing them, shell commands, `web_fetch`, `remember` and
reading a file from outside the working directory all ask first. Reads
inside the project do not.

Answering `a` (always) is scoped, not a blanket grant: for shell it allows
that one program (`cargo`, `git`, ...), for `web_fetch` that one host. The
grant is saved in `~/.thoth/projects/<key>/allow.json` and survives
restarts, so review it with `/allow` now and then. Auto-approved actions
still print the full command line and the full diff.

## Editor integration

Install [thoth-for-vscode](https://github.com/thoth-coder/thoth-for-vscode)
and thoth knows your active file, the selected lines and text, and the
Problems panel. That context is attached to each request, and the model can
also call a `problems` tool after editing to see if errors cleared. It works
through small state files in `~/.thoth/ide/`; nothing is sent anywhere.

Without the extension, on Windows, thoth falls back to reading editor window
titles. That only reveals which file is open, not selections or diagnostics.

The status bar shows `ctx 12.3k/32.8k (37%)` (context used / window size),
`out` (tokens generated this session) and the current editor file. While the
model works, the line above the input shows a spinner and the elapsed time.

## Memory and recap

`.thoth/memory.md` lives in the project. The model saves durable facts there
with its `remember` tool (conventions, decisions, gotchas) and they are
loaded into the system prompt every session. Commit the file or gitignore
`.thoth/`, whichever fits your repo.

`~/.thoth/projects/<key>/` lives in your home directory, like Claude Code's
`~/.claude/projects`, and never touches the repo. It holds
`last-session.md` (a summary written on every compact, loaded with
`/recap`), `session.json` (the full transcript for `--continue`) and
`allow.json` (the permission allowlist). The transcript contains whatever
the tools printed, so treat it as you would your shell history; delete the
directory to wipe it.

## Context management

thoth auto-compacts at 2/3 of the window (when the window size is known,
i.e. Ollama). If generation hits the limit mid-answer it compacts and
continues. Identical repeated tool calls get blocked. After 40 agent steps
it pauses and asks you to say "continue"; raise `max_turns` in the config if
your tasks routinely need more.

## Project instructions

At startup the system prompt includes a scan of the working directory
(top-level files plus detected stack, e.g. Bun/TypeScript/Rust) and your
project instruction file: `THOTH.md` (what `/init` generates), `AGENTS.md`,
or `CLAUDE.md`, whichever exists first. If the file is just a short pointer
to one of the others, thoth follows it.
