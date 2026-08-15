# Tools and guardrails

## Tools

| tool | description | permission |
|---|---|---|
| `read_file` | read a file with line numbers, offset/limit for big files | auto inside the project |
| `list_dir` | list a directory | auto |
| `glob` | find files by pattern, e.g. `**/*.rs` | auto |
| `grep` | regex search over file contents, `context` shows surrounding lines | auto |
| `problems` | current errors/warnings from the editor (only offered when the VS Code extension is connected) | auto |
| `todo` | the plan for the task, rewritten as it goes | auto |
| `web_search` | web search through DuckDuckGo, no API key needed | auto |
| `web_fetch` | fetch a URL as readable text | asks per host |
| `remember` | save a durable fact to project memory | asks first |
| `write_file` | create a new file | asks first |
| `edit_file` | exact string replacement in a file | asks first |
| `multi_edit` | several replacements in one file, all or nothing | asks first |
| `move_file` | rename or move a file, never over an existing one | asks first |
| `delete_file` | delete one file inside the project, after reading it | asks first |
| `shell` | run a command (PowerShell on Windows, sh elsewhere) | asks per program |

Permission prompt answers: `y` this once, `a` always, `n` deny. On deny the
model is told and adjusts. An `a` answer is scoped to what you saw: the
program for a shell command, the host for a fetch. It is saved per project
in `~/.thoth/projects/<key>/allow.json`; `/allow` reviews it and
`/allow reset` clears it.

About `shell`: foreground commands time out after 120s (the model can raise
that to 600s for slow builds). Servers and watch modes have to be started
with `background=true`, which returns a pid and a log file path instead of
blocking. The model kills the pid when it is done.

## Guardrails enforced in code

- `edit_file` and `multi_edit` are rejected unless the model read the file
  first, and a `multi_edit` where any one edit does not apply writes nothing
  at all.
- Overwriting a file with `write_file` requires having read every line of
  it. Reads are tracked as line ranges, so neither a one-line peek nor a
  peek at the first and last line counts as having read the file.
- `read_file` refuses files over 2 MB; use `grep` or offset/limit instead.
- Every file change is shown as a unified diff with line numbers, and every
  shell command line is shown in full, even after "always allow".
- `delete_file` needs the whole file read first, the same condition as
  overwriting it, and only works inside the working directory. That also
  shuts the back door of deleting a file and writing a fresh one in its
  place to dodge the read rule. Directories are never deleted.
- `move_file` refuses to land on an existing file, so nothing is lost to a
  rename. The read record follows the file, since the content did not change.
- A tool call repeated with identical input gets blocked after 2 attempts,
  and an identical read-only call replaces the older copy in the context
  instead of adding a second one.
- Tool output is capped against the context window: a quarter of it or so,
  never the fixed number that used to make a small window unusable and a
  large one wasteful.

Every file a request changes is snapshotted first, and `/undo` puts the
whole request back. A file that was edited by someone else after thoth
touched it is reported and left alone: undo is not a second way to lose
work. The last 20 checkpoints are kept in
`~/.thoth/projects/<key>/undo/`, so they survive a crash.

The system prompt adds rules on top: no file I/O through the shell, no
destructive git commands unless you asked for exactly that, and nothing from
web pages goes into memory. See SECURITY.md for the threat model.
