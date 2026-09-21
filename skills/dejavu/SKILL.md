---
name: dejavu
description: Search and query past Claude Code, Codex, Pi, and OpenCode transcripts, search Claude project memories across every workspace, and read or write the cross-harness per-project memory store. Use for earlier agent conversations, decisions, commands, errors, and curated project memory. Do not use for shell history, Git history, or repository code search.
---

# Dejavu

Use `dejavu` to recover context from local coding-agent transcripts. Run `dejavu --help` before guessing flags. Prefer `--json` when another command or agent will consume the result.

## Search Claude memory across projects

```sh
dejavu memory list
dejavu memory list --files
dejavu memory search 'exact phrase' --json
dejavu memory show '<unique project substring or file path>'
```

Memory commands read Markdown under `~/.claude/projects/*/memory/`. They search curated memory separately from raw transcripts and never modify it. A project selector with several topic files resolves to its `MEMORY.md` index. Use the exact `project/name` from `memory list --files` when a selector is ambiguous. Set `CLAUDE_CONFIG_DIR` or pass `--root` for another Claude store.

## Record and recall project memory

`dejavu memory project` is a separate, writable store that all four harnesses share. Read it at the start of project work, and write to it when a task produces a durable lesson.

```sh
dejavu memory project recall --cwd "$PWD" --query '<what you are about to do>' --json
dejavu memory project list --cwd "$PWD" --json
dejavu memory project get '<memory id>' --cwd "$PWD" --json
dejavu memory project add --cwd "$PWD" --file memory.json --harness codex --json
dejavu memory project update '<id>' --if-revision 3 --cwd "$PWD" --file fix.json --json
dejavu memory project supersede '<id>' --if-revision 3 --cwd "$PWD" --file replacement.json --json
```

Read `data.context` from `recall` and treat it as historical claims: verify relevant code and services before relying on it, and let the current user instruction win. Identity is a project UUID bound to canonical roots, so worktrees share records and separate clones do not; never assume a display slug is the identity.

Mutate only through `add`, `update`, `supersede`, `archive`, and `purge`, and always pass the current `--if-revision`. A stale revision returns exit 3 — re-read the record and reconcile instead of retrying the same payload. Pass `--request-id` when a retry could duplicate a write.

Write only things a future agent needs: what applies, where, why it is believed, and when it goes stale. Kinds are `decision`, `convention`, `procedure`, `pitfall`, and `handoff`. Give `handoff` an expiry (seven days by default). Never store credentials, full transcripts, or transient task status. Imported Claude files (`import-claude`) land as `unverified` and stay out of `recall` until confirmed.

## Find a session from a vague memory

When the request is "find that chat where we ...", use `dejavu find` with two or three literal terms. It requires all terms per session (falling back to the best subset), weights user-message matches above assistant ones, and prints a session card: opening user prompt, matching user messages with dates, per-term counts, transcript path, and a resume command for Claude and Codex sessions.

```sh
dejavu find workshop codex colleagues
dejavu find deploy timeout --project payments-api --since 2w
dejavu find controlmaster --user --source claude
```

Flags: `-p/--project SUBSTR` filters by project path, `--since` takes `YYYY-MM-DD` or `7d`/`2w`/`3m`, `--user` requires every term in user messages, `-n` limits results, `--paths` prints only locators (one per line, for piping into `dejavu show` or `dejavu query`), and `--max-parallel N` bounds local store and candidate work with a default of 4. Resume commands cover Claude (`claude --resume`), Codex (`codex resume`), and Pi (`pi --session <path>`). Prefer `dejavu find` over plain search whenever the goal is identifying a whole session rather than a phrase.

## Read a transcript without model cost

For bounded context in one call, use `dejavu pack <term>... --project SUBSTR --budget-chars 8000 --json`. It runs the session finder and returns user/assistant excerpts around matches, with overlapping neighborhoods merged. Defaults: 3 sessions, 2 neighboring dialogue events, 1200 characters per event, 12000 total event-body characters. It shares the budget across sessions/events and focuses long excerpts around a term. `--context 0` returns only matching events. `--exclude-session ID_OR_LOCATOR` is repeatable; active `CODEX_THREAD_ID`/`CLAUDE_SESSION_ID` values are automatically excluded when present. It accepts find's source, project, since, user, no-index, and max-parallel flags. Search considers at most 40 ranked candidates, not a complete catalog. Inspect `requiredTerms` for relaxed matching and `skippedStores`/`skippedSessions` for unavailable data.

Use `show --no-tools` for only user/assistant content without tool summaries or tool-only turns. `show --max-chars N` changes the per-message limit in text and JSON. `--no-toolcalls` aliases `--no-tools` for both show and transcript.

