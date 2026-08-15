# Thoth

[![CI](https://github.com/thoth-coder/thoth/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/thoth-coder/thoth/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](LICENSE-MIT)

**A coding agent for the model you choose.** Point Thoth at your own
hardware (Ollama, llama.cpp, vLLM) or at an api you pay for (Anthropic,
OpenAI, Google, OpenRouter) and it explores your codebase, edits files, runs
builds and tests, and searches the web when it needs documentation. Every
action is shown on screen and destructive ones wait for your approval.

## Highlights

- **Your model, your machine.** Auto-detects Ollama and uses its native API
  to control the context window itself, so agentic prompts don't get
  silently truncated. Anthropic gets its own transport, with prompt caching.
  Everything else speaks the OpenAI api.
- **Profiles.** Keep local and hosted setups side by side, switch with one
  key, and see what a session has cost while it runs. `thoth config`.
- **Safe by construction.** The model cannot edit a file it hasn't read, or
  overwrite one it hasn't read completely. These rules live in Rust, not in
  the prompt. Full diffs and full command lines are always displayed, even
  after "always allow".
- **Editor aware.** With the
  [VS Code extension](https://github.com/thoth-coder/thoth-for-vscode),
  Thoth knows your active file, selected lines and the Problems panel, and
  the model can re-check diagnostics after every edit.
- **Built for small context windows.** Live token meter, automatic
  compaction, per-project memory and session recaps keep long tasks running
  on 16-32k tokens.
- **No account needed.** Web search goes through DuckDuckGo, with no key.

## Install

Linux / macOS:

```sh
curl -fsSL https://raw.githubusercontent.com/thoth-coder/thoth/main/scripts/install.sh | sh
```

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/thoth-coder/thoth/main/scripts/install.ps1 | iex
```

Or build from source: `cargo build --release` (-> `target/release/thoth`).
Update later with `thoth upgrade`.

## Quick start

Local:

```sh
ollama pull qwen3:8b             # any tool-calling model
thoth                            # or: thoth -p "one-shot prompt"
```

Hosted: `thoth config` and fill in a profile, e.g. base url
`https://api.anthropic.com/v1` with your key and `claude-sonnet-4-5`. See
[Configuration](docs/configuration.md) for OpenAI, Google and the rest.

## Documentation

| | |
|---|---|
| [Getting started](docs/getting-started.md) | Ollama, llama.cpp, choosing a model |
| [Usage](docs/usage.md) | keys, commands, memory, editor integration |
| [Tools & guardrails](docs/tools.md) | what the model can do and how it is contained |
| [Configuration](docs/configuration.md) | profiles, providers, cost, context window |

## Contributing

Bug reports and pull requests are welcome. See
[CONTRIBUTING.md](CONTRIBUTING.md) for the ground rules and
[SECURITY.md](SECURITY.md) for the threat model.

## License

Dual-licensed under [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at
your option.
