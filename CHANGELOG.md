# Changelog

## 0.5.0

- Dejavu is now a single native binary written in Rust. It replaces the Bun/TypeScript CLI and needs no JavaScript runtime.
- Installation through npm or a release binary works as before. To run from source, use `cargo build --release` or `scripts/install-local.sh`.
- `find` and `pack` are faster on large transcript stores.
- Commands, flags, text output, `--json` shapes, exit codes, environment variables, and transcript locators are unchanged, apart from the Droid additions below.
- Dejavu reads Factory Droid sessions from `~/.factory/sessions`, or `$FACTORY_HOME_OVERRIDE/.factory/sessions` when that variable is set. `--source droid` selects them, and `show`, `transcript`, `profile`, and `scrub` accept Droid transcript paths. Droid's injected `<system-reminder>` context is left out of search and views.
