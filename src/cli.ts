#!/usr/bin/env bun
import { homedir } from "node:os";
import { join } from "node:path";
import { DEFAULT_MAX_PARALLEL } from "./concurrency.ts";
import { DEFAULT_SEARCH_LIMIT, DEFAULT_SNIPPET_LIMIT, querySession, searchSessions } from "./core.ts";
import { DEFAULT_FIND_LIMIT, findSessions, showSession } from "./find.ts";
import { defaultMemoryRoot, listMemories, memoryFiles, searchMemories, showMemory } from "./memory.ts";
import { renderFind, renderQuery, renderSearch, renderShow, renderTranscript } from "./render.ts";
import { discoverTranscriptStores, parseSource } from "./source-registry.ts";
import { refreshTranscriptIndex, transcriptIndexStatus } from "./transcript-index.ts";
import type { StoreDiagnostic } from "./transcript-types.ts";
import { viewTranscript } from "./transcript-view.ts";
import { DEFAULT_PLACEHOLDER, parseDropList, scrubTranscript } from "./transcript-scrub.ts";
import { explainProfile, profileSessions, renderProfile } from "./profile.ts";
import { packSessions, renderPack } from "./pack.ts";
import { renderWindow, validateBound, windowTranscript } from "./transcript-window.ts";
import { availableUpdate, compareVersions, DISABLE_CHECK_ENV, selfUpdate, VERSION } from "./update.ts";

const colors = {
  red: (text: string) => `\x1b[31m${text}\x1b[0m`,
  dim: (text: string) => `\x1b[90m${text}\x1b[0m`,
  bold: (text: string) => `\x1b[1m${text}\x1b[0m`,
};

