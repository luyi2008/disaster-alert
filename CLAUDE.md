# CLAUDE.md

Repository notes for Claude Code. Full contributor guide (in Chinese) lives in [CONTRIBUTING.md](CONTRIBUTING.md); this file only lists what Claude must check before making a change.

## Rules

- [.claude/rules/git.md](.claude/rules/git.md) — branch naming

## Before changing anything

- `src/` is the full Rust application. The web frontend lives in a separate repo, [disaster-alert-web](https://github.com/luyi2008/disaster-alert-web); this repo does not embed it.
- Run at least `cargo fmt --check`, `cargo check`, `cargo test` before committing. Run `cargo clippy --all-targets --all-features` too when the change touches dependencies, concurrency, error handling, HTTP/WebSocket, or shared models.
- New code must not use `unwrap()`, `expect()`, `dbg!()`, `println!()`, `todo!()`, `unimplemented!()`, and must not introduce `unsafe`.
- Before touching Bark keys, subscription data, notification tokens, or log masking, read the "安全和隐私确认" section in CONTRIBUTING.md.
