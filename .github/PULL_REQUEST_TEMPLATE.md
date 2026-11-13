## What and why

<!-- What this changes and the problem it fixes. Link the issue if there is one. -->

## How it was tested

<!-- If agent behavior changed, say which model and server you ran it against
and the prompt you used (`thoth -p "..."` in a scratch project is enough). -->

## Checklist

- [ ] `cargo fmt`
- [ ] `cargo clippy -- -D warnings` passes
- [ ] `cargo test` passes (and `cargo test -- --ignored` if `tools/web.rs` changed)
- [ ] Line added to `CHANGELOG.md` under Unreleased, if it is user-visible
- [ ] Docs in `docs/` updated for a new flag, tool or slash command
- [ ] New screen has a view in `src/ui/demo.rs`