const HELP = `${colors.bold("dejavu")}: search and query coding-agent transcripts

  ${colors.bold("dejavu")} <token-or-exact-phrase> [flags]
  ${colors.bold("dejavu find")} <term> [term...] [flags]
  ${colors.bold("dejavu pack")} <term> [term...] [flags]
  ${colors.bold("dejavu show")} <transcript-locator> [flags]
  ${colors.bold("dejavu transcript")} <transcript-locator> [flags]
  ${colors.bold("dejavu scrub")} <transcript-locator> [--drop N|A-B]... [--pattern TEXT]... [flags]
  ${colors.bold("dejavu query")} <transcript-locator> <question> [flags]
  ${colors.bold("dejavu profile")} <transcript-locator>... [--explain] [--json]
  ${colors.bold("dejavu profile")} --project SUBSTR [--since 7d] [--limit 10] [--explain]
  ${colors.bold("dejavu memory list")} [--files] [--root DIR] [--json]
  ${colors.bold("dejavu memory search")} <phrase> [--limit N] [--snippets N] [--root DIR] [--json]
  ${colors.bold("dejavu memory show")} <project-or-file> [--root DIR] [--json]
  ${colors.bold("dejavu index")} <status|update|rebuild> [--json]
  ${colors.bold("dejavu self-update")} [--check] [--json]
  ${colors.bold("dejavu --version")}

search flags
  -s, --source NAME      all, claude, codex, pi, or opencode (default all)
  -n, --limit N          transcripts to return (default ${DEFAULT_SEARCH_LIMIT})
      --snippets N       snippets per transcript (integer >= 1; default ${DEFAULT_SNIPPET_LIMIT})
      --max-parallel N   local store/file workers (default ${DEFAULT_MAX_PARALLEL})
      --no-index         bypass the transcript index and scan files directly

find flags (multi-term session finder, ranked, user messages weighted)
  -s, --source NAME      restrict to one source
  -n, --limit N          sessions to return (default ${DEFAULT_FIND_LIMIT})
  -p, --project SUBSTR   only sessions whose project path contains SUBSTR
      --since WHEN       YYYY-MM-DD or 7d / 2w / 3m
      --user             require every term to appear in user messages
      --paths            print matching transcript locators only, one per line
      --max-parallel N   local store/candidate workers (default ${DEFAULT_MAX_PARALLEL})
      --no-index         bypass the transcript index and scan files directly

show flags
      --full             do not truncate long messages
      --around TERM      only messages containing TERM, with 3 turns of context
      --no-tools         only user/assistant content, without tool summaries
      --max-chars N      characters per message (default 700; applies to JSON)

pack flags (model-free search plus user/assistant excerpts)
      --limit N          sessions to return (default 3; search cap 40)
      --budget-chars N   total event-body characters (default 12000)
      --max-chars N      maximum characters per event (default 1200)
      --context N        neighboring dialogue events per match (default 2; 0 allowed)
      --exclude-session ID_OR_LOCATOR
                         repeatable; active session IDs from the environment are excluded
                         also accepts find's source, project, since, user, no-index, max-parallel

transcript flags (turn-by-turn view with tool calls and results)
      --full             do not truncate messages, tool inputs, or tool outputs
      --thinking         include model thinking blocks
      --no-tools         hide tool calls and tool results
      --max-chars N      cap dialogue/thinking event bodies, including JSON
      --tool-chars N     cap tool input/output bodies, including JSON
      --budget-chars N   cap total event-body characters, excluding labels/metadata
      --from-event N     start at stable event #N (inclusive; 0 allowed)
      --limit N          maximum events; JSON window.nextEvent gives the next ID
                         --no-toolcalls is an alias for --no-tools in show/transcript
      --color / --no-color
                         force ANSI colors on or off (default: on for a terminal)

scrub flags (redact a transcript in place; writes a .bak-<epoch> copy first)
      --drop N|A-B       redact event #N (or a range) from dejavu transcript; repeatable,
                         comma lists allowed; dropping a tool call also drops its result
      --pattern TEXT     remove every line containing TEXT (case-insensitive) from every
                         string field in every record, including dead branches; repeatable
      --placeholder TEXT replacement text (default "${DEFAULT_PLACEHOLDER}")
      --dry-run          report what would change without writing

profile flags
      --project SUBSTR  select indexed sessions by project instead of locators
      --since WHEN      select sessions with visible-message activity since YYYY-MM-DD/7d
      --limit N         maximum sessions in project mode (default 10)
      --output-threshold N
                         flag results over N characters (default 10000)
      --explain         interpret bounded metrics with Luna medium; no raw context sent

query flags
      --model ID         Codex model (default gpt-5.6-luna, medium reasoning)
                         Explicit provider/id selects a legacy HTTP/Pi provider;
                         codex/id selects Codex exec
      --agent-dir P      Pi config directory for legacy overrides (default ~/.pi/agent)

common flags
      --json             emit structured results, respecting explicit bounds
  -q, --quiet            suppress stderr diagnostics
  -h, --help             show this help

Search covers detected Claude, Codex, Pi, and OpenCode stores by default.
Memory commands read Claude's cross-project Markdown memory corpus.
Memory selectors accept exact listed project keys, unique project substrings, or file paths.
Memory search --snippets also requires an integer >= 1.
It is case-insensitive literal fixed-string search, not semantic search.
Use one distinctive token or exact phrase per call.`;

function die(message: string): never {
  console.error(colors.red(`✗ ${message}`));
  process.exit(1);
}

function pullFlag(args: string[], ...names: string[]): boolean {
  let found = false;
  for (const name of names) {
    let index: number;
    while ((index = args.indexOf(name)) >= 0) {
      args.splice(index, 1);
      found = true;
    }
  }
  return found;
}

function pullValues(args: string[], names: string[]): string[] {
  const values: string[] = [];
  let value: string | undefined;
  while ((value = pullValue(args, names)) !== undefined) values.push(value);
  return values;
}

function pullValue(args: string[], names: string[]): string | undefined {
  for (const name of names) {
    const index = args.findIndex((arg) => arg === name || arg.startsWith(`${name}=`));
    if (index < 0) continue;
    const arg = args[index]!;
    if (arg.includes("=")) {
      args.splice(index, 1);
      return arg.slice(arg.indexOf("=") + 1);
    }
    const value = args[index + 1];
    if (value === undefined || value.startsWith("-")) die(`${name} needs a value`);
    args.splice(index, 2);
    return value;
  }
  return undefined;
}

function integer(value: string | undefined, flag: string, fallback: number): number {
  if (value === undefined) return fallback;
  const parsed = Number(value);
  if (!Number.isInteger(parsed) || parsed < 1) die(`${flag} needs an integer >= 1 (got '${value}')`);
  return parsed;
}

