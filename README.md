# dejavu

Agents lose useful context when work moves between Claude Code, Codex, Pi, OpenCode, and Droid. `dejavu` gives them one local command for finding earlier sessions and project memories.

`dejavu` is agent-first. An agent can search past sessions, inspect the relevant conversation, and recover decisions, commands, errors, and file changes. The agent can then verify that historical context against the current workspace. Humans can run the same commands from a terminal.

Search and transcript parsing stay on your machine. The optional `dejavu query` command sends selected conversation context through Codex exec to `gpt-6-luna` with medium reasoning by default. Search results can contain credentials or personal data that appeared in a transcript. Agents should treat the output as private.

Dejavu maintains an incremental SQLite full-text index under `~/.cache/dejavu/`. Before each search it parses only the appended tail of changed JSONL transcripts and pulls new or updated OpenCode messages by cursor. A typical refresh takes well under a second.

## Quick start

Dejavu is a single native binary. It needs no Bun, Node, or other runtime to run.

Install from npm:

```bash
npm install -g @safzanpirani/dejavu
# or
bun add -g @safzanpirani/dejavu
```

The package downloads the checksum-verified binary for your platform from the matching GitHub release. Bun skips install scripts for untrusted packages, so the first `dejavu` run fetches the binary instead. If no verified binary is available for your platform, the launcher reports it and exits.

