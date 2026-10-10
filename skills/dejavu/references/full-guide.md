<!-- The full dejavu guide as of 2026-10-10 (dejavu 0.6.3), kept verbatim. SKILL.md is the short core. -->

# Dejavu

Use `dejavu` to recover context from local coding-agent transcripts. Run `dejavu --help` for the overview. Run `dejavu <command> --help` or `dejavu help <command>` before guessing flags or JSON fields. Nested help also works: `dejavu memory search --help`. Command help lists flags, top-level JSON keys, a jq command, and exit codes. Piped help contains no ANSI styling. Any `NO_COLOR` value or `--no-color` disables help color. Prefer `--json` when another command or agent will consume the result.

## Read the JSON shape

Use these jq paths:

```sh
dejavu search 'exact phrase' --json | jq '.matches[].path'
dejavu find deployment timeout --json | jq '.hits[].path'
dejavu memory search 'exact phrase' --json | jq '.[].path'
dejavu transcript '<locator>' --json | jq '.events[]'
```

Memory list and search return bare arrays. Transcript returns an object with an `events` array. Search and find return exit 0 for zero matches. `last` returns exit 1 for no session. Read each command's help for its error and partial-result rules.

## Search Claude memory across projects

```sh
dejavu memory list
dejavu memory list --files
dejavu memory search 'exact phrase' --json
dejavu memory show '<unique project substring or file path>'
```

Memory commands read Markdown under `~/.claude/projects/*/memory/`. They search curated memory separately from raw transcripts and never modify it. A project selector with several topic files resolves to its `MEMORY.md` index. Use the exact `project/name` from `memory list --files` when a selector is ambiguous. Set `CLAUDE_CONFIG_DIR` or pass `--root` for another Claude store.

## Continue where a session left off

When the user says "continue where the last session left off", "pick up from yesterday's session", or "continue where the <x> session left off", run `dejavu last` and resume the work from its output.

```sh
dejavu last                              # the previous session in this repo
dejavu last --list -n 5                  # pick one when "last" is ambiguous
dejavu last <x terms>                    # "the <x> session": find's best match
dejavu last '<locator or session id>'    # a session the user named exactly
dejavu last --anywhere --since 2d        # newest session in any project
```

With no argument it picks the newest session in the current Git repo (or a subdirectory) and skips the session you are running in, so "last session" is the one before this one. Terms rank sessions like `dejavu find` within the current repo, and widen to every project when nothing there matches. `-p SUBSTR` pins one project and `--anywhere` searches all of them. `--source codex` limits to one agent. The output is a session card (date, source, project, opening prompt, last real user request, transcript, resume command) followed by the newest user/assistant turns within 8,000 characters. `--tools` adds tool calls and results. `--turns N` and `--budget-chars N` widen the tail. The last line gives a `dejavu transcript ... --from-event N` command for earlier turns.

Then:

1. Say which session you picked (date, source, opening prompt) in one line. If several recent sessions could be "the <x> session", run `dejavu last --list` and ask the user which one before continuing.
2. Read the tail for the open thread: the last user request, what the agent finished, what it said was next, and anything left pending or failing. Read earlier turns with the printed `transcript --from-event` command when the tail starts mid-task.
3. Check current state before acting. Run `git status` and `git log`, and read the files the session touched. The transcript is history, and another session may have changed things since.
4. Continue the work. Do not tell the user to run `claude --resume` unless they ask to reopen the old session itself.

## Find a session from a vague memory

When the request is "find that chat where we ...", use `dejavu find` with two or three literal terms. It requires all terms per session (falling back to the best subset), weights user-message matches above assistant ones, and prints a session card: opening user prompt, matching user messages with dates, per-term counts, transcript path, and a resume command for Claude, Codex, Pi, Droid, agy, and OpenCode sessions.

```sh
dejavu find workshop codex colleagues
dejavu find deploy timeout --project payments-api --since 2w
dejavu find controlmaster --user --source claude
```

