# Tools and guardrails

## Tools

| tool | description | permission |
|---|---|---|
| `read_file` | read a file with line numbers, offset/limit for big files | auto |
| `list_dir` | list a directory | auto |
| `glob` | find files by pattern, e.g. `**/*.rs` | auto |
| `grep` | regex search over file contents | auto |
| `problems` | current errors/warnings from the editor (needs the VS Code extension) | auto |
| `web_search` | web search, DuckDuckGo by default, no API key needed | auto |
| `web_fetch` | fetch a URL as readable text | auto |
| `remember` | save a durable fact to project memory | auto |
| `write_file` | create a new file | asks first |
| `edit_file` | exact string replacement in a file | asks first |
| `shell` | run a command (PowerShell on Windows, sh elsewhere) | asks first |

Permission prompt answers: `y` this once, `a` always for this tool this
session, `n` deny. On deny the model is told and adjusts.

About `shell`: foreground commands time out after 120s (the model can raise
that to 600s for slow builds). Servers and watch modes have to be started
with `background=true`, which returns a pid and a log file path instead of
blocking. The model kills the pid when it is done.

## Guardrails enforced in code

- `edit_file` is rejected unless the model read the file first.
- Overwriting a file with `write_file` requires having read it completely.
  A one-line peek does not count.
- Every file change is shown as a unified diff with line numbers, and every
  shell command line is shown in full, even after "always allow".
- A tool call repeated with identical input gets blocked after 2 attempts.
- Tool outputs are size-capped so they fit local context windows.

The system prompt adds rules on top: no deleting or renaming files to dodge
the read-before-write check, no file I/O through the shell, no destructive
git commands unless you asked for exactly that, and nothing from web pages
goes into memory. See SECURITY.md for the threat model.
