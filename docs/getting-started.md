# Getting started

## Install

Prebuilt binaries (Linux x86_64/arm64, macOS Intel/Apple Silicon,
Windows x86_64):

```sh
# Linux / macOS
curl -fsSL https://raw.githubusercontent.com/thoth-coder/thoth/main/scripts/install.sh | sh
```

```powershell
# Windows
irm https://raw.githubusercontent.com/thoth-coder/thoth/main/scripts/install.ps1 | iex
```

Pin a version with `THOTH_VERSION=v0.2.0`, change the location with
`THOTH_INSTALL_DIR`. Uninstall with the matching `uninstall.sh` /
`uninstall.ps1` from the same directory (add `--purge` / `THOTH_PURGE=1`
to also remove config and state).

Update an existing install to the latest release with:

```sh
thoth upgrade
```

## Build from source

```sh
cargo build --release   # -> target/release/thoth
```

## With Ollama (default)

```sh
ollama pull qwen3:8b    # any model that supports tool calling
thoth
```

thoth defaults to `http://localhost:11434/v1`. When it detects Ollama it
switches to the native API so it can request a proper context window itself,
32768 tokens by default. Ollama's own default is 4096, which silently
truncates agentic prompts and makes models stall or loop.

If the server has exactly one model it is picked automatically, otherwise
pass `-m qwen3:8b`.

## With llama.cpp

```sh
llama-server -m model.gguf --jinja -c 16384
thoth --base-url http://localhost:8080/v1
```

`--jinja` enables the chat-template engine llama.cpp needs for tool calling.
`-c` sets the context size; thoth cannot set it for you here.

Other OpenAI-compatible servers (vLLM, LM Studio, ...) work the same way,
just point `--base-url` at them.

## With a hosted api

Anthropic, OpenAI, Google and the aggregators all work. Put the endpoint,
the key and the model in a profile:

```sh
thoth config
```

then start with it: `thoth -P claude`. thoth speaks Anthropic's own api
(with prompt caching, which cuts the bill on long sessions) and the OpenAI
api for everyone else. [Configuration](configuration.md) has a ready-made
profile for each provider, and how to show what a session costs.

A hosted endpoint offers hundreds of models, so thoth will not pick one for
you: set `model` in the profile, or pass `-m`.

## One-shot mode

```sh
thoth -p "explain what src/main.rs does"
```

Plain stdout output, permission prompts on stdin. Useful for scripting.

## Choosing a model

Tool-calling quality depends heavily on the model. qwen3, qwen2.5-coder,
devstral, llama3.1+ and mistral-small all work. Models under ~7B tend to
struggle with tool calls. Reasoning models (qwen3, deepseek-r1) show their
thinking dimmed and collapsed in the transcript.