OpenCode cards keep their project directory. `--since` uses matching message dates and falls back to session activity when dates are missing. A resumed session can match after its filename date. Harness instruction blocks do not count as user matches or opening prompts. Find excerpts show a 240-character window around a matching term. Find and last cards cap opening prompts at 300 characters and mark cuts with an ellipsis. Use `transcript --full` for the complete prompt. Counts cover all matching visible messages in each scored session, including messages after large tool logs. In mixed queries, long terms select candidates and short terms are checked only inside those sessions. Queries with only short terms scan directly. Source, project, and date filters apply before the 40-candidate scoring limit. Inspect `truncated` in find JSON. A true value means eligible candidates were omitted; narrow the filters for more coverage.

Flags: `-p/--project SUBSTR` filters by project path, `--since` takes `YYYY-MM-DD` or `7d`/`2w`/`3m`, `--user` requires every term in user messages, `-n` limits results, `--paths` prints only locators (one per line, for piping into `dejavu show` or `dejavu query`), and `--max-parallel N` bounds local store and candidate work with a default of 4. Resume commands cover Claude (`claude --resume`), Codex (`codex resume`), Pi (`pi --session <path>`), Droid (`droid --resume <id>`), and agy (`agy --conversation <id>`). Prefer `dejavu find` over plain search whenever the goal is identifying a whole session rather than a phrase.

## Read a transcript without model cost

For bounded context in one call, use `dejavu pack <term>... --project SUBSTR --budget-chars 8000 --json`. It runs the session finder and returns user/assistant excerpts around matches, with overlapping neighborhoods merged. Defaults: 3 sessions, 2 neighboring dialogue events, 1200 characters per event, 12000 total event-body characters. It ranks neighborhoods by match density, gives user dialogue extra weight, and selects fewer events before sharing the budget. It aims for 200 body characters per excerpt when limits permit and reserves a metadata allowance. Long excerpts center on a term. JSON uses compact formatting and retains event metadata. `omitted` lists omitted match-anchor counts, event counts, locators, and `nextEvent` values per loaded session. `--context 0` returns only matching events. `--exclude-session ID_OR_LOCATOR` is repeatable; active `CODEX_THREAD_ID`/`CLAUDE_CODE_SESSION_ID`/`CLAUDE_SESSION_ID`/`ANTIGRAVITY_CONVERSATION_ID`/`DROID_SESSION_ID` values and the running Droid session are automatically excluded. It accepts find's source, project, since, user, no-index, and max-parallel flags. Search considers at most 40 ranked candidates, not a complete catalog. Inspect `requiredTerms` for relaxed matching and `skippedStores`/`skippedSessions` for unavailable data.

Use `show --no-tools` for only user/assistant content without tool summaries or tool-only turns. `show --max-chars N` changes the per-message limit in text and JSON. `show --around TERM` centers matching excerpts on the first case-insensitive occurrence and marks cut sides with `…`. `--no-toolcalls` aliases `--no-tools` for both show and transcript.

For a bounded event view, use `transcript <locator> --no-tools --max-chars 1500 --budget-chars 8000 --json`. `--tool-chars N` caps tool input/output bodies; `--from-event N --limit N` paginates by stable event IDs after filtering. Explicit bounds apply to JSON; unbounded JSON stays complete. Budgets count event-body characters including ellipses, excluding labels, JSON encoding, and metadata. Shortened structured tool inputs become preview strings, marked in `window.clipped`. Follow `window.nextEvent` for omitted events, and recover clipped content separately with `transcript <locator> --full --from-event N --limit 1` (add `--thinking` for reasoning). A pack's nextEvent resumes the conversation, not the match filter. `--full` cannot be combined with character limits.

`dejavu show <locator>` prints the parsed conversation as `[user]`/`[assistant]` turns (tool calls summarized, long messages truncated; `--full` disables truncation). `--around TERM` prints only messages containing TERM plus three turns of context — use it to jump to the relevant region of a long session. Use `show` to confirm a session is the right one before resuming it or paying for `dejavu query`.

