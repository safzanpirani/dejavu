---
name: dejavu
description: Search and query past Claude Code, Codex, Pi, and OpenCode transcripts, or search and read Claude project memories across every workspace. Use for earlier agent conversations, decisions, commands, errors, and curated cross-project memory. Do not use for shell history, Git history, or repository code search.
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

## Find a session from a vague memory

When the request is "find that chat where we ...", use `dejavu find` with two or three literal terms. It requires all terms per session (falling back to the best subset), weights user-message matches above assistant ones, and prints a session card: opening user prompt, matching user messages with dates, per-term counts, transcript path, and a resume command for Claude and Codex sessions.

```sh
dejavu find workshop codex colleagues
dejavu find deploy timeout --project payments-api --since 2w
dejavu find controlmaster --user --source claude
```

Flags: `-p/--project SUBSTR` filters by project path, `--since` takes `YYYY-MM-DD` or `7d`/`2w`/`3m`, `--user` requires every term in user messages, `-n` limits results, `--paths` prints only locators (one per line, for piping into `dejavu show` or `dejavu query`), and `--max-parallel N` bounds local store and candidate work with a default of 4. Resume commands cover Claude (`claude --resume`), Codex (`codex resume`), and Pi (`pi --session <path>`). Prefer `dejavu find` over plain search whenever the goal is identifying a whole session rather than a phrase.

## Read a transcript without model cost

`dejavu show <locator>` prints the parsed conversation as `[user]`/`[assistant]` turns (tool calls summarized, long messages truncated; `--full` disables truncation). `--around TERM` prints only messages containing TERM plus three turns of context — use it to jump to the relevant region of a long session. Use `show` to confirm a session is the right one before resuming it or paying for `dejavu query`.

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

## Ask about one transcript

Use a focused question after selecting a result:

```sh
dejavu query '<locator from search results>' 'What did we decide, and which files changed?' --json
```

`dejavu query` sends the selected conversation context to the configured Pi model and may incur model usage. Query only the transcript needed for the request. The loader removes thinking, developer instructions, and tool output. It follows branches where the source supports them and windows large transcripts around question terms. The model-backed query stays serial and does not accept `--max-parallel`.

Model selection is `--model provider/id`, then `~/.pi/agent/session-recall.json`, then Pi's default model. Do not rewrite model configuration unless the user asks.

## Report the result

Answer the user's question. Include the source and locator when they help the user inspect or resume the conversation. Treat transcript facts as historical evidence. Verify current files, deployments, hosts, and services separately when the answer depends on present state.

If `dejavu` is not on `PATH`, use the source checkout:

```sh
bun run /path/to/dejavu/src/cli.ts --help
```
