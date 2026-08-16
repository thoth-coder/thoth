# Changelog

All notable changes to thoth are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added
- Four modes for how much thoth asks before it acts, cycled with
  `shift+tab`, named with `/mode`, and set for one run with `--mode`:
  `manual` (the default, ask every time), `accept edits` (file changes go
  through, the shell and the network still ask), `auto` (nothing asks) and
  `plan` (nothing is changed at all). Accept-edits stops at the shell on
  purpose: a file change is a diff with an undo behind it, a command is
  neither. The mode is never saved, so one turned on for a sandbox is not
  still on tomorrow against a real repository.
- Plan mode answers with the plan, then asks what to do with it: carry it
  out with edits accepted, carry it out asking each time, or keep planning.
  The first two switch the mode and send the plan back to be carried out,
  so there is nothing to retype. The tools that write are refused while it
  is on, and the refusal tells the model to describe the change instead.
- `shift+enter` starts a new line in the input instead of sending it, and
  the input grows to as many rows as the message has lines, up to ten.
  `alt+enter` and `ctrl+j` do the same for terminals that never deliver the
  first one, and a line ending in `\` breaks where even those do not get
  through. `up` and `down` walk the lines of a multi-line message, and are
  the history again when there is only one line. On unix thoth now asks the
  terminal for key disambiguation, without which shift+enter arrives as a
  plain enter; the windows console tells them apart on its own.
- `write_file` takes `append`, for adding a section to the end of a file.
  It needs no prior read, because nothing already in the file is touched.
  It is how a file too long for one call gets written: the first section
  normally, every one after it appended. Doing that with `edit_file` meant
  inventing a unique `old_string` out of the last lines of the file, and the
  last lines of a Rust file are `}` and `}`.

### Security
- Anything fetched from the network arrives wrapped in a line saying what it
  is: content someone else wrote, to be read and never obeyed, and to be
  reported if it asks for a command, a file change or for the instructions to
  be set aside. The rule was already in the system prompt, two thousand
  tokens before the page shows up; the warning that holds is the one next to
  the payload. Search results are wrapped too, since a title and a snippet
  are the cheapest place on the internet to put a sentence in front of
  somebody else's agent.

### Fixed
- A background process thoth started and the model never stopped is named at
  the end of the request. "Closing the server now" followed by the turn
  ending leaves it holding the port for the rest of the day, and whether it
  is still up is a question the operating system answers rather than
  something to take the model's word on.
- A tool call missing a field is told which tool and what a good call
  carries. serde said "missing field `old_string`" and stopped there, which
  names neither, and the model has to guess which of its calls went wrong.
  The schema already lists the required fields, so the error can too.
- Several files asked for in one turn no longer blow the window apart. Each
  tool result was capped on its own, so six read_file calls in one reply
  arrived as six full results and the request after them went over the
  window. A server does not complain about that: it drops the front of the
  prompt, which is the system prompt and everything agreed so far, and the
  model answers the last file with "what would you like me to do with this?".
  One turn's results now share one turn's room, and a result that runs out
  of it says where it was cut and to ask again. The same job in an 8k window
  went from losing the task entirely to finishing it across two compactions.
- Running the tests again after a fix is no longer blocked as a repeat. The
  loop breaker counts a command by its text, and `bun test` after an edit is
  the same text and a different answer; a model that fixed the bug and went
  to confirm it got "this exact shell call was already run 3 times, the
  result will not change". A change to any file now clears what was counted
  about commands, which is the one habit worth encouraging.
- A path that is not there says where the working directory is. "The system
  cannot find the path specified" cost three more calls and a `pwd` every
  time a model guessed an absolute path wrong.
- Adding a dependency counts as changing code for the note above: a version
  number written into a manifest and never resolved is the same broken
  handoff as code that was never built.
- A request that changed files and ran nothing says so. The prompt tells the
  model to build what it changed, and a model that skipped it still signs off
  with "build passed"; whether a command ran is not the model's word about
  itself, thoth watched every tool call the request made. It now prints a
  note, so "it builds" over an untouched compiler is contradicted on the
  spot.
- The preview of an `edit_file` with an empty `old_string` gives the reason
  the tool will give. It used to say "old_string not found in file", sending
  the reader to look for a typo in a string that is not there at all.
- The write cap does not shrink a hosted model to 170 lines a file. It used
  to be derived from the tool-result cap, which guesses small when a profile
  declares no `context_window`, on purpose: one grep must not eat the context
  of a server nobody described. A write is capped for the opposite reason,
  so an undeclared window now means 40k characters there, not 6k.
- A compaction in the middle of a request no longer leaves files unreadable.
  The loop breaker counts identical tool calls, and it kept counting across
  the compaction that had just thrown their results away, so a re-read of the
  file the model was working on came back "STOP: this exact read_file call was
  already run 4 times" with no way to get the content. It burned the rest of
  the turn going in circles.
- Answers come back in the language the request was written in. Local models
  read a system prompt full of English and answer in English (or in Chinese)
  whatever the style rule says, so a request in a non-Latin script now names
  the language outright. The name is remembered for the session, because the
  reminder rides on the user message and a compaction deletes every one of
  them: after the first compaction the whole conversation went back to
  English. The summary is asked for in the user's language too.
- "old_string appears 2 times" now says which lines they are. Telling a
  model its anchor is not unique and to "add surrounding context" leaves it
  guessing which of the matches it was aiming at; the line numbers let it
  pick the context in one turn instead of two.
- An `edit_file` with an empty `old_string` is answered with the tool that
  does what it was trying to do. An empty anchor matches between every pair
  of characters, so a model trying to add a `[[bin]]` section to a 7-line
  Cargo.toml got "old_string appears 61 times, add surrounding context to
  make it unique": true, and no help at all. It now says to use `write_file`
  with `append`.
- One edit in a `multi_edit` that asks for no change (old_string equal to
  new_string) is skipped and named in the result, instead of failing the
  whole call. Throwing away three real edits because the fourth had nothing
  in it cost a turn every time a model got one of four wrong. A call where
  every edit is like that is still an error.
- Reading a file again after changing it is no longer treated as going in
  circles. The loop breaker counts identical calls, so a model that read a
  file, edited it wrong, and read it back to see the damage was told "this
  exact read_file call was already run 3 times, the result will not change"
  while the file on disk said otherwise. A change to a file now clears what
  was counted about reads of it. Only the count: what makes the re-read
  supersede the copy taken before the change stays, because after a change
  that copy is not merely old, it is wrong, and leaving it would put both
  versions of the file in front of the model at once.
- Hitting the context limit mid-generation no longer costs two compactions.
  The recovery path compacted and jumped straight back to the model, past the
  point where the auto-compact request from that same turn is answered, so
  the next turn compacted again with four messages in the conversation and
  threw away the file it had just read.
- Long files are built up in sections instead of being lost. A model that
  tries to emit a whole 1200-line file in one write_file has to fit every
  character of it, and its reasoning, inside one generation; past the window
  it is cut off mid-file and all of it is gone. write_file now refuses a call
  larger than half of what a tool result may take and says how to write the
  file in parts, and the prompt asks for that shape up front.
- The model stopped deleting files to rewrite them. write_file already
  counts a file thoth wrote itself as read, so overwriting it was allowed all
  along; the prompt said only "write_file is for brand-new files", and a
  model that wanted to start over on a 1300-line file it had just written
  reached for delete_file instead.

### Changed
- Release notes are the changelog entry for the tag, put there by one
  publish job instead of by all five build jobs at once. Each of them was
  generating notes of its own, which is where the repeated
  "**Full Changelog**" lines in v0.3.0 came from.

## [0.3.0] - 2026-08-16

### Added
- Hosted apis are first-class. Anthropic gets a native transport
  (`/v1/messages`) with prompt caching on the system prompt, the tool
  schemas and the end of the history. OpenAI, Google's OpenAI-compatible
  endpoint, OpenRouter and the rest work through the existing OpenAI path.
  A profile picks the protocol with `api = "auto" | "openai" | "ollama" |
  "anthropic"`.
- Per-profile `headers`, for endpoints that do not take a bearer token
  (Azure's `api-key`, gateways with their own).
- Running cost. Set `price_in` / `price_out` / `price_cached` on a profile
  and the status bar, `/status` and `-p` show what the session has spent.
- `/models` numbers the list, and `/model 3` picks from it.
- `multi_edit`: several replacements in one file in a single call, applied in
  order, all of them or none. One round trip instead of one per change, one
  diff to approve, and no way to leave a file half edited.
- `/undo`. Every file a request changes is snapshotted before the change,
  and one request is one checkpoint, so `/undo` puts all of it back at once
  (`/undo list` shows what is there). A file someone edited after thoth
  wrote it is reported and left alone rather than overwritten, and the last
  20 checkpoints live under `~/.thoth/projects/<key>/undo/` so a crash does
  not take them with it.
- `move_file` and `delete_file`. Renaming and deleting used to mean reaching
  for the shell, which skips the read registry, shows no diff, and turns one
  "always allow" into a standing permission to run `rm`. Deleting needs the
  whole file read first (the same condition as overwriting it), only works
  inside the working directory, and never touches a directory.
- `todo`: the plan for the task, written before starting and rewritten as it
  goes. It keeps a model from losing track of step three of five, and lets
  you see the plan is wrong before the work is done. Only the latest version
  of the list stays in the context.
- The config screen asks which endpoint a new profile talks to (anthropic,
  openai, gemini, openrouter, ollama) and fills in the url, so only the
  model and the key are left to type.
- Named config profiles. `thoth config` (or `thoth cfg`) opens a screen to
  edit them, `/config` does the same inside a session and applies the saved
  profile to the running conversation. `thoth -P NAME` runs one profile
  once, `thoth config use NAME` makes it the default, `thoth config list`
  shows what exists. Config files from 0.2 and earlier keep working; they
  are read as a profile named `default`.

### Changed
- Tool calls the model writes as text (`<tool_call>{...}</tool_call>`, or a
  bare json object) are read and run instead of printed. A json object only
  counts when it names a tool that exists, and anything that does not parse
  comes back to the transcript untouched.
- What a request costs is now budgeted against the context window instead of
  fixed at numbers tuned for 16k: how much of a tool result is kept, how much
  of a file `read_file` returns, and how much of the instruction file goes
  into the prompt. A small window keeps its room, a large one gets to use it.
- `grep`, `web_fetch`, `list_dir` and `glob` cut their own output to the
  budget too, the same way `read_file` does, so the line that says the
  result was incomplete is not itself the thing that gets cut off. They had
  fixed limits of their own (11k characters, 15k, 500 entries) that ignored
  how much room there actually was.
- `read_file` does its own cutting, so its "showing lines 1-240 of 900, read
  on with offset=241" survives. It used to be replaced by a blind truncation
  that left the model with no idea there was more, or how to ask for it.
- Repeating a read-only call with the exact same arguments replaces the
  older result in the context with a one-line note. Two copies of the same
  file are one copy of dead weight; a read of a different range, or a search
  with a different pattern, is left alone because it says something else.
- The prompt ends with one worked example of a small task from grep to final
  answer, and says how many tool calls are left for the request. Small models
  follow an example better than another rule.
- The style rules now say what not to write: no listing the edits again, no
  repeating the plan just carried out, no explaining code that was not asked
  about, and an answer as long as the question deserves.
- Rules about git and about the editor's `problems` tool are only sent when
  there is a repo and an editor to use them on, and the tool itself is only
  offered then: 800 characters of every request that was buying nothing.
- Auto-compact works on every api, not just Ollama: it measures against the
  profile's `context_window`. The field used to be called `num_ctx`, which
  is still read.
- thoth no longer picks a model on its own when the endpoint is hosted. A
  local server usually has one or two and guessing is a kindness; a hosted
  one has hundreds and guessing spends the user's money, so it lists some
  and asks.
- The Ollama probe only fires at a local address. A hosted endpoint never
  sees a request to a path thoth guessed at.
- `stream_options` is an OpenAI extension that not every compatible server
  takes. When one rejects it, thoth drops the field and retries instead of
  failing the turn: losing the token counts beats losing the answer.
- The config file moved to `~/.thoth/config.toml` on every OS, next to the
  state thoth already kept there, instead of the platform config directory
  (`%APPDATA%\thoth` on Windows, `~/.config/thoth` elsewhere). The old path
  is still read when the new one is missing, so upgrading changes nothing
  until the next save.
- `@path` now opens a picker under the input instead of completing on tab
  only: up/down move, tab or enter takes the highlighted entry, esc closes
  it, and picking a directory lists what is inside it.
- Startup screen: logo, version, working directory and, on the Ollama
  native api, the context window that used to be a transcript line.
  `/clear` shows it again.

### Fixed
- The directory an "always allow" covered for a file outside the working
  directory was recorded as the model spelled it, so reading `../notes.txt`
  saved a grant ending in `/..`: no later path matched it, and `/allow`
  showed the user something they could not place. Found by running it.
- `glob` stopped at 500 matches and said nothing about it, so a truncated
  list read like the whole answer. It says what it stopped at now.
- The editor context (active file, selected text, the Problems panel) went
  into every request unbudgeted: a large selection or one page-long type
  error could take a good part of a small window. Both are capped, and say
  when they were cut.
- The permission preview for overwriting a file with CRLF line endings
  showed every line of it as changed. It now previews what will actually be
  written.
- Every multi-line `edit_file` failed on a file with CRLF line endings,
  which is most files on a Windows checkout. `read_file` shows lines with
  the carriage return stripped, so the text the model copies back could
  never match the file byte for byte. Edits are now matched in the file's
  own line endings, and a full overwrite keeps them instead of turning
  every line into a change.
- `--continue` on a session that was killed between a tool call and its
  result sent a transcript every api rejects, and nothing but `/clear` got
  past it. Half a turn is dropped when the transcript is loaded.
- The `todo` tool refused a status it understood perfectly well: a model
  writing "in_progress" or "Completed" instead of "doing" and "done" lost a
  turn to a validation error. Those spellings are read as what they mean; a
  word that means nothing here is still refused.
- Security: one "always allow" for a file outside the working directory
  covered every file everywhere for the life of the project. Reads and
  writes outside it are now scoped to the directory the file is in; inside
  the working directory one answer still covers the project.
- `multi_edit`, `move_file` and `delete_file` showed nothing once they were
  always-allowed, unlike `write_file`, `edit_file` and `shell`. Every one of
  them now prints what it does whether or not it had to ask.
- `/undo` said nothing about a file it had not snapshotted because the
  request was too large, so a request could come back looking complete while
  one file was still changed. That file is now named and reported.
- An undone checkpoint is kept, marked, instead of being deleted. For a file
  that had been edited since and was therefore left alone, the copy in that
  checkpoint was the only one left of what was in it before.
- Security: `move_file` could carry a file in from anywhere or out to
  anywhere, and needed no read first, which made renaming a way around
  having to read a file before overwriting it. Both endpoints must now be
  inside the working directory and the file must have been read, the same
  bar as deleting it. Found by review of the commit that added it.
- Security: the `delete_file` permission preview read the file before the
  user had approved anything, including one outside the working directory
  and of any size. It now refuses outside paths and reads only the first
  few lines.
- `inside_project` said "outside" for any path that did not exist yet on
  Windows, where the canonical form of the working directory carries a
  `\\?\` prefix that a plain joined path does not.

### Internal
- `client.rs` was 1600 lines holding three wire protocols; it is a module
  directory now, one file per protocol (`openai`, `ollama`, `anthropic`)
  with the shared message shape and transport choice in `mod.rs`. The two
  text protocols shared 50 lines of copied stream handling, which is now one
  `TextStream` in `client/stream.rs`.
- The drawing half of the interface moved out of `ui/mod.rs` into
  `ui/screen.rs`. A child module sees its parent's private fields, so
  nothing had to be made public to split it.

### Removed
- Google Programmable Search. `web_search` goes through DuckDuckGo, which
  needs no key and no account. The `google_api_key` and `google_cx`
  settings are gone; search backends are worth revisiting as a whole later.

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

[0.3.0]: https://github.com/thoth-coder/thoth/releases/tag/v0.3.0
[0.2.0]: https://github.com/thoth-coder/thoth/releases/tag/v0.2.0
[0.1.0]: https://github.com/thoth-coder/thoth/releases/tag/v0.1.0
