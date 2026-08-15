# Configuration

Settings live in named profiles. One is active; that is the one thoth
starts with. Switch with `thoth -P NAME` for a single run, or make another
one active for good.

The quickest way in is the screen:

```sh
thoth config        # or: thoth cfg
```

and from inside a session, `/config` (or `/cfg`). Saving there applies the
profile to the running session right away: the model, server, context
window and turn budget change, the conversation stays.

On the screen: `up`/`down` moves, `tab` switches between the profile list
and the settings, `enter` edits, `space` toggles, `a` makes a profile
active, `n` adds one, `r` renames, `d` deletes, `ctrl+s` saves, `esc`
leaves. Api keys show as dots until you open the field.

Without the screen:

```sh
thoth config list          # profiles, with * on the active one
thoth config use big       # start with "big" from now on
thoth config path          # where the file is
```

## The file

`~/.thoth/config.toml`, the same path on every OS, next to the per-project
state in `~/.thoth/projects/`. thoth writes it owner-only, since api keys
live in it. `thoth config path` prints it.

thoth 0.2 and earlier kept it in the platform config directory
(`%APPDATA%\thoth` on Windows, `~/.config/thoth` elsewhere). That file is
still read when there is nothing in `~/.thoth`, so nothing breaks; the next
save writes to the new place and the old file can be deleted.

```toml
active = "local"

[profiles.local]
base_url = "http://localhost:11434/v1"
model = "qwen3:8b"
num_ctx = 32768
think = false             # disable thinking (Ollama only), saves a lot of
                          # tokens on small windows

[profiles.big]
base_url = "http://192.168.1.10:11434/v1"
model = "qwen3.6:35b"
num_ctx = 65536

[profiles.hosted]
base_url = "https://openrouter.ai/api/v1"
api_key = "..."
model = "..."
temperature = 0.7
max_turns = 40            # agent steps per request before pausing

# Use Google for web_search instead of DuckDuckGo. Google has no free
# scraping, so this needs a Programmable Search Engine:
#   1. create an engine at https://programmablesearchengine.google.com (the "cx" id)
#   2. get an API key at https://developers.google.com/custom-search/v1/introduction
# Free tier is 100 queries/day. If Google fails, thoth falls back to DuckDuckGo.
# google_api_key = "..."
# google_cx = "..."
```

Every field is optional. A profile only records what it changes.

A config file from thoth 0.2 or earlier, with the settings at the top level
and no profiles, still works: it is read as a profile named `default`, and
the next save writes it out in the new shape.

## Precedence

CLI flags, then environment variables, then the active profile, then the
built-in defaults.

Environment variables: `THOTH_PROFILE`, `THOTH_BASE_URL`, `THOTH_MODEL`,
`THOTH_API_KEY`, `THOTH_TEMPERATURE`, `THOTH_NUM_CTX`, `THOTH_THINK`,
`THOTH_GOOGLE_API_KEY`, `THOTH_GOOGLE_CX`.

CLI flags: `-P/--profile`, `--base-url`, `-m/--model`, `--api-key`,
`--temperature`, `-p/--prompt` for one-shot mode. See `thoth --help`.

## Context window notes

`num_ctx` and `think` only work with Ollama, because thoth talks to Ollama
through its native API where those are per-request options. Other servers
use the standard OpenAI API, which has no such fields; the window is
whatever the server was started with (`llama-server -c 16384`).

If the model stalls, rambles, or forgets files it just read, the window is
almost always too small. Raise `num_ctx` (costs RAM/VRAM; `ollama ps` shows
the active size), or set `think = false` for reasoning models.