`dejavu transcript <locator>` prints the full turn-by-turn view: labeled `USER` / `ASSISTANT` turns with timestamps, each tool call with its input (`▶ name`), and each tool result (`◀ name result`, or `◀ name error`). It works identically for Claude, Codex, Pi, OpenCode, Droid, and agy. Tool inputs and outputs are truncated by default; `--full` prints everything, `--thinking` adds model reasoning, `--no-tools` hides tool activity, and `--json` emits an object with an `events` array (`kind` is `user`, `assistant`, `thinking`, `tool_call`, or `tool_result`). Use `transcript` over `show` when the question is what the agent actually ran and what came back.

## Redact a transcript

`dejavu scrub <locator>` rewrites a transcript in place after saving a `.bak-<epoch>` copy. Use `--drop N` (repeatable, ranges like `30-34`) with the `#N` numbers from `dejavu transcript` to replace a user turn, assistant turn, thinking block, or tool call and its result with `[redacted]`; ids, types, and parent links stay so the session still resumes. Use `--pattern TEXT` (repeatable) to delete every line containing the text from every string field in every record, which also covers tool result copies stored outside the message and inactive branches. Run with `--dry-run` first, report the counts, and remind the user that an agent process that already loaded the session keeps the old content until it restarts. Never print the redacted content back. Scrub refuses agy conversations, because agy keeps other copies that dejavu cannot redact.

## Find a transcript

Search all detected stores by default. Each agent's own variable selects its store instead of the home default: `CLAUDE_CONFIG_DIR` (Claude `$CLAUDE_CONFIG_DIR/projects`), `CODEX_HOME` (`$CODEX_HOME/sessions` and `$CODEX_HOME/archived_sessions`), `PI_CODING_AGENT_DIR` (`$PI_CODING_AGENT_DIR/sessions`; unset, Pi covers every `~/.pi/*/sessions` profile), `XDG_DATA_HOME` or `OPENCODE_DB` (OpenCode `$XDG_DATA_HOME/opencode/*.db`), and `FACTORY_HOME_OVERRIDE` (Droid `$FACTORY_HOME_OVERRIDE/.factory/sessions`). omp reads `~/.omp/agent/sessions` and each `~/.omp/profiles/*/agent/sessions` profile. `OPENCLAW_STATE_DIR` (default `~/.openclaw`) selects OpenClaw, whose agents keep sessions in SQLite, and `HERMES_HOME` (default `~/.hermes`) selects Hermes's `state.db`; their locators are `openclaw://...#id` and `hermes://...#id`, and `scrub` refuses them. agy has no variable and always reads `~/.gemini/antigravity-cli/brain`. Keep these set to search only the current user's history on a shared account:

```sh
dejavu --json 'session-recall.ts'
dejavu search 'Cannot find module' --json
```

The search covers Claude Code, Codex, Pi, omp, OpenCode, Factory Droid, OpenClaw, Hermes, and agy. Narrow it only when the user names a source or broad results are noisy:

```sh
dejavu --source claude --max-parallel 4 --json 'distinctive phrase'
dejavu --source codex --json 'functionName'
dejavu --source pi --json 'package-name'
dejavu --source omp --json 'package-name'
dejavu --source openclaw --json 'telegram'
dejavu --source hermes --json 'exact error text'
dejavu --source opencode --json 'exact error text'
dejavu --source droid --json 'exact error text'
dejavu --source agy --json 'exact error text'
```

Use `dejavu search PHRASE` or the bare `dejavu PHRASE` form. Both perform the same search. Use `dejavu -- search` or `dejavu search search` for the literal word `search`. A quoted phrase such as `dejavu "search failed"` stays literal.

Search uses case-insensitive fixed-string matching. Spaces mean exact spaces. Use one distinctive token or phrase. Run separate searches for unrelated terms, then compare their locators. `--max-parallel N` also bounds local store and file work for plain search. It defaults to 4 and requires an integer greater than or equal to 1. Results remain deterministic when work completes out of order. Dejavu reports unreadable OpenCode SQLite stores on stderr and continues with readable stores. JSON results include the same paths in `skippedStores`.

Good anchors include filenames, symbols, package names, issue IDs, exact error fragments, host names, and unusual terms. If a search returns nothing, shorten the phrase or try another exact anchor.

