# Getting started

## Build

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
