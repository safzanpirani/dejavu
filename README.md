# dejavu

Agents lose useful context when work moves between Claude Code, Codex, Pi, and OpenCode. `dejavu` gives them one local command for finding earlier sessions and project memories.

`dejavu` is agent-first. An agent can search past sessions, inspect the relevant conversation, and recover decisions, commands, errors, and file changes. The agent can then verify that historical context against the current workspace. Humans can run the same commands from a terminal.

Search and transcript parsing stay on your machine. The optional `dejavu query` command sends selected conversation context through Codex exec to `gpt-5.6-luna` with medium reasoning by default. Search results can contain credentials or personal data that appeared in a transcript. Agents should treat the output as private.

Dejavu maintains an incremental SQLite full-text index under `~/.cache/dejavu/`. Before each search it parses only the appended tail of changed JSONL transcripts and pulls new or updated OpenCode messages by cursor. A typical refresh takes well under a second.

## Quick start

Install from npm:

```bash
npm install -g @safzanpirani/dejavu
# or
bun add -g @safzanpirani/dejavu
```

The package downloads the checksum-verified binary for your platform from the matching GitHub release. Bun skips install scripts for untrusted packages, so the first `dejavu` run fetches the binary instead. If no binary is available, the launcher runs the bundled source with Bun 1.4.2 or newer.

