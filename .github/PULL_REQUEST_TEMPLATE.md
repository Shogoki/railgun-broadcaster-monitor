## What and why

## Checklist

- [ ] `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test` pass
- [ ] Parser change: real fixture line added, `PARSER_VERSION` bumped if existing meaning changed
- [ ] Schema change: `SCHEMA_VERSION` bumped, migration and test added
- [ ] User-visible change: line added under **Unreleased** in CHANGELOG.md, docs updated
- [ ] No secrets, personal addresses or private log lines in the diff