function rejectUnknownFlags(args: string[]): void {
  const unknown = args.find((arg) => arg.startsWith("-") && arg !== "-");
  if (unknown) die(`unknown flag: ${unknown}`);
}

function optionalBound(args: string[], flag: string, minimum = 1): number | undefined {
  const raw = pullValue(args, [flag]);
  if (raw === undefined) return undefined;
  const value = raw.trim() ? Number(raw) : NaN;
  validateBound(value, flag, minimum);
  return value;
}

function reportSkippedStores(diagnostics: StoreDiagnostic[], quiet: boolean): void {
  if (quiet) return;
  for (const diagnostic of diagnostics) {
    console.error(colors.dim(`skipped unreadable ${diagnostic.source} store ${diagnostic.path}: ${diagnostic.error}`));
  }
}

async function main(): Promise<void> {
  const args = process.argv.slice(2);
  if (args.length === 0 || pullFlag(args, "-h", "--help")) {
    console.log(HELP);
    return;
  }
  if (args.length === 1 && (args[0] === "--version" || args[0] === "-V")) {
    console.log(VERSION);
    return;
  }
  const json = pullFlag(args, "--json");
  const quiet = pullFlag(args, "-q", "--quiet");
  if (args[0] === "self-update") {
    args.shift();
    const checkOnly = pullFlag(args, "--check");
    rejectUnknownFlags(args);
    if (args.length > 0) die(`self-update accepts no positional arguments (unexpected: '${args[0]}')`);
    const result = await selfUpdate({ checkOnly });
    if (json) console.log(JSON.stringify(result, null, 2));
    else if (result.updated) console.log(`dejavu ${result.current} -> ${result.latest} installed at ${result.path}`);
    else if (compareVersions(result.latest, result.current) > 0) console.log(`dejavu ${result.latest} is available (installed ${result.current}); run \`dejavu self-update\` to install it`);
    else console.log(`dejavu ${result.current} is up to date`);
    return;
  }
  if (args[0] === "profile") {
    args.shift();
    const project = pullValue(args, ["--project"]);
    const since = pullValue(args, ["--since"]);
    const limit = integer(pullValue(args, ["--limit"]), "--limit", 10);
    const threshold = integer(pullValue(args, ["--output-threshold"]), "--output-threshold", 10_000);
    const explain = pullFlag(args, "--explain");
    rejectUnknownFlags(args);
    const report = await profileSessions(args, { project, since, limit, threshold });
    if (explain && report.sessions.length) {
      const controller = new AbortController();
      const cancel = () => controller.abort();
      process.once("SIGINT", cancel); process.once("SIGTERM", cancel);
      try { report.explanation = await explainProfile(report, controller.signal); }
      finally { process.removeListener("SIGINT", cancel); process.removeListener("SIGTERM", cancel); }
    }
    console.log(json ? JSON.stringify(report, null, 2) : renderProfile(report));
    if (report.diagnostics.length || report.explanation?.error) process.exitCode = 1;
    return;
  }
  if (args[0] === "index") {
    args.shift();
    const verb = args.shift() ?? "status";
    rejectUnknownFlags(args);
    if (args.length > 0) die(`index ${verb} accepts no positional arguments (unexpected: '${args[0]}')`);
    if (verb === "status") {
      const result = await transcriptIndexStatus();
      console.log(json ? JSON.stringify(result, null, 2) : result.exists
        ? `${result.files} files · ${result.messages} messages · ${result.bytes} bytes · schema v${result.schemaVersion} · ${result.path}`
        : `not built · ${result.path}`);
      return;
    }
    if (verb === "update" || verb === "rebuild") {
      const result = await refreshTranscriptIndex(await discoverTranscriptStores("all"), undefined, verb === "rebuild");
      console.log(json ? JSON.stringify(result, null, 2) : `${result.files} files · ${result.messages} messages · ${result.indexed} indexed · ${result.removed} removed · ${result.elapsedMs}ms · ${result.path}`);
      reportSkippedStores(result.skipped, quiet || json);
      return;
    }
    die(`unknown index command '${verb}' (status|update|rebuild)`);
  }
  if (args[0] === "memory") {
    args.shift();
    const verb = args.shift() ?? "list";
    const root = pullValue(args, ["--root"]) ?? defaultMemoryRoot();
    if (verb === "list") {
      const files = pullFlag(args, "--files");
      rejectUnknownFlags(args);
      if (args.length > 0) die(`memory list accepts no positional arguments (unexpected: '${args[0]}')`);
      if (files) {
        const result = await memoryFiles(root);
        console.log(json ? JSON.stringify(result, null, 2) : result.map((file) => `${file.project}/${file.name}\t${file.path}`).join("\n"));
      } else {
        const result = await listMemories(root);
        console.log(json ? JSON.stringify(result, null, 2) : result.map((project) => `${project.project}\t${project.files}\t${project.path}`).join("\n"));
      }
      return;
    }
    if (verb === "search") {
      const limit = integer(pullValue(args, ["-n", "--limit"]), "--limit", 20);
      const snippets = integer(pullValue(args, ["--snippets"]), "--snippets", 3);
      rejectUnknownFlags(args);
      const query = args.join(" ").trim() || die("memory search needs one token or exact phrase");
      const result = await searchMemories(query, { root, limit, snippets });
      if (json) console.log(JSON.stringify(result, null, 2));
      else console.log(result.map((match) => `${match.count}\t${match.project}/${match.name}\t${match.path}\n  ${match.snippets.join("\n  ")}`).join("\n"));
      return;
    }
    if (verb === "show") {
      // Claude project keys emitted by memory list begin with a single hyphen.
      rejectUnknownFlags(args.filter((arg) => !/^-[^-]/.test(arg)));
      const selector = args.shift() ?? die("memory show needs a project slug, project substring, or memory file path");
      if (args.length > 0) die(`memory show accepts one selector (unexpected: '${args[0]}')`);
      const result = await showMemory(selector, root);
      console.log(json ? JSON.stringify(result, null, 2) : result.content);
      return;
    }
    die(`unknown memory command '${verb}' (list|search|show)`);
  }
  if (args[0] === "find") {
    args.shift();
    const source = parseSource(pullValue(args, ["-s", "--source"]) ?? "all");
    const limit = integer(pullValue(args, ["-n", "--limit"]), "--limit", DEFAULT_FIND_LIMIT);
    const project = pullValue(args, ["-p", "--project"]);
    const since = pullValue(args, ["--since"]);
    const userOnly = pullFlag(args, "--user");
    const pathsOnly = pullFlag(args, "--paths");
    const maxParallel = integer(pullValue(args, ["--max-parallel"]), "--max-parallel", DEFAULT_MAX_PARALLEL);
    const noIndex = pullFlag(args, "--no-index");
    rejectUnknownFlags(args);
    if (args.length === 0) die("find needs one or more terms");
    const result = await findSessions(args, { source, limit, project, since, userOnly, maxParallel, noIndex });
    if (pathsOnly) console.log(result.hits.map((hit) => hit.path).join("\n"));
    else console.log(json ? JSON.stringify(result, null, 2) : renderFind(result));
    reportSkippedStores(result.skippedStores, quiet);
    if (!quiet && !json) {
      const timings = Object.entries(result.storeTimings).map(([store, ms]) => `${store} ${ms}ms`).join(" · ");
      console.error(colors.dim(`${timings} · total ${result.elapsedMs}ms`));
    }
    return;
  }
  if (args[0] === "pack") {
    args.shift();
    const source = parseSource(pullValue(args, ["-s", "--source"]) ?? "all");
    const limit = integer(pullValue(args, ["-n", "--limit"]), "--limit", 3);
    const project = pullValue(args, ["-p", "--project"]);
    const since = pullValue(args, ["--since"]);
    const userOnly = pullFlag(args, "--user");
    const noIndex = pullFlag(args, "--no-index");
    const maxParallel = integer(pullValue(args, ["--max-parallel"]), "--max-parallel", DEFAULT_MAX_PARALLEL);
    const budgetChars = optionalBound(args, "--budget-chars");
    const maxChars = optionalBound(args, "--max-chars");
    const context = optionalBound(args, "--context", 0);
    const excludeSessions = pullValues(args, ["--exclude-session"]);
    rejectUnknownFlags(args);
    if (args.length === 0 || args.some((term) => !term.trim())) die("pack needs one or more nonempty terms");
    const result = await packSessions(args, { source, limit, project, since, userOnly, noIndex, maxParallel, budgetChars, maxChars, context, excludeSessions });
    console.log(json ? JSON.stringify(result, null, 2) : renderPack(result));
    reportSkippedStores(result.skippedStores, quiet);
    if (!quiet) for (const skipped of result.skippedSessions) console.error(colors.dim(`skipped ${skipped.path}: ${skipped.error}`));
    return;
  }
  if (args[0] === "show") {
    args.shift();
    const full = pullFlag(args, "--full");
    const around = pullValue(args, ["--around"]);
    const tools = !pullFlag(args, "--no-tools", "--no-toolcalls");
    const maxChars = optionalBound(args, "--max-chars");
    rejectUnknownFlags(args);
    const locator = args.shift() ?? die("show needs a transcript locator from search results");
    if (args.length > 0) die(`show accepts one transcript locator (unexpected argument: '${args[0]}')`);
    const result = await showSession(locator, { full, around, tools, maxChars });
    console.log(json ? JSON.stringify(result, null, 2) : renderShow(result));
    if (!quiet && !json) console.error(colors.dim(`${result.source} · ${result.messageCount} message${result.messageCount === 1 ? "" : "s"}`));
    return;
  }
  if (args[0] === "transcript" || args[0] === "view") {
    args.shift();
    const full = pullFlag(args, "--full");
    const thinking = pullFlag(args, "--thinking");
    const tools = !pullFlag(args, "--no-tools", "--no-toolcalls");
    const maxChars = optionalBound(args, "--max-chars");
    const toolChars = optionalBound(args, "--tool-chars");
    const budgetChars = optionalBound(args, "--budget-chars");
    const fromEvent = optionalBound(args, "--from-event", 0);
    const limit = optionalBound(args, "--limit");
    const clipping = maxChars !== undefined || toolChars !== undefined || budgetChars !== undefined;
    if (full && clipping) die("--full cannot be combined with character limits");
    const forceColor = pullFlag(args, "--color");
    const noColor = pullFlag(args, "--no-color");
    rejectUnknownFlags(args);
    const locator = args.shift() ?? die("transcript needs a transcript locator from search results");
    if (args.length > 0) die(`transcript accepts one transcript locator (unexpected argument: '${args[0]}')`);
    const view = await viewTranscript(locator, { thinking, tools });
    const bounded = clipping || fromEvent !== undefined || limit !== undefined;
    const result = bounded ? windowTranscript(view, {
      fromEvent, limit, budgetChars,
      maxChars: maxChars ?? (!json && clipping ? 1200 : undefined),
      toolChars: toolChars ?? (!json && clipping ? 600 : undefined),
    }) : view;
    const color = forceColor || (!noColor && !json && Boolean(process.stdout.isTTY) && !process.env.NO_COLOR);
    const summary = "window" in result ? `\n\n${renderWindow((result as ReturnType<typeof windowTranscript>).window)}` : "";
    console.log(json ? JSON.stringify(result, null, 2) : `${renderTranscript(result, { full: full || clipping, color })}${summary}`);
    if (!quiet && !json) {
      const c = result.counts;
      console.error(colors.dim(`${result.source} · ${c.user} user · ${c.assistant} assistant · ${c.toolCalls} tool calls · ${c.toolResults} results · ${c.thinking} thinking`));
    }
    return;
  }
  if (args[0] === "scrub") {
    args.shift();
    const drop = parseDropList(pullValues(args, ["--drop"]));
    const patterns = pullValues(args, ["--pattern"]);
    const placeholder = pullValue(args, ["--placeholder"]);
    const dryRun = pullFlag(args, "--dry-run");
    rejectUnknownFlags(args);
    const locator = args.shift() ?? die("scrub needs a transcript locator");
    if (args.length > 0) die(`scrub accepts one transcript locator (unexpected argument: '${args[0]}')`);
    const result = await scrubTranscript(locator, { drop, patterns, placeholder, dryRun });
    if (json) { console.log(JSON.stringify(result, null, 2)); return; }
    const lines = [
      `${result.dryRun ? "Would change" : "Changed"} ${result.changedRecords} record${result.changedRecords === 1 ? "" : "s"} in ${result.source} transcript ${result.path}`,
      result.droppedEvents.length ? `Redacted events: ${result.droppedEvents.map((index) => `#${index}`).join(", ")}` : undefined,
      patterns.length ? `Pattern lines removed: ${result.patternLines}` : undefined,
      result.backup ? `Backup: ${result.backup}` : undefined,
    ];
    console.log(lines.filter((line): line is string => line !== undefined).join("\n"));
    if (!quiet && !result.dryRun && result.changedRecords > 0) {
      console.error(colors.dim("a running agent that already loaded this session keeps the old content in memory until it restarts"));
    }
    return;
  }
  if (args[0] === "query") {
    args.shift();
    const agentDir = pullValue(args, ["--agent-dir"]) ?? join(homedir(), ".pi", "agent");
    const model = pullValue(args, ["--model"]);
    rejectUnknownFlags(args);
    const locator = args.shift() ?? die("query needs a transcript locator from search results");
    const question = args.join(" ").trim() || die("query needs a question");
    const controller = new AbortController();
    const cancel = () => controller.abort();
    process.once("SIGINT", cancel);
    process.once("SIGTERM", cancel);
    let result;
    try {
      result = await querySession(locator, question, { agentDir, model, signal: controller.signal });
    } finally {
      process.removeListener("SIGINT", cancel);
      process.removeListener("SIGTERM", cancel);
    }
    console.log(json ? JSON.stringify(result, null, 2) : renderQuery(result));
    if (!quiet && !json) {
      const tokens = result.usage ? ` · ${result.usage.inputTokens} in / ${result.usage.outputTokens} out` : "";
      const cost = result.costUsd !== undefined ? ` · ~$${result.costUsd.toFixed(4)}` : "";
      const reasoning = result.model.reasoningEffort ? ` · ${result.model.reasoningEffort}` : "";
      console.error(colors.dim(`${result.source} · ${result.model.provider}/${result.model.id}${reasoning} · ${result.transport} · ${result.messageCount} message${result.messageCount === 1 ? "" : "s"}${result.wasWindowed ? " · windowed" : ""}${tokens}${cost} · ${result.elapsedMs}ms`));
    }
    return;
  }
  const source = parseSource(pullValue(args, ["-s", "--source"]) ?? "all");
  const limit = integer(pullValue(args, ["-n", "--limit"]), "--limit", DEFAULT_SEARCH_LIMIT);
  const snippets = integer(pullValue(args, ["--snippets"]), "--snippets", DEFAULT_SNIPPET_LIMIT);
  const maxParallel = integer(pullValue(args, ["--max-parallel"]), "--max-parallel", DEFAULT_MAX_PARALLEL);
  const noIndex = pullFlag(args, "--no-index");
  rejectUnknownFlags(args);
  const query = args.join(" ").trim() || die("need one token or exact phrase to search");
  const result = await searchSessions(query, { source, limit, snippets, maxParallel, noIndex });
  console.log(json ? JSON.stringify(result, null, 2) : renderSearch(result));
  reportSkippedStores(result.skippedStores, quiet);
  if (!quiet && !json) console.error(colors.dim(`${result.sources.join(",")} · ${result.elapsedMs}ms`));
}

// People at a terminal see a daily release notice on stderr; agents and pipes never pay for the lookup.
async function printUpdateNotice(): Promise<void> {
  const args = process.argv.slice(2);
  if (!process.stderr.isTTY || args[0] === "self-update" || args.some((arg) => ["--json", "-q", "--quiet", "--paths"].includes(arg))) return;
  const latest = await availableUpdate().catch(() => null);
  if (latest) console.error(colors.dim(`dejavu ${latest} is available (installed ${VERSION}); run \`dejavu self-update\`, or set ${DISABLE_CHECK_ENV}=1 to silence this`));
}

main()
  .then(printUpdateNotice)
  .catch((error) => die(error instanceof Error ? error.message : String(error)));
