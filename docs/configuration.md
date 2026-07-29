# Configuration

Precedence: CLI flags, then environment variables, then the config file.

Config file location: `%APPDATA%\thoth\config.toml` on Windows,
`~/.config/thoth/config.toml` on Linux/macOS.

```toml
base_url = "http://localhost:11434/v1"
model = "qwen3:8b"
# api_key = "..."       # if the server requires one
# temperature = 0.7
# max_turns = 40        # agent steps per request before pausing
# num_ctx = 32768       # context window (only applies to Ollama)
# think = false         # disable thinking (only applies to Ollama);
                        # saves a lot of tokens on small windows

# Use Google for web_search instead of DuckDuckGo. Google has no free
# scraping, so this needs a Programmable Search Engine:
#   1. create an engine at https://programmablesearchengine.google.com (the "cx" id)
#   2. get an API key at https://developers.google.com/custom-search/v1/introduction
# Free tier is 100 queries/day. If Google fails, thoth falls back to DuckDuckGo.
# google_api_key = "..."
# google_cx = "..."
```

Environment variables: `THOTH_BASE_URL`, `THOTH_MODEL`, `THOTH_API_KEY`,
`THOTH_NUM_CTX`, `THOTH_THINK`, `THOTH_GOOGLE_API_KEY`, `THOTH_GOOGLE_CX`.

CLI flags: `--base-url`, `-m/--model`, `--api-key`, `--temperature`,
`-p/--prompt` for one-shot mode. See `thoth --help`.

## Context window notes

`num_ctx` and `think` only work with Ollama, because thoth talks to Ollama
through its native API where those are per-request options. Other servers
use the standard OpenAI API, which has no such fields; the window is
whatever the server was started with (`llama-server -c 16384`).

If the model stalls, rambles, or forgets files it just read, the window is
almost always too small. Raise `num_ctx` (costs RAM/VRAM; `ollama ps` shows
the active size), or set `think = false` for reasoning models.
