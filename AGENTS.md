# AGENTS.md

Rust 2024, stable 1.85+. Run `cargo fmt`, `cargo clippy -- -D warnings`
and `cargo test` before you finish. CI runs them on Linux, macOS and
Windows. Tests go in `mod tests` at the bottom of the file.

Don't hardcode tool names like `cargo` or `tsc` outside `agent/stack.rs`.
Adding a language is a new row in that table.

If the model must not do something, block it in code. Don't rely on the
prompt. When a guard says no, it should say what the model can do instead.

Show the user every command and diff, also when it's auto-approved.
"Always allow" is per program or per host.

Keep an eye on context size. If a tool cuts output, say so and say how to
get the rest. After changing the prompt, check
`cargo test prompt_budget -- --ignored --nocapture`.

Don't trust anything from the network or from files: no byte-offset
slicing, no unchecked paths.

Use `cfg!(windows)` instead of `#[cfg]` where you can. Debug-only stuff
goes behind `#[cfg(debug_assertions)]`. Don't add big dependencies.

User-facing changes need `docs/` and a `CHANGELOG.md` line. New screens
need a view in `ui/demo.rs`.
