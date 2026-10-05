# Changelog

## 0.6.1

- dejavu reads agy (Antigravity CLI) conversations from `~/.gemini/antigravity-cli/brain`. `--source agy` selects them, and search, `find`, `last`, `pack`, `show`, `transcript`, `profile`, and `query` cover them. The project comes from agy's `conversation_summaries.db`, or from `history.jsonl` for older conversations. Session cards print `agy --conversation <id>` to resume. Hook injections and agy's metadata blocks do not count as user text. agy records tool calls without IDs, so `transcript` pairs each result with the next unanswered call. `last` and `pack` skip the conversation in `ANTIGRAVITY_CONVERSATION_ID`, which agy sets for the commands it runs. `scrub` refuses agy conversations, because agy keeps copies dejavu cannot redact. The index rebuilds once to pick this up.

## 0.6.0

- `find` returns sessions it used to drop. OpenCode hits keep their project, so `--project` works for them. `--since` checks the dates of matching messages, so a session resumed after its filename date still matches. Source, project, and date filters run before the 40-candidate limit; a project-scoped search no longer comes back empty because other projects outranked it. Every matching visible message counts toward a session's score, including messages after long tool logs. A hit carries `truncated: true` when eligible candidates were left out.
- `find` is faster on queries with a short term such as `rg` or `PR`. Long terms pick the candidates from the index, and short terms are checked only inside them. On one large store, `deja find dejavu rg` dropped from 11–30 seconds to about 2.
- Harness instruction blocks (`# AGENTS.md instructions`, `<INSTRUCTIONS>`, environment context) no longer count as user prompts or user matches. Find excerpts center on a matching term instead of showing the start of the message. `find` and `last` cards cap the opening prompt at 300 characters; `transcript --full` shows the rest.
- `show --around TERM` centers each matching message's excerpt on the term. Long messages used to be cut before the match.
- `pack` picks fewer, denser neighborhoods and gives each a useful excerpt. Small budgets now return a few readable excerpts instead of dozens of fragments, and the JSON stays close to `--budget-chars`. A top-level `omitted` array lists what was left out with the `nextEvent` to continue from.
- `deja search TERMS` searches for TERMS. It used to search for the word "search" as well.
- `deja <command> --help` and `deja help <command>` print that command's flags, its JSON keys, and a working `jq` line. Help output carries no ANSI codes when piped.
- `query` and `profile --explain` take `--harness codex|ruddr` and `--effort`. Defaults come from `DEJAVU_QUERY_HARNESS`, `DEJAVU_QUERY_MODEL`, `DEJAVU_QUERY_EFFORT`, or `~/.config/dejavu/config.json`. `codex exec` with `gpt-6-luna` at medium stays the default.
- Status, warning, and error lines on stderr are colored only when stderr is a terminal and `NO_COLOR` is unset. `--no-color` turns them off too.
- Windows: transcript paths that mix `\` and `/`, or use 8.3 short names, resolve to their store.
- Droid sessions open again. Newer Droid versions record hook runs as message rows and write `SessionEnd` last with no parent, so `show` and `transcript` started from that row and failed with `transcript has no recallable messages`. Hook rows no longer start the active branch. A message id that Droid reuses on a later self-parented row no longer cuts the branch off before the conversation.
- Droid skill activations no longer read as user text. Droid appends the skill body to the user's message as a `<system-notification>` block; `find`, `show`, and `pack` keep only what the user wrote. The index rebuilds once to pick this up.
- Archived Codex rollouts in `$CODEX_HOME/archived_sessions` are searched, and `show` opens them. They used to fail with `cannot determine transcript source`.
- Droid compaction summaries appear where the compaction happened, as a user message headed `[Compaction summary of N earlier messages]`. A Droid session that holds only a summary used to fail with `transcript has no recallable messages`. Codex compactions with a plaintext summary appear the same way; most Codex summaries are encrypted and stay hidden. Like Claude's summaries, they show in `show`, `transcript`, and search, and `find` does not treat them as user prompts. The index rebuilds once to pick this up.
- `show`, `transcript`, and `query` drop the harness blocks Codex sends as user text (`# AGENTS.md instructions`, `<environment_context>`, `<recommended_plugins>`), so a Codex session opens on the user's first prompt. Plain search still matches them.
- OpenCode subtask parts, such as a `/usage` command that runs as a subagent, appear in `transcript` as a user message headed `[subtask /usage]`.

## 0.5.3

- Windows paths work throughout. `C:\...` and `\\server\...` values of `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `PI_CODING_AGENT_DIR`, `XDG_DATA_HOME`, and `FACTORY_HOME_OVERRIDE` are used as given; they used to be joined onto the current directory, so those stores went missing. Transcript paths with backslashes (`C:\Users\me\.claude\projects\...`) are recognized by `show`, `transcript`, `last`, and the other locator commands. OpenCode locators for a Windows database read `opencode:///C:%5C...` and open again; before, every OpenCode session on Windows failed with `invalid OpenCode locator`. With `HOME` unset, the home directory comes from the account, so projects under it still print relative to `~`. Opening a directory as a transcript reports `Directories cannot be read like files` instead of `EACCES`.
- CI passes on Windows again.

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
