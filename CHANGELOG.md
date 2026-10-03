# Changelog

## 0.5.2

- `last` and `pack` skip the active Droid session. Droid exports no session ID, so dejavu finds the nearest `droid` parent process, reads its working directory, and treats the newest transcript in that directory's session folder as the active one. Before this, `dejavu last` run inside Droid returned the session asking.

## 0.5.1

- `dejavu last` shows where a session left off: its card and the newest user/assistant turns within a character budget. With no argument it picks the newest session in the current Git repo, skipping the active one. It also accepts `find` terms, a transcript locator, or a session ID. `--list` prints recent session cards.
- Claude and Pi sessions report their recorded working directory as the project. The encoded directory name turns both `/` and `-` into `-`, so a project such as `hul-tech` used to show as `hul/tech` in `find`, the index, and Pi transcript views. The index rebuilds once to pick this up.
- `search` and `find` print in color on a terminal: each agent gets its own color, matched terms are highlighted, roles and labels are tinted, and multi-line excerpts are indented. Piped and `--json` output is unchanged. `--color` and `--no-color` override the default.
- `find` and `last` print a resume command for OpenCode sessions: `opencode2 -s <session id>`, taken from the locator.
- `query` and `profile --explain` default to `gpt-6-luna` through `codex exec`, still at medium reasoning. Pass `--model gpt-5.6-luna` for the previous model.
- Colored output renders Markdown: headings, bold, italic, strikethrough, inline code, fenced code blocks, bullet, numbered, and task lists, quotes, rules, links, and tables. It applies to `find` and `search` excerpts, `transcript` message bodies, and the `last` card and tail. Plain output keeps the raw Markdown.
- `pack` also skips the active Claude Code session through `CLAUDE_CODE_SESSION_ID`, the variable Claude Code exports.

## 0.5.0

- Dejavu is now a single native binary written in Rust. It replaces the Bun/TypeScript CLI and needs no JavaScript runtime.
- Installation through npm or a release binary works as before. To run from source, use `cargo build --release` or `scripts/install-local.sh`.
- `find` and `pack` are faster on large transcript stores.
- Commands, flags, text output, `--json` shapes, exit codes, environment variables, and transcript locators are unchanged, apart from the Droid additions below.
- Dejavu reads Factory Droid sessions from `~/.factory/sessions`, or `$FACTORY_HOME_OVERRIDE/.factory/sessions` when that variable is set. `--source droid` selects them, and `show`, `transcript`, `profile`, and `scrub` accept Droid transcript paths. Droid's injected `<system-reminder>` context is left out of search and views.
