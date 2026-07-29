# Usage

## Keys

- `enter` sends. While the agent is working, messages queue up.
- `esc` interrupts generation, or clears the input when idle.
- `up` / `down` move through input history.
- Mouse wheel, `pgup` / `pgdn` scroll the transcript. Hold `shift` while
  dragging to select text.
- `ctrl+o` expands or collapses long tool outputs.
- `y` / `a` / `n` answer permission prompts: once, always for this tool,
  deny.
- `ctrl+c` quits.

## Slash commands

| command | effect |
|---|---|
| `/clear` | reset the conversation (the system prompt is rebuilt) |
| `/compact` | summarize the conversation to free context space |
| `/recap` | load the previous session's summary into context |
| `/memory` | show project memory, `/memory clear` wipes it |
| `/status` | model, server, tokens, uptime |
| `/init` | analyze the project and generate THOTH.md |
| `/model NAME` | switch model, `/models` lists what the server has |
| `/help`, `/quit` | help and exit |

## Editor integration

Install [thoth-for-vscode](https://github.com/thoth-coder/thoth-for-vscode)
and thoth knows your active file, the selected lines and text, and the
Problems panel. That context is attached to each request, and the model can
also call a `problems` tool after editing to see if errors cleared. It works
through small state files in `~/.thoth/ide/`; nothing is sent anywhere.

Without the extension, on Windows, thoth falls back to reading editor window
titles. That only reveals which file is open, not selections or diagnostics.

The status bar shows `ctx 12.3k/32k` (context used / window size), `out`
(tokens generated this session) and the current editor file. The input box
title shows a timer while the model is working.

## Memory and recap

`.thoth/memory.md` lives in the project. The model saves durable facts there
with its `remember` tool (conventions, decisions, gotchas) and they are
loaded into the system prompt every session. Commit the file or gitignore
`.thoth/`, whichever fits your repo.

`~/.thoth/projects/<encoded-path>/last-session.md` lives in your home
directory, like Claude Code's `~/.claude/projects`. A summary of the
conversation is written there every time a compact happens. Load it into a
new session with `/recap`. It never touches the repo.

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