Or download the binary for your platform from the [latest release](https://github.com/safzanpirani/dejavu/releases/latest) and put it on your `PATH`:

```bash
curl -fsSL -o ~/.local/bin/dejavu https://github.com/safzanpirani/dejavu/releases/latest/download/dejavu-darwin-arm64
chmod +x ~/.local/bin/dejavu
ln -sf dejavu ~/.local/bin/deja
```

Releases ship `dejavu-darwin-arm64`, `dejavu-darwin-x64`, `dejavu-linux-x64`, `dejavu-linux-arm64`, and `dejavu-windows-x64.exe`, with SHA-256 sums in `checksums.txt`.

To run from source instead, install [Rust](https://rustup.rs/) 1.88 or newer and clone the repository. Build the binary with Cargo, or build and install it with the script:

```bash
cargo build --release
./target/release/dejavu --help

scripts/install-local.sh
```

The script builds the release binary and installs `dejavu` and the `deja` link in `~/.local/bin`. Set `DEST` to install somewhere else.

`dejavu` is the primary command. The package also installs `deja` as a compatibility alias for existing scripts and agent instructions.

Search every detected agent store:

```bash
dejavu 'Cannot find module'
dejavu find deployment timeout --since 2w
dejavu show '<locator from search results>' --around timeout
dejavu transcript '<locator from search results>'
dejavu scrub '<locator from search results>' --drop 12 --pattern secret-host
dejavu memory search 'deployment boundary'
```

Ask a model to summarize one selected session:

```bash
dejavu query '<locator from search results>' 'What did we decide?'
```

The default `dejavu query` harness requires an installed, authenticated `codex` binary with access to `gpt-6-luna`. Select `--harness ruddr` to use an installed Ruddr and its provider authentication. Plain search, `find`, `show`, and memory commands do not invoke a model. Explicit `--model provider/id` overrides retain the legacy HTTP/Pi transports.

## What it reads

`dejavu` detects these local stores:

| Agent | Local data | Selected by |
| --- | --- | --- |
| Claude Code | JSONL transcripts under `~/.claude/projects` | `CLAUDE_CONFIG_DIR` → `$CLAUDE_CONFIG_DIR/projects` |
| Codex | JSONL transcripts under `~/.codex/sessions` | `CODEX_HOME` → `$CODEX_HOME/sessions` |
| Pi | JSONL transcripts under `~/.pi/agent/sessions` and sibling profiles such as `~/.pi/juna/sessions` | `PI_CODING_AGENT_DIR` → `$PI_CODING_AGENT_DIR/sessions` |
| OpenCode | SQLite databases under `~/.local/share/opencode`, in the legacy `part` schema or the v2 `session_message` schema | `XDG_DATA_HOME` → `$XDG_DATA_HOME/opencode/*.db`; `OPENCODE_DB` → that one database |
| Factory Droid | JSONL transcripts under `~/.factory/sessions`; only `*.jsonl` session files are read | `FACTORY_HOME_OVERRIDE` → `$FACTORY_HOME_OVERRIDE/.factory/sessions` |

Each variable is the one the agent itself honors. When it is set, Dejavu searches that store instead of the home-directory default, never both. Two people who share one Unix account with separate agent directories therefore search only their own history.

It also reads Claude Code Markdown memory under `~/.claude/projects/*/memory/`. Set `CLAUDE_CONFIG_DIR` or pass `--root` to use another Claude store.

## Commands

### Search for an exact phrase

```bash
dejavu search --source codex session-recall.ts --max-parallel 4 --json
```

The explicit `dejavu search PHRASE` command and bare `dejavu PHRASE` search the same phrase. Use `dejavu -- search` or `dejavu search search` to search for the literal word `search`. A quoted phrase such as `dejavu "search failed"` also stays literal.

Search uses case-insensitive fixed-string matching. Spaces form one exact phrase. The default source is `all`. Use `--source claude|codex|pi|opencode|droid` to narrow the search.

The index preserves literal phrase semantics and excludes reasoning, developer instructions, and tool output. Match counts are occurrences in visible message text. Pass `--no-index` to use the direct filesystem and SQLite scanners, which count raw transcript lines and rank differently.

Each result includes its source, date, project, match count, snippets, and locator. JSONL sources return file paths. OpenCode returns `opencode://...#session-id` locators.

### Find a session from a few terms

```bash
dejavu find workshop codex colleagues
dejavu find deployment timeout --project payments-api --since 2w
```

`find` searches for multiple literal terms in one session. `--since` uses the newest matching message date and falls back to session activity when matching messages have no date. Resumed sessions can match even when their filenames predate the cutoff. OpenCode results retain the session project directory, and `--project` filters that directory. It ranks user-message matches above assistant-message matches. Harness instruction blocks and command wrappers do not count as user matches or opening prompts. Counts include all matching visible messages in each scored session, including dialogue after large tool logs. Match excerpts center on the first matching term and use at most 240 characters. An ellipsis marks each cut side. Source, project, and date filters run before the 40-candidate scoring limit. JSON sets `truncated: true` when more eligible candidates exist, and stderr suggests narrower filters. The result includes the opening prompt, matching messages, transcript path, and a resume command when the source supports one. In a terminal, `search` and `find` color each agent, highlight the search terms, render Markdown in excerpts, and indent multi-line excerpts. Piped output stays plain. Use `--color` or `--no-color` to override. Stderr color requires a terminal, and any defined `NO_COLOR` value disables it. Commands that accept `--no-color` also disable stderr color; `--color` only forces stdout color.

### Read a transcript

```bash
dejavu show '<locator>'
dejavu show '<locator>' --around database
```

`show` renders user and assistant turns without model usage. It summarizes tool calls and truncates long messages by default. Pass `--full` to disable truncation.

Use `show --no-tools` to remove tool summaries and tool-only turns. `--around` then counts only the remaining dialogue turns. Matching excerpts center on the first case-insensitive occurrence and mark cut sides with `…`. `--max-chars N` changes the 700-character message limit in text and JSON output; the ` [...]` marker is additional. `--no-toolcalls` is an alias for `--no-tools` in both `show` and `transcript`.

### Pack search results into bounded excerpts

```bash
dejavu pack deployment timeout --project payments-api --budget-chars 8000 --json
dejavu pack database --context 1 --limit 3 --exclude-session '<session ID or locator>'
```

`pack` combines the session finder with user/assistant excerpts around literal matches. It uses no model. Defaults are three sessions, two neighboring dialogue events per match, 1,200 characters per event, and 12,000 event-body characters across the pack. It ranks neighborhoods by match density and gives user dialogue extra weight. It selects fewer events before sharing the budget, reserves a metadata allowance, and aims for at least 200 body characters per excerpt when limits permit. It merges overlapping selections and returns events in transcript order. Long matching events show a region around a search term. Very small budgets can abbreviate or omit matches.

The command accepts `find` filters (`--source`, `--project`, `--since`, `--user`), `--no-index`, and `--max-parallel`. `--context 0` returns matching events only. `--exclude-session` is repeatable and accepts an exact locator or session ID. Available `CODEX_THREAD_ID`, `CLAUDE_CODE_SESSION_ID`, and `CLAUDE_SESSION_ID` values exclude the active session automatically. Under Droid, which exports no session ID, the active session is the newest transcript in the `droid` parent process's working-directory folder. Search examines at most 40 ranked candidates; counts describe those candidates, not every stored session. Results retain relaxed search terms and report unreadable stores and sessions.

Pack JSON uses compact formatting and preserves event metadata fields. The budget counts body characters; locators, metadata, and JSON encoding add overhead. Each excerpt carries its source locator and original event IDs. `omitted` lists omitted event and match-neighborhood counts per loaded session, with a locator and `nextEvent`. A neighborhood counts as omitted when its matching anchor is absent. Partial neighborhoods can omit context events even when their anchor is present. `window.clipped` lists shortened fields and character offsets; `window.nextEvent` identifies the first omitted excerpt event. Read more with `transcript '<locator>' --from-event N`, or recover a shortened event with `transcript '<locator>' --full --from-event N --limit 1`. Transcript continuation reads the conversation from that ID; it does not repeat the pack's match filter.

### Continue where a session left off

```bash
dejavu last                         # newest session in this Git repo
dejavu last --list -n 5             # cards for the five newest
dejavu last auth refactor           # find's best match for the terms
dejavu last '<locator or session ID>'
dejavu last --anywhere --since 1d --json
```

`find` and `last` cap opening-prompt previews at 300 characters, including a trailing ellipsis when shortened. Use `transcript --full` to read the complete prompt.

`last` prints a session card (date, source, project, opening prompt, last user request, transcript, resume command) and the newest user/assistant turns that fit a budget. Injected harness messages such as task notifications are left out of the tail. It uses no model. With no argument it picks the newest session whose project is the current Git work tree or a directory below it, falling back to the current directory. `--project SUBSTR` matches project paths that contain SUBSTR instead, and `--anywhere` drops the project filter. Terms rank sessions the way `find` does within the current repo, then across every project when the repo has no match. A transcript locator or session ID selects that session directly, even the active one.

The active `CODEX_THREAD_ID`, `CLAUDE_CODE_SESSION_ID`, or `CLAUDE_SESSION_ID` session is skipped, as is the active Droid session (found the way `pack` finds it), so "last session" means the one before this one. `--exclude-session` skips more. The tail defaults to the newest 12 events within 8,000 body characters at 1,500 per event. `--turns`, `--budget-chars`, and `--max-chars` change those bounds. `--tools` adds tool calls and results at `--tool-chars` (default 400) each. `--list` prints only cards. The output ends with a `transcript --from-event` command for earlier turns.

### View a transcript turn by turn

```bash
dejavu transcript '<locator>'
dejavu transcript '<locator>' --full --thinking
dejavu transcript '<locator>' --no-tools --json
```

`transcript` renders the full conversation with labeled `USER` and `ASSISTANT` turns, timestamps, every tool call with its input, and every tool result. It works the same way for Claude, Codex, Pi, OpenCode, and Droid sessions. Tool inputs and outputs are truncated by default. Pass `--full` to print everything, `--thinking` to include model reasoning, and `--no-tools` to hide tool activity. Colors are on when stdout is a terminal, and message bodies render as Markdown (headings, emphasis, inline code, code blocks, lists, quotes, links, and tables). `last` does the same for its card and tail. Use `--color` or `--no-color` to override.

```bash
dejavu transcript '<locator>' --no-tools --max-chars 1500 --budget-chars 8000 --json
dejavu transcript '<locator>' --tool-chars 400 --from-event 20 --limit 10 --json
```

Explicit `--max-chars`, `--tool-chars`, and `--budget-chars` limits apply to JSON as well as text. `--max-chars` caps dialogue and thinking bodies; `--tool-chars` caps each tool input and output. Character budgets count JavaScript string characters in event bodies, including truncation ellipses, and exclude JSON encoding, formatting, labels, and metadata. A shortened structured tool input becomes a preview string, identified by its `window.clipped` record. No original transcript data is changed.

`--from-event N` is inclusive and accepts zero. `--limit N` bounds the event count after filtering. Event IDs remain stable across filters; `window.nextEvent` is the next event to request or `null` at the end. Shortened content must be recovered separately using `--full --from-event N --limit 1` (add `--thinking` for reasoning events). `--full` accepts pagination but cannot be combined with character limits. Without explicit bounds, JSON remains complete. Text keeps its original display limits; when character bounds are supplied, unspecified dialogue/tool limits default to 1,200/600 characters.

### Profile tool activity

```bash
dejavu profile '<transcript-locator>' --json
dejavu profile --project dejavu --since 7d --limit 10 --json
dejavu profile '<transcript-locator>' --explain --json
```

The default is deterministic and invokes no model. It measures outer calls, result characters, identical-input repeats, error flags, observation calls, and recognizable nested `tools.name()` call sites. JSON preserves event IDs for inspection with `dejavu transcript`; it omits raw prompts, inputs, and outputs. `--output-threshold N` changes the oversized-result threshold from 10,000 characters.

Repeated calls are candidates for review, not proven waste. Nested call sites are lexical hints, not executed counts; aliases, loops, templates, and computed access limit coverage. First-result latency includes waiting and is not model reasoning time. Project mode selects sessions by their last indexed visible-message date and measures each entire selected session. Check `omittedSessions` and `diagnostics` for coverage limits. Exit 1 signals skipped sources or an explanation failure even when measurements are available.

`--explain` sends only bounded metrics and event references to the selected query model. It defaults to Codex exec with `gpt-6-luna` at medium reasoning and accepts the same `--harness`, `--model`, `--effort`, and persistent defaults as `query`. It requires authentication for the selected provider and may incur model usage. Observations must cite supplied event IDs and remain separate from measurements. An explanation failure preserves the deterministic report.

### Redact a transcript

```bash
dejavu transcript '<locator>'                       # note the #N event numbers
dejavu scrub '<locator>' --drop 12 --drop 30-34 --dry-run
dejavu scrub '<locator>' --drop 12 --pattern "secret-host" --pattern "api key"
```

`scrub` edits the transcript in place after writing a `.bak-<epoch>` copy next to it. `--drop` replaces the content of the numbered events from `dejavu transcript` with a placeholder while keeping ids, types, and parent links intact, so the session still resumes. Dropping a tool call also drops its result. `--pattern` removes every line that contains the text, case-insensitively, from every string field in every record, including tool result copies stored outside the message and branches that are no longer active. `--placeholder` changes the replacement text and `--dry-run` reports without writing. Claude, Codex, Pi, and Droid files are rewritten line by line; OpenCode rows are updated in a transaction after the database file is copied.

### Search agent memory

```bash
dejavu memory list
dejavu memory list --files
dejavu memory search 'deployment boundary' --json
dejavu memory show '<project or file selector>'
```

Memory search stays separate from transcript search. Memory files contain curated facts instead of conversation turns.

### Query one session

`dejavu query` follows the source's conversation structure. It removes reasoning, developer instructions, and tool output before it calls the model. Large sessions use windows around the question terms.

The default is `codex exec --model gpt-6-luna` with medium reasoning and the OpenAI provider. It uses Codex's existing authentication, ignores user config overrides, disables project/skill instructions, and runs ephemerally in an isolated temporary directory with a read-only sandbox. Transcript text goes through stdin. The final answer comes from Codex's output file, which is deleted with the temporary directory after completion. Queries time out after 120 seconds and do not retry automatically.

Both `query` and `profile --explain` accept these options:

| Option | Environment variable | Config field | Built-in default |
| --- | --- | --- | --- |
| `--harness codex\|ruddr` | `DEJAVU_QUERY_HARNESS` | `query.harness` | `codex` |
| `--model ID` | `DEJAVU_QUERY_MODEL` | `query.model` | `gpt-6-luna` |
| `--effort LEVEL` | `DEJAVU_QUERY_EFFORT` | `query.effort` | `medium` for Codex; provider default for other Ruddr providers |

Each setting uses this precedence: flag, environment variable, config file, built-in default. Dejavu reads the optional `$XDG_CONFIG_HOME/dejavu/config.json`, or `~/.config/dejavu/config.json` when `XDG_CONFIG_HOME` is unset. Windows uses the same convention and Dejavu's existing home-directory lookup (`HOME`, then the account home). A missing file is fine. Invalid JSON or a malformed query object produces an error. Dejavu never writes this file.

```json
{"query": {"harness": "ruddr", "model": "claude/your-model-id", "effort": "high"}}
```

```bash
dejavu query '<locator>' 'What did we decide?' --model gpt-6-luna --effort high
dejavu query '<locator>' 'What did we decide?' --harness ruddr --model claude/your-model-id
dejavu profile '<locator>' --explain --harness ruddr --model codex/gpt-6-luna --effort low
```

With `--harness codex`, a bare model ID or `codex/<id>` selects Codex exec. `--effort` changes its reasoning-effort setting. No Pi configuration is read on this path, and old Pi defaults do not override Luna.

With `--harness ruddr`, the model prefix selects the Ruddr provider: `codex/`, `claude/`, `pi/`, `opencode/`, or `droid/`. A bare model ID selects Codex. Dejavu passes the remaining ID unchanged, including any additional slashes. Ruddr must be on PATH; a missing executable produces an installation hint and never triggers a fallback. Effort levels depend on the provider and model. Ruddr receives the selected level and reports unsupported settings.

Dejavu runs Ruddr in the foreground with a read-only sandbox, `--ephemeral`, an empty temporary working directory, and a separate temporary state directory. The prompt goes through stdin. Dejavu checks completion through `ruddr status --json` and reads the answer through `ruddr result`. Both directories are removed after success or failure. The Codex provider receives the same explicit lockdown config settings as Codex exec. Other providers apply Ruddr's own sandbox behavior. Providers can reject `--ephemeral`; Droid currently rejects it, and Dejavu surfaces that error. Ruddr queries share the 120-second timeout and do not retry automatically.

With the default `codex` harness, a non-Codex `provider/id` retains the legacy routing. Dejavu reads the endpoint and key from Pi config under `~/.pi/agent` (or `--agent-dir`). OpenAI-compatible providers with an API key use HTTP; other providers use the installed `pi` binary. `DEJAVU_QUERY_VIA_PI=1` forces Pi for these legacy models. This switch has no effect under `--harness ruddr`. Legacy HTTP/Pi transports do not support the new effort option; selecting an effort for them produces an error.

The query status line and JSON report the model, configured reasoning effort, transport, and token counts when available. Ruddr token counts come from `status.tokenUsage`. Estimated cost remains unknown without model pricing; Codex and Ruddr omit `costUsd`. Legacy providers with configured pricing retain their cost estimates. Profile explanations report the selected model, effort, and available token counts.

### Manage the transcript index

```bash
dejavu index status
dejavu index update
dejavu index rebuild
```

Search and `find` update the index automatically. `update` performs the same incremental refresh explicitly. `rebuild` discards the index and recreates it from the current JSONL and OpenCode stores. A schema change triggers the same rebuild on the next refresh. Set `DEJAVU_INDEX_PATH` to use another database path. The index records the store each row came from, and every lookup is limited to the stores the current environment selects, so one shared index file never returns rows from another store.

## Updates

```bash
dejavu --version
dejavu self-update --check
dejavu self-update
```

`self-update` downloads the newest release binary for this platform, verifies it against the release's `checksums.txt`, and replaces the running binary in place. An npm or Bun global install is updated with `npm install -g` or `bun add -g` instead. A source checkout reports the new version and asks for `git pull` instead. When stderr is a terminal, dejavu checks for a new release at most once a day and prints a one-line notice after the command. It skips the check for `--json`, `--quiet`, `--paths`, and piped output. Set `DEJAVU_NO_UPDATE_CHECK=1` to turn it off. The last result is cached in `~/.local/state/dejavu/update-check.json`.

## Agent guidance

Use `--json` when another agent or command consumes the result. Use one distinctive token or phrase for plain search. Use `find` when you remember several terms from the same session. Use `show` to confirm a result before you resume or query it.

Treat every result as historical evidence. Verify current files, deployments, machines, and services before acting on an earlier session. Never repeat credentials from transcript snippets.

Run `dejavu --help` for the overview. Run `dejavu find --help` or `dejavu help find` for command flags, JSON keys, a jq filter, and exit codes. Nested commands also have help: `dejavu memory search --help`.

Search JSON contains `.matches`, find JSON contains `.hits`, and memory search returns a bare array. For example:

```bash
dejavu search 'exact phrase' --json | jq '.matches[].path'
dejavu find deployment timeout --json | jq '.hits[].path'
dejavu memory search 'deployment boundary' --json | jq '.[].path'
```

Help uses color only when stdout is a terminal. Any `NO_COLOR` value or `--no-color` disables help styling. Piped help stays plain even with `--color`.

## Development

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
scripts/install-local.sh
```

With `mbx` installed, prefix the Cargo commands with `mbx` (`mbx clippy`, `mbx test`) to share compiled work across checkouts. `scripts/install-local.sh` uses mbx when it is on `PATH`.

`.github/workflows/ci.yml` runs the same checks on Linux, macOS, and Windows for every push and pull request. Pushing a `v*` tag that matches the version in `Cargo.toml` and `package.json` runs `.github/workflows/release.yml`. It tests, builds every platform binary with Cargo on native runners (static musl binaries on Linux), publishes them with checksums as a GitHub release, and publishes the npm package with the `NPM_TOKEN` repository secret.

The CLI is one Rust crate in `crates/dejavu`. It keeps JSONL search, SQLite access, transcript parsing, model access, and rendering in separate modules.

## License

MIT. See [LICENSE](LICENSE).