Each result includes a source, date, project, match count, snippets, and locator. OpenCode locators start with `opencode://`; pass them back unchanged. Search results can include historical user text. Never repeat credentials or other secrets found in snippets.

## Profile tool activity

```bash
dejavu profile '<transcript-locator>' --json
dejavu profile --project dejavu --since 7d --limit 10 --json
dejavu profile '<transcript-locator>' --explain --json
```

The default is deterministic and invokes no model. It measures outer calls, result characters, identical-input repeats, error flags, observation calls, and recognizable nested `tools.name()` call sites. JSON preserves event IDs for inspection with `dejavu transcript`; it omits raw prompts, inputs, and outputs. `--output-threshold N` changes the oversized-result threshold from 10,000 characters.

Repeated calls are candidates for review, not proven waste. Nested call sites are lexical hints, not executed counts; aliases, loops, templates, and computed access limit coverage. First-result latency includes waiting and is not model reasoning time. Project mode selects sessions by their last indexed visible-message date and measures each entire selected session. Check `omittedSessions` and `diagnostics` for coverage limits. Exit 1 signals skipped sources or an explanation failure even when measurements are available.

`--explain` sends only bounded metrics and event references to the selected query harness and model. It defaults to Codex exec with `gpt-6-luna` at medium reasoning. It uses the same flags and persistent defaults as `query` and may incur model usage. Observations must cite supplied event IDs and remain separate from measurements. An explanation failure preserves the deterministic report.

## Ask about one transcript

Use a focused question after selecting a result:

```sh
dejavu query '<locator from search results>' 'What did we decide, and which files changed?' --json
```

`dejavu query` sends the selected conversation context through `codex exec` to `gpt-6-luna` with medium reasoning by default and may incur model usage. It requires an authenticated Codex CLI. Query only the transcript needed for the request. The loader removes thinking, developer instructions, and tool output. It follows branches where the source supports them and windows large transcripts around question terms. The model-backed query stays serial and does not accept `--max-parallel`.

Both `query` and `profile --explain` accept these settings:

| Flag | Environment variable | Config field | Default |
| --- | --- | --- | --- |
| `--harness codex\|ruddr` | `DEJAVU_QUERY_HARNESS` | `query.harness` | `codex` |
| `--model ID` | `DEJAVU_QUERY_MODEL` | `query.model` | `gpt-6-luna` |
| `--effort LEVEL` | `DEJAVU_QUERY_EFFORT` | `query.effort` | `medium` for Codex; provider default for other Ruddr providers |

Each setting uses flag, environment variable, config file, then built-in default. The optional config file is `$XDG_CONFIG_HOME/dejavu/config.json` or `~/.config/dejavu/config.json`. Its format is `{"query":{"harness":"codex","model":"gpt-6-luna","effort":"medium"}}`. Dejavu never writes it.

Use `--model <codex-model-id>` or `--model codex/<id>` for a Codex override. Use `--effort high` to change reasoning effort. `--harness ruddr` requires an installed, authenticated Ruddr. Its model prefixes select `codex/`, `claude/`, `pi/`, `omp/`, `opencode/`, or `droid/`. A bare ID selects Codex. Ruddr runs ephemerally with a read-only sandbox and a 120-second timeout. Provider failures are reported without fallback. Legacy HTTP/Pi routes reject `--effort`.

The default ignores Pi model settings. Codex runs ephemerally with transcript input on stdin, a read-only sandbox, project/skill instructions disabled, and a 120-second timeout. It uses the OpenAI provider and existing Codex authentication without changing user configuration. An explicit non-Codex `--model provider/id` retains the legacy HTTP/Pi path and reads provider settings from `~/.pi/agent` or `--agent-dir`. Do not rewrite model configuration unless the user asks.

## Report the result

Answer the user's question. Include the source and locator when they help the user inspect or resume the conversation. Treat transcript facts as historical evidence. Verify current files, deployments, hosts, and services separately when the answer depends on present state.

If `dejavu` is not on `PATH`, build it from the source checkout:

```sh
cd /path/to/dejavu && cargo build --release
/path/to/dejavu/target/release/dejavu --help
```
