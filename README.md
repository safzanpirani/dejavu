# dejavu

Agents lose useful context when work moves between Claude Code, Codex, Pi, and OpenCode. `dejavu` gives them one local command for finding earlier sessions and project memories.

`dejavu` is agent-first. An agent can search past sessions, inspect the relevant conversation, and recover decisions, commands, errors, and file changes. The agent can then verify that historical context against the current workspace. Humans can run the same commands from a terminal.

Search and transcript parsing stay on your machine. The optional `dejavu query` command sends selected conversation context to your configured Pi model. Search results can contain credentials or personal data that appeared in a transcript. Agents should treat the output as private.

## Quick start

You need [Bun](https://bun.sh/) 1.4 or newer. Clone the repository and link the CLI:

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
dejavu memory search 'deployment boundary'
```

Ask a model to summarize one selected session:

```bash
dejavu query '<locator from search results>' 'What did we decide?'
```

`dejavu query` requires an installed [Pi coding agent](https://github.com/earendil-works/pi). Plain search, `find`, `show`, and memory commands do not invoke a model.

## What it reads

`dejavu` detects these local stores:

| Agent | Local data |
| --- | --- |
| Claude Code | JSONL transcripts under `~/.claude/projects` |
| Codex | JSONL transcripts under `~/.codex/sessions` |
| Pi | JSONL transcripts under `~/.pi/agent/sessions` |
| OpenCode | SQLite databases under `~/.local/share/opencode` |

It also reads Claude Code Markdown memory under `~/.claude/projects/*/memory/`. Set `CLAUDE_CONFIG_DIR` or pass `--root` to use another Claude store.

## Commands

### Search for an exact phrase

```bash
dejavu --source codex session-recall.ts --max-parallel 4 --json
```

Search uses case-insensitive fixed-string matching. Spaces form one exact phrase. The default source is `all`. Use `--source claude|codex|pi|opencode` to narrow the search.

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

### Search agent memory

```bash
dejavu memory list
dejavu memory list --files
dejavu memory search 'deployment boundary' --json
dejavu memory show '<project or file selector>'
```

Memory search stays separate from transcript search. Memory files contain curated facts instead of conversation turns.

### Query one session

`dejavu query` follows the source's conversation structure. It removes reasoning, developer instructions, and tool output before it invokes Pi. Large sessions use windows around the question terms.

Model selection follows this order:

1. `--model provider/id`
2. `~/.pi/agent/session-recall.json`
3. Pi's default provider and model in `settings.json`

## Agent guidance

Use `--json` when another agent or command consumes the result. Use one distinctive token or phrase for plain search. Use `find` when you remember several terms from the same session. Use `show` to confirm a result before you resume or query it.

Treat every result as historical evidence. Verify current files, deployments, machines, and services before acting on an earlier session. Never repeat credentials from transcript snippets.

Run `dejavu --help` for the complete flag reference.

## Development

```bash
bun run check
bun run build:local
```

The macOS build script applies an ad hoc signature because Bun 1.4 can emit an invalid arm64 signature. The code keeps JSONL search, SQLite access, transcript parsing, model access, and rendering in separate modules.

## License

MIT. See [LICENSE](LICENSE).
