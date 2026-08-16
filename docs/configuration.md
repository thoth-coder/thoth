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

The screen has two pages. The first is the profiles and nothing else:
`up`/`down` moves, `enter` opens the one you are on, `n` adds one, `r`
renames, `d` deletes, `a` makes it the active profile, the one thoth starts
with. `enter` on the last row (`+ new profile`) adds one too.

The second page is that profile's settings, one row each, the essential four
first and the rest under an `advanced` divider. `esc` goes back to the list.
On a setting: **type to replace it**, or `enter` to edit what is there.
`enter` keeps the value, `tab` keeps it and moves down, `esc` throws the
edit away. `space` flips the ones that are a choice rather than text (`api`,
`think`). Api keys show as dots until you open them.

The row you are on decides what the keys do, and the bottom line always
spells it out.

`ctrl+s` saves. `esc` leaves, and asks first if anything is unsaved. A new
profile starts by asking which endpoint it talks to, so the url is filled
in for you and only the model and key are left.

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
still read while the new one holds no profiles, so nothing breaks; the next
save writes to the new place and the old file can be deleted.

```toml
active = "local"

[profiles.local]
base_url = "http://localhost:11434/v1"
model = "qwen3:8b"
context_window = 32768
think = false             # disable thinking (Ollama only), saves a lot of
                          # tokens on small windows

[profiles.big]
base_url = "http://192.168.1.10:11434/v1"
model = "qwen3.6:35b"
context_window = 65536

[profiles.claude]
api = "anthropic"         # native api, so prompt caching works
base_url = "https://api.anthropic.com/v1"
api_key = "sk-ant-..."
model = "claude-sonnet-4-5"
context_window = 200000
max_tokens = 8192         # cap on one reply; anthropic requires one
price_in = 3              # usd per million tokens, for the cost readout
price_out = 15
price_cached = 0.3
```

Every field is optional. A profile only records what it changes.

### Fields

| field | meaning |
|---|---|
| `api` | `auto` (default), `openai`, `ollama` or `anthropic`. Auto reads the url, and asks a local server whether it is Ollama |
| `base_url` | the endpoint |
| `model` | empty asks the server, which only works for a local one |
| `api_key` | bearer token, or `x-api-key` on Anthropic |
| `headers` | extra request headers, for endpoints that want their own |
| `context_window` | requested per call on Ollama; elsewhere it is what auto-compact measures against, and what sizes one tool result and one `write_file`. Was called `num_ctx` |
| `max_tokens` | cap on one reply. Anthropic gets 8192 when it is unset, since that api requires one. Leave it empty on OpenAI reasoning models, which want `max_completion_tokens` instead and reject this |
| `think` | force thinking on or off (Ollama) |
| `temperature`, `max_turns` | sampling, and tool calls allowed per request |
| `price_in`, `price_out`, `price_cached` | usd per million tokens |

## Providers

```toml
[profiles.openai]
base_url = "https://api.openai.com/v1"
api_key = "sk-..."
model = "gpt-5"
price_in = 1.25
price_out = 10

[profiles.gemini]
base_url = "https://generativelanguage.googleapis.com/v1beta/openai"
api_key = "..."
model = "gemini-2.5-pro"

[profiles.openrouter]
base_url = "https://openrouter.ai/api/v1"
api_key = "sk-or-..."
model = "anthropic/claude-sonnet-4.5"
```

Groq, DeepSeek, Together, Fireworks, xAI and Mistral follow the same shape.
Check the provider's current prices before trusting the cost readout.

Endpoints that do not take a bearer token need `headers`:

```toml
[profiles.azure]
base_url = "https://NAME.openai.azure.com/openai/deployments/DEPLOYMENT"
model = "gpt-4o"
headers = { "api-key" = "..." }
```

## Cost

Set `price_in` and `price_out` and the status bar shows what the session has
spent, `/status` breaks it down, and `-p` prints it at the end. Nothing is
shown when the prices are missing, which is the sane default for a local
model that costs nothing.

`price_cached` is the discounted rate for input the provider served from its
cache. On the Anthropic transport thoth marks the system prompt, the tool
schemas and the end of the history as cacheable, so a long session mostly
re-reads instead of re-paying. Cache *writes* are billed slightly above the
normal input rate; the readout counts them as normal input, so a real bill
can be a few percent higher than what is shown.

A config file from thoth 0.2 or earlier, with the settings at the top level
and no profiles, still works: it is read as a profile named `default`, and
the next save writes it out in the new shape.

## Precedence

CLI flags, then environment variables, then the active profile, then the
built-in defaults.

Environment variables: `THOTH_PROFILE`, `THOTH_BASE_URL`, `THOTH_MODEL`,
`THOTH_API_KEY`, `THOTH_TEMPERATURE`, `THOTH_CONTEXT_WINDOW` (or the older
`THOTH_NUM_CTX`), `THOTH_THINK`.

CLI flags: `-P/--profile`, `--base-url`, `-m/--model`, `--api-key`,
`--temperature`, `-p/--prompt` for one-shot mode. See `thoth --help`.

## Context window notes

Only Ollama takes the window as a per-request option, so only there does
`context_window` (and `think`) change what the server does. Everywhere else
the window is fixed by the server or the model, and the number in the
profile is thoth's own yardstick: it is what auto-compact measures against,
and it is what the `ctx 12.4k/200k` readout divides by. Leave it out and
thoth simply stops guessing when to compact, so run `/compact` yourself.

It also sizes the two tool budgets. Left out, they fall back to numbers that
guess in opposite directions on purpose: a tool result is capped at 12k
characters, small, so one `grep` cannot eat the context of a server nobody
described, and one `write_file` at 40k, large, because a window nobody
declared is a hosted api and cutting a capable model down to 170 lines a
file would be the wrong mistake. Declaring the window replaces both guesses.

If the model stalls, rambles, or forgets files it just read, the window is
almost always too small. On Ollama raise `context_window` (costs RAM/VRAM;
`ollama ps` shows the active size), or set `think = false` for reasoning
models.