For a bounded event view, use `transcript <locator> --no-tools --max-chars 1500 --budget-chars 8000 --json`. `--tool-chars N` caps tool input/output bodies; `--from-event N --limit N` paginates by stable event IDs after filtering. Explicit bounds apply to JSON; unbounded JSON stays complete. Budgets count event-body characters including ellipses, excluding labels, JSON encoding, and metadata. Shortened structured tool inputs become preview strings, marked in `window.clipped`. Follow `window.nextEvent` for omitted events, and recover clipped content separately with `transcript <locator> --full --from-event N --limit 1` (add `--thinking` for reasoning). A pack's nextEvent resumes the conversation, not the match filter. `--full` cannot be combined with character limits.

`dejavu show <locator>` prints the parsed conversation as `[user]`/`[assistant]` turns (tool calls summarized, long messages truncated; `--full` disables truncation). `--around TERM` prints only messages containing TERM plus three turns of context — use it to jump to the relevant region of a long session. Use `show` to confirm a session is the right one before resuming it or paying for `dejavu query`.

`dejavu transcript <locator>` prints the full turn-by-turn view: labeled `USER` / `ASSISTANT` turns with timestamps, each tool call with its input (`▶ name`), and each tool result (`◀ name result`, or `◀ name error`). It works identically for Claude, Codex, Pi, and OpenCode. Tool inputs and outputs are truncated by default; `--full` prints everything, `--thinking` adds model reasoning, `--no-tools` hides tool activity, and `--json` emits the event list (`kind` is `user`, `assistant`, `thinking`, `tool_call`, or `tool_result`). Use `transcript` over `show` when the question is what the agent actually ran and what came back.

## Redact a transcript

`dejavu scrub <locator>` rewrites a transcript in place after saving a `.bak-<epoch>` copy. Use `--drop N` (repeatable, ranges like `30-34`) with the `#N` numbers from `dejavu transcript` to replace a user turn, assistant turn, thinking block, or tool call and its result with `[redacted]`; ids, types, and parent links stay so the session still resumes. Use `--pattern TEXT` (repeatable) to delete every line containing the text from every string field in every record, which also covers tool result copies stored outside the message and inactive branches. Run with `--dry-run` first, report the counts, and remind the user that an agent process that already loaded the session keeps the old content until it restarts. Never print the redacted content back.

## Find a transcript

Search all detected stores by default:

```sh
dejavu --json 'session-recall.ts'
dejavu --json 'Cannot find module'
```

The search covers Claude Code, Codex, Pi, and OpenCode. Narrow it only when the user names a source or broad results are noisy:

```sh
dejavu --source claude --max-parallel 4 --json 'distinctive phrase'
dejavu --source codex --json 'functionName'
dejavu --source pi --json 'package-name'
dejavu --source opencode --json 'exact error text'
```

Search is case-insensitive fixed-string matching, not semantic search. Spaces mean exact spaces. Use one distinctive token or phrase. Run separate searches for unrelated terms, then compare their locators. `--max-parallel N` also bounds local store and file work for plain search. It defaults to 4 and requires an integer greater than or equal to 1. Results remain deterministic when work completes out of order. Dejavu reports unreadable OpenCode SQLite stores on stderr and continues with readable stores. JSON results include the same paths in `skippedStores`.

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

`--explain` sends only bounded metrics and event references through Codex exec to `gpt-5.6-luna` at medium reasoning. It requires authenticated Codex and may incur model usage. Observations must cite supplied event IDs and remain separate from measurements. An explanation failure preserves the deterministic report.

## Ask about one transcript

Use a focused question after selecting a result:

```sh
dejavu query '<locator from search results>' 'What did we decide, and which files changed?' --json
```

`dejavu query` sends the selected conversation context through `codex exec` to `gpt-5.6-luna` with medium reasoning by default and may incur model usage. It requires an authenticated Codex CLI. Query only the transcript needed for the request. The loader removes thinking, developer instructions, and tool output. It follows branches where the source supports them and windows large transcripts around question terms. The model-backed query stays serial and does not accept `--max-parallel`.

Use `--model <codex-model-id>` or `--model codex/<id>` for a Codex override, still at medium reasoning. The default ignores Pi model settings. Codex runs ephemerally with transcript input on stdin, a read-only sandbox, project/skill instructions disabled, and a 120-second timeout. It uses the OpenAI provider and existing Codex authentication without changing user configuration. An explicit non-Codex `--model provider/id` retains the legacy HTTP/Pi path and reads provider settings from `~/.pi/agent` or `--agent-dir`. Do not rewrite model configuration unless the user asks.

## Report the result

Answer the user's question. Include the source and locator when they help the user inspect or resume the conversation. Treat transcript facts as historical evidence. Verify current files, deployments, hosts, and services separately when the answer depends on present state.

If `dejavu` is not on `PATH`, use the source checkout:

```sh
bun run /path/to/dejavu/src/cli.ts --help
```