Or download the binary for your platform from the [latest release](https://github.com/safzanpirani/dejavu/releases/latest) and put it on your `PATH`:

```bash
curl -fsSL -o ~/.local/bin/dejavu https://github.com/safzanpirani/dejavu/releases/latest/download/dejavu-darwin-arm64
chmod +x ~/.local/bin/dejavu
ln -sf dejavu ~/.local/bin/deja
```

Releases ship `dejavu-darwin-arm64`, `dejavu-darwin-x64`, `dejavu-linux-x64`, `dejavu-linux-arm64`, and `dejavu-windows-x64.exe`, with SHA-256 sums in `checksums.txt`.

To run from source instead, install [Bun](https://bun.sh/) 1.4.2 or newer, clone the repository, and link the CLI:

```bash
bun install
bun link
```

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

`dejavu query` requires an installed, authenticated `codex` binary with access to `gpt-5.6-luna`. Plain search, `find`, `show`, and memory commands do not invoke a model. Explicit `--model provider/id` overrides retain the legacy HTTP/Pi transports.

## What it reads

`dejavu` detects these local stores:

| Agent | Local data |
| --- | --- |
| Claude Code | JSONL transcripts under `~/.claude/projects` |
| Codex | JSONL transcripts under `~/.codex/sessions` |
| Pi | JSONL transcripts under `~/.pi/agent/sessions` |
| OpenCode | SQLite databases under `~/.local/share/opencode`, in the legacy `part` schema or the v2 `session_message` schema |

It also reads Claude Code Markdown memory under `~/.claude/projects/*/memory/`. Set `CLAUDE_CONFIG_DIR` or pass `--root` to use another Claude store.

## Commands

### Search for an exact phrase

```bash
dejavu --source codex session-recall.ts --max-parallel 4 --json
```

Search uses case-insensitive fixed-string matching. Spaces form one exact phrase. The default source is `all`. Use `--source claude|codex|pi|opencode` to narrow the search.

The index preserves literal phrase semantics and excludes reasoning, developer instructions, and tool output. Match counts are occurrences in visible message text. Pass `--no-index` to use the direct filesystem and SQLite scanners, which count raw transcript lines and rank differently.

Each result includes its source, date, project, match count, snippets, and locator. JSONL sources return file paths. OpenCode returns `opencode://...#session-id` locators.

### Find a session from a few terms

```bash
dejavu find workshop codex colleagues
dejavu find deployment timeout --project payments-api --since 2w
```

`find` searches for multiple literal terms in one session. It ranks user-message matches above assistant-message matches. The result includes the opening prompt, matching messages, transcript path, and a resume command when the source supports one.

### Read a transcript

```bash
dejavu show '<locator>'
dejavu show '<locator>' --around database
```

`show` renders user and assistant turns without model usage. It summarizes tool calls and truncates long messages by default. Pass `--full` to disable truncation.

Use `show --no-tools` to remove tool summaries and tool-only turns. `--around` then counts only the remaining dialogue turns. `--max-chars N` changes the 700-character message limit in text and JSON output; the ` [...]` marker is additional. `--no-toolcalls` is an alias for `--no-tools` in both `show` and `transcript`.

### Pack search results into bounded excerpts

```bash
dejavu pack deployment timeout --project payments-api --budget-chars 8000 --json
dejavu pack database --context 1 --limit 3 --exclude-session '<session ID or locator>'
```

`pack` combines the session finder with user/assistant excerpts around literal matches. It uses no model. Defaults are three sessions, two neighboring dialogue events per match, 1,200 characters per event, and 12,000 event-body characters across the pack. It merges overlapping neighborhoods and shares the budget across sessions and events; actual per-event limits can be smaller. Long matching events show a region around a search term. Very small budgets can abbreviate or omit matches.

The command accepts `find` filters (`--source`, `--project`, `--since`, `--user`), `--no-index`, and `--max-parallel`. `--context 0` returns matching events only. `--exclude-session` is repeatable and accepts an exact locator or session ID. Available `CODEX_THREAD_ID` and `CLAUDE_SESSION_ID` values exclude the active session automatically. Search examines at most 40 ranked candidates; counts describe those candidates, not every stored session. Results retain relaxed search terms and report unreadable stores and sessions.

Each excerpt carries its source locator and original event IDs. `window.clipped` lists shortened fields and character offsets; `window.nextEvent` identifies the first omitted excerpt event. Read more with `transcript '<locator>' --from-event N`, or recover a shortened event with `transcript '<locator>' --full --from-event N --limit 1`. Transcript continuation reads the conversation from that ID; it does not repeat the pack's match filter.

### View a transcript turn by turn

```bash
dejavu transcript '<locator>'
dejavu transcript '<locator>' --full --thinking
dejavu transcript '<locator>' --no-tools --json
```

`transcript` renders the full conversation with labeled `USER` and `ASSISTANT` turns, timestamps, every tool call with its input, and every tool result. It works the same way for Claude, Codex, Pi, and OpenCode sessions. Tool inputs and outputs are truncated by default. Pass `--full` to print everything, `--thinking` to include model reasoning, and `--no-tools` to hide tool activity. Colors are on when stdout is a terminal. Use `--color` or `--no-color` to override.

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

`--explain` sends only bounded metrics and event references through Codex exec to `gpt-5.6-luna` at medium reasoning. It requires authenticated Codex and may incur model usage. Observations must cite supplied event IDs and remain separate from measurements. An explanation failure preserves the deterministic report.

### Redact a transcript

```bash
dejavu transcript '<locator>'                       # note the #N event numbers
dejavu scrub '<locator>' --drop 12 --drop 30-34 --dry-run
dejavu scrub '<locator>' --drop 12 --pattern "secret-host" --pattern "api key"
```

`scrub` edits the transcript in place after writing a `.bak-<epoch>` copy next to it. `--drop` replaces the content of the numbered events from `dejavu transcript` with a placeholder while keeping ids, types, and parent links intact, so the session still resumes. Dropping a tool call also drops its result. `--pattern` removes every line that contains the text, case-insensitively, from every string field in every record, including tool result copies stored outside the message and branches that are no longer active. `--placeholder` changes the replacement text and `--dry-run` reports without writing. Claude, Codex, and Pi files are rewritten line by line; OpenCode rows are updated in a transaction after the database file is copied.

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

The default is `codex exec --model gpt-5.6-luna` with medium reasoning and the OpenAI provider. It uses Codex's existing authentication, ignores user config overrides, disables project/skill instructions, and runs ephemerally in an isolated temporary directory with a read-only sandbox. Transcript text goes through stdin. The final answer comes from Codex's output file, which is deleted with the temporary directory after completion. Queries time out after 120 seconds and do not retry automatically.

Use `--model <codex-model-id>` or `--model codex/<id>` to select another Codex model, still with medium reasoning. No Pi configuration is read on this path, and old Pi defaults do not override Luna.

For an explicit legacy `--model provider/id`, Dejavu reads the endpoint and key from the Pi config under `~/.pi/agent` (or `--agent-dir`). OpenAI-compatible providers with an API key use HTTP; other providers use the installed `pi` binary. `DEJAVU_QUERY_VIA_PI=1` forces Pi only for these explicit legacy overrides.

The status line and JSON report the model, reasoning effort for Codex, transport, and token counts when available. Estimated cost is reported only for legacy providers with configured pricing; Codex queries do not invent a dollar estimate.

### Manage the transcript index

```bash
dejavu index status
dejavu index update
dejavu index rebuild
```

Search and `find` update the index automatically. `update` performs the same incremental refresh explicitly. `rebuild` discards the index and recreates it from the current JSONL and OpenCode stores. A schema change triggers the same rebuild on the next refresh. Set `DEJAVU_INDEX_PATH` to use another database path.

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

Run `dejavu --help` for the complete flag reference.

## Development

```bash
bun run check
bun run build:local
```

Pushing a `v*` tag that matches the `package.json` version runs `.github/workflows/release.yml`. It runs the checks, builds every platform binary, publishes them with checksums as a GitHub release, and publishes the npm package through npm trusted publishing.

The macOS build script applies an ad hoc signature because Bun 1.4 can emit an invalid arm64 signature. The code keeps JSONL search, SQLite access, transcript parsing, model access, and rendering in separate modules.

## License

MIT. See [LICENSE](LICENSE).
