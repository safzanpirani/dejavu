//! Command-specific help. JSON contracts are checked against fixture CLI output.
use crate::args::Args;
use crate::render::Paint;

pub const COMMANDS: &[&str] = &[
    "search",
    "find",
    "pack",
    "last",
    "show",
    "transcript",
    "view",
    "scrub",
    "query",
    "profile",
    "memory",
    "index",
    "self-update",
];

pub fn requested(mut args: Args, explicit: bool) -> Result<String, String> {
    args.flag(&["--json", "-q", "--quiet", "--color", "--no-color"]);
    if explicit {
        args.shift();
    }
    let Some(first) = args.first() else {
        return Ok(overview());
    };
    let first = if COMMANDS.contains(&first) {
        first
    } else if explicit {
        return Err(format!("unknown help topic '{first}'; run dejavu --help"));
    } else {
        "search"
    };
    let topic = if matches!(first, "memory" | "index") {
        match args.items.get(1).map(String::as_str) {
            Some(sub) if !sub.starts_with('-') => format!("{first} {sub}"),
            _ => first.to_string(),
        }
    } else {
        first.to_string()
    };
    command(&topic)
}

pub fn render(text: &str, color: bool) -> String {
    let paint = Paint { color };
    let (heading, rest) = text.split_once('\n').unwrap_or((text, ""));
    let heading = if color {
        paint.markdown(&format!("**{heading}**"))
    } else {
        heading.to_string()
    };
    format!("{heading}\n{rest}")
}

pub fn overview() -> String {
    format!(
        "dejavu: search and query coding-agent transcripts

Usage: dejavu <phrase> [flags]
       dejavu <command> [flags]
       dejavu help <command>

Commands:
  search <phrase>                  literal phrase search (also the bare form)
  find <term>...                   rank sessions by several literal terms
  pack <term>...                   collect bounded excerpts around matches
  last [terms | locator | id]      recover the previous session's context
  show <locator|id>               read summarized conversation turns
  transcript <locator|id>         read events and tool activity (alias: view)
  scrub <locator>                  redact events or patterns; creates a backup
  query <locator|id> <question>    ask a model about one session (model usage)
  profile <locator|id>...          measure tool activity (--explain uses a model)
  memory [list | search | show]    read Claude project Markdown memory
  index [status | update | rebuild] manage the local transcript index
  self-update [--check]            check or install a release
  help [command]                  command flags, JSON shapes, jq, exit codes
  --version, -V                   print the installed version

Common forms:
  dejavu last                                         continue the previous session here
  dejavu find deploy timeout --paths                  locators of matching sessions
  dejavu search 'exact phrase' -n 5 --json | jq -r '.matches[].path'
  dejavu show <locator|id> --no-tools --around TERM   one region of a conversation
  dejavu transcript <locator|id> --no-tools --max-chars 1500 --budget-chars 8000

A locator is a path (or opencode://, openclaw://, hermes:// locator) printed by search,
find, or last. A bare session id also works for show, transcript, query, and profile.

{COMMON}

Use dejavu <command> --help or dejavu help <command> for the full flag reference.
Use dejavu memory search --help for a nested command.
Use dejavu -- search to search for the literal word search.
Help uses color only on a terminal; NO_COLOR or --no-color disables it."
    )
}

const SEARCH: &str = r#"  -s, --source NAME      all, claude, codex, pi, omp, opencode, droid, openclaw, hermes, or agy (default all)
  -n, --limit N          transcripts to return (default 10)
      --snippets N       snippets per transcript (integer >= 1; default 3)
      --max-parallel N   local store/file workers (default 4)
      --no-index         bypass the transcript index and scan files directly
      --color / --no-color
                         force ANSI colors on or off (default: on for a terminal)"#;

const FIND: &str = r#"  -s, --source NAME      restrict to one source
  -n, --limit N          sessions to return (default 5)
  -p, --project SUBSTR   only sessions whose project path contains SUBSTR
      --since WHEN       YYYY-MM-DD or 7d / 2w / 3m
      --user             require every term to appear in user messages
      --paths            print matching transcript locators only, one per line
      --max-parallel N   local store/candidate workers (default 4)
      --no-index         bypass the transcript index and scan files directly
      --color / --no-color
                         force ANSI colors on or off (default: on for a terminal)"#;

const SHOW: &str = r#"      --full             do not truncate long messages
      --around TERM      only messages containing TERM, with 3 turns of context
      --no-tools         only user/assistant content, without tool summaries
      --max-chars N      characters per message (default 700; applies to JSON)

      --no-toolcalls      alias for --no-tools"#;

const PACK: &str = r#"      --limit N          sessions to return (default 3; search cap 40)
      --budget-chars N   total event-body characters (default 12000)
      --max-chars N      maximum characters per event (default 1200)
      --context N        neighboring dialogue events per match (default 2; 0 allowed)
      --exclude-session ID_OR_LOCATOR
                         repeatable; active session IDs from the environment are excluded
                         also accepts find's source, project, since, user, no-index, max-parallel

  -s, --source NAME      all, claude, codex, pi, omp, opencode, droid, openclaw, hermes, or agy
  -p, --project SUBSTR   project path substring
      --since WHEN       YYYY-MM-DD or 7d / 2w / 3m
      --user             require every term in user messages
      --no-index         scan directly
      --max-parallel N   local workers (default 4)
  -n N                  alias for --limit"#;

const LAST: &str = r#"                         no argument: newest session in the current Git repo (or directory);
                         terms: find's best match in this repo, else anywhere;
                         a locator or session id: that session.
                         The active Claude, Codex, or Droid session is skipped.
  -p, --project SUBSTR   sessions whose project contains SUBSTR, not the current repo
      --anywhere         newest session in any project
      --list             session cards only; -n/--limit N of them (default 5)
      --turns N          newest events in the tail (default 12)
      --budget-chars N   total tail body characters (default 8000)
      --max-chars N      characters per dialogue event (default 1500)
      --tools            include tool calls and results (--tool-chars N, default 400)
      --exclude-session ID_OR_LOCATOR
                         repeatable; also accepts --source, --since, and --color/--no-color

  -s, --source NAME      all, claude, codex, pi, omp, opencode, droid, openclaw, hermes, or agy
      --since WHEN       YYYY-MM-DD or 7d / 2w / 3m
      --color / --no-color"#;

const TRANSCRIPT: &str = r#"      --full             do not truncate messages, tool inputs, or tool outputs
      --thinking         include model thinking blocks
      --no-tools         hide tool calls and tool results
      --max-chars N      cap dialogue/thinking event bodies, including JSON
      --tool-chars N     cap tool input/output bodies, including JSON
      --budget-chars N   cap total event-body characters, excluding labels/metadata
      --from-event N     start at stable event #N (inclusive; 0 allowed)
      --limit N          maximum events; JSON window.nextEvent gives the next ID
                         --no-toolcalls is an alias for --no-tools in show/transcript
      --color / --no-color
                         force ANSI colors on or off (default: on for a terminal)"#;

const SCRUB: &str = r#"      --drop N|A-B       redact event #N (or a range) from dejavu transcript; repeatable,
                         comma lists allowed; dropping a tool call also drops its result
      --pattern TEXT     remove every line containing TEXT (case-insensitive) from every
                         string field in every record, including dead branches; repeatable
      --placeholder TEXT replacement text (default "[redacted]")
      --dry-run          report what would change without writing"#;

const PROFILE: &str = r#"      --project SUBSTR  select indexed sessions by project instead of locators
      --since WHEN      select sessions with visible-message activity since YYYY-MM-DD/7d
      --limit N         maximum sessions in project mode (default 10)
      --output-threshold N
                         flag results over N characters (default 10000)
      --explain         interpret bounded metrics with the query model; no raw context sent"#;

const QUERY: &str = r#"      --harness NAME     codex (default) or ruddr (requires Ruddr on PATH)
      --model ID         model (default gpt-6-luna); bare IDs select Codex
                         With ruddr, prefix selects codex/claude/pi/opencode/droid;
                         otherwise provider/id selects legacy HTTP/Pi (except codex/id)
      --effort LEVEL     reasoning effort (Codex default medium; Ruddr forwards level)
      --agent-dir P      Pi config directory for legacy overrides (default ~/.pi/agent)
                         Legacy HTTP/Pi does not support --effort
      Defaults: flags > DEJAVU_QUERY_HARNESS / DEJAVU_QUERY_MODEL /
                DEJAVU_QUERY_EFFORT > config.json query object > built-in defaults.
      Config: $XDG_CONFIG_HOME/dejavu/config.json or ~/.config/dejavu/config.json."#;

const COMMON: &str = r#"      --json             emit structured results, respecting explicit bounds
  -q, --quiet            suppress stderr diagnostics
  -h, --help             show command help"#;

const STORES: &str = r#"Search covers detected Claude, Codex, Pi, omp, OpenCode, Droid, OpenClaw, Hermes, and agy stores by default.
Each agent's variable replaces its home-directory store: CLAUDE_CONFIG_DIR
($CLAUDE_CONFIG_DIR/projects), CODEX_HOME ($CODEX_HOME/sessions),
PI_CODING_AGENT_DIR ($PI_CODING_AGENT_DIR/sessions), XDG_DATA_HOME
($XDG_DATA_HOME/opencode/*.db) or OPENCODE_DB for OpenCode, and
FACTORY_HOME_OVERRIDE ($FACTORY_HOME_OVERRIDE/.factory/sessions). Without
PI_CODING_AGENT_DIR, Pi search also covers sibling profiles (~/.pi/*/sessions).
omp search reads ~/.omp/agent/sessions and ~/.omp/profiles/*/agent/sessions.
OpenClaw reads each $OPENCLAW_STATE_DIR/agents/*/agent/openclaw-agent.sqlite
(default ~/.openclaw); Hermes reads $HERMES_HOME/state.db (default ~/.hermes).
Memory commands read Claude's cross-project Markdown memory corpus.
Memory selectors accept exact listed project keys, unique project substrings, or file paths.
Memory search --snippets also requires an integer >= 1.
It is case-insensitive literal fixed-string search, not semantic search.
Use one distinctive token or exact phrase per call."#;

const MEMORY_LIST: &str = "JSON array item keys: project, path, files\njq: jq '.[].project'\nWith --files, JSON array item keys: project, name, path\njq (--files): jq '.[].path'";
const MEMORY_SEARCH: &str = "JSON array item keys: project, name, path, count, snippets\njq: jq '.[].path'\nThe top level is a bare array, with no results or hits wrapper.";
const MEMORY_SHOW: &str =
    "JSON object keys: file, content\njq: jq '.content'\nfile contains project, name, path.";
const INDEX_STATUS: &str =
    "JSON object keys: path, exists, schemaVersion, files, messages, bytes\njq: jq '.exists'";
const INDEX_REFRESH: &str = "JSON object keys: path, files, messages, indexed, removed, skipped, elapsedMs\njq: jq '.indexed'";

pub fn command(topic: &str) -> Result<String, String> {
    let (usage, flags, schema, notes, exits) = match topic {
        "search" => (
            "search <phrase> [flags]", SEARCH.to_string(),
            "JSON object keys: query, sources, matches, skippedStores, elapsedMs\njq: jq '.matches[].path'",
            format!("The bare dejavu <phrase> form is equivalent. Spaces join into one literal phrase.\nUse dejavu -- search or dejavu search search for the literal word search.\n\n{STORES}"),
            "0: completed, including zero matches or skipped stores; 1: usage or search error.",
        ),
        "find" => (
            "find <term> [term...] [flags]", FIND.to_string(),
            "JSON object keys: terms, requiredTerms, sources, hits, truncated, skippedStores, elapsedMs, storeTimings\njq: jq '.hits[].path'",
            "Terms match within a session. requiredTerms reports relaxed matching.\ntruncated is true when eligible sessions exceed the 40-candidate scoring limit.\nopeningPrompt previews contain at most 300 characters; an ellipsis marks clipping.\n--paths overrides --json and emits locators instead of JSON.".into(),
            "0: completed, including zero hits or skipped stores; 1: usage or search error.",
        ),
        "pack" => (
            "pack <term> [term...] [flags]", PACK.to_string(),
            "JSON object keys: terms, requiredTerms, budgetChars, usedChars, candidateCount, excludedCount, sessions, skippedStores, skippedSessions, omitted\njq: jq '.sessions[].events[]'",
            "Model-free excerpts. Neighborhoods rank by match density before sharing the budget.\nJSON uses compact formatting. sessions contain events and window metadata.\nomitted: JSON array item keys: path, neighborhoods, events, nextEvent\nomitted counts excluded match anchors and context events per loaded session.\nUse omitted[].path and nextEvent with transcript <locator> --from-event N.\nwindow.nextEvent also continues with transcript --from-event; recover clipped events with --full.".into(),
            "0: completed, including empty or partial results; 1: usage or search error.",
        ),
        "last" => (
            "last [term... | transcript-locator | session-id] [flags]", LAST.to_string(),
            "JSON object keys: sessions, total, excludedCount, tail?, tailStart?, skippedStores\njq: jq '.sessions[].path'",
            "tail and tailStart are omitted for --list or when no tail is available.\nA tail contains events and window metadata. No model is used.\nopeningPrompt previews contain at most 300 characters; an ellipsis marks clipping.".into(),
            "0: at least one session; 1: no sessions, usage error, or read error.",
        ),
        "show" => (
            "show <transcript-locator> [flags]", SHOW.to_string(),
            "JSON object keys: path, source, messageCount, messages\njq: jq '.messages[].text'",
            "messages contain role and text. This command uses no model.".into(),
            "0: completed; 1: usage or read error.",
        ),
        "transcript" | "view" => (
            "transcript <transcript-locator> [flags]", TRANSCRIPT.to_string(),
            "JSON object keys: path, source, project, counts, events, window?\njq: jq '.events[]'",
            "view is an alias. JSON contains an object with events, not a bare event array.\nwindow appears with pagination or character bounds. window.nextEvent is the next event ID.\n--full accepts pagination but cannot be combined with character limits.".into(),
            "0: completed; 1: usage or read error.",
        ),
        "scrub" => (
            "scrub <transcript-locator> [--drop N|A-B]... [--pattern TEXT]... [flags]", SCRUB.to_string(),
            "JSON object keys: path, source, dryRun, backup, droppedEvents, patternLines, changedRecords\njq: jq '.changedRecords'",
            "This command modifies a transcript. Use --dry-run to inspect counts first.\nbackup is null for dry runs and unchanged transcripts.".into(),
            "0: completed; 1: usage, read, or write error.",
        ),
        "query" => (
            "query <transcript-locator> <question> [flags]", QUERY.to_string(),
            "JSON object keys: source, sessionPath, question, answer, model, transport, usage?, costUsd?, messageCount, wasWindowed, elapsedMs\njq: jq '.answer'",
            "This command invokes a model and may incur usage charges.\nusage requires available token counts; costUsd requires legacy model pricing.\nCodex and Ruddr omit costUsd.".into(),
            "0: answer returned; 1: usage, configuration, read, or model error.",
        ),
        "profile" => (
            "profile <transcript-locator>... [flags]\n       dejavu profile --project SUBSTR [--since WHEN] [flags]", format!("{PROFILE}\n\nModel options (--explain only):\n{QUERY}"),
            "JSON object keys: version, sessions, oversizedThreshold, matchedSessions, omittedSessions, diagnostics, limitations, explanation?\njq: jq '.sessions[].metrics'",
            "The default measures tool activity without a model. --explain incurs model usage.\nexplanation appears only when --explain runs on nonempty sessions.".into(),
            "0: completed without diagnostics; 1: diagnostics, explanation failure, or other error.",
        ),
        "memory" => (
            "memory [list|search|show] [flags]", "      --root DIR        Claude projects root (default $CLAUDE_CONFIG_DIR/projects or ~/.claude/projects)\n      list --files      list individual memory files\n      search <phrase> [-n N|--limit N] [--snippets N]\n                        limit defaults to 20; snippets defaults to 3; both >= 1\n      show <selector>   project key, unique project substring, or file path".into(),
            "JSON: list and search return bare arrays; show returns {file, content}.",
            format!("Default command: list.\n\nmemory list:\n{MEMORY_LIST}\n\nmemory search:\n{MEMORY_SEARCH}\n\nmemory show:\n{MEMORY_SHOW}"),
            "0: completed, including empty lists/searches; 1: usage, selection, or read error.",
        ),
        "memory list" => (
            "memory list [--files] [--root DIR] [flags]", "      --files           list files instead of projects\n      --root DIR        override the Claude projects root".into(), MEMORY_LIST,
            "The default root is $CLAUDE_CONFIG_DIR/projects or ~/.claude/projects.".into(),
            "0: completed, including an empty list; 1: usage or read error.",
        ),
        "memory search" => (
            "memory search <phrase> [flags]", "  -n, --limit N         maximum files (default 20; >= 1)\n      --snippets N      snippets per file (default 3; >= 1)\n      --root DIR        override the Claude projects root".into(), MEMORY_SEARCH,
            "Search matches a literal phrase without case sensitivity.".into(),
            "0: completed, including zero matches; 1: usage or read error.",
        ),
        "memory show" => (
            "memory show <project-or-file> [flags]", "      --root DIR        override the Claude projects root".into(), MEMORY_SHOW,
            "Selectors accept exact listed project keys, unique project substrings, or file paths.\nA project with multiple files selects its MEMORY.md index.".into(),
            "0: completed; 1: missing or ambiguous selector, usage error, or read error.",
        ),
        "index" => (
            "index [status|update|rebuild] [flags]", "      status            inspect the index (default)\n      update            incrementally refresh selected stores\n      rebuild           discard and recreate the index".into(),
            "JSON: status returns an index-status object; update/rebuild return a refresh object.",
            format!("DEJAVU_INDEX_PATH overrides the index database path.\n\nindex status:\n{INDEX_STATUS}\n\nindex update / rebuild:\n{INDEX_REFRESH}"),
            "0: completed, including absent index or skipped stores; 1: usage or refresh error.",
        ),
        "index status" => (
            "index status [flags]", "No command-specific flags.".into(), INDEX_STATUS,
            "DEJAVU_INDEX_PATH overrides the index database path. exists is false for an absent index.".into(),
            "0: status returned, including an absent index; 1: usage error.",
        ),
        "index update" | "index rebuild" => (
            if topic == "index update" { "index update [flags]" } else { "index rebuild [flags]" },
            "No command-specific flags.".into(), INDEX_REFRESH,
            "DEJAVU_INDEX_PATH overrides the index database path. rebuild discards the old index.\nskipped reports unreadable stores.".into(),
            "0: completed, including skipped stores; 1: usage or refresh error.",
        ),
        "self-update" => (
            "self-update [--check] [flags]", "      --check           check the latest version without installing it".into(),
            "JSON object keys: current, latest, updated, path?\njq: jq '.updated'",
            "This command accesses the release server. path appears after an update.\nSource checkouts require git pull; npm/Bun installs use their package manager.".into(),
            "0: completed; 1: usage, network, verification, or installation error.",
        ),
        _ => return Err(format!("unknown help topic '{topic}'; run dejavu --help")),
    };
    Ok(format!(
        "dejavu {topic}\n\nUsage: dejavu {usage}\n\nFlags:\n{flags}\n{COMMON}\n\n{schema}\nPipe --json output to the jq command above.\nA key ending in ? is optional; the actual key has no question mark.\n\n{notes}\n\nExit codes: {exits}\nHelp uses color only on a terminal; NO_COLOR or --no-color disables it."
    ))
}

#[cfg(test)]
#[path = "../tests/support/help_contract.rs"]
mod help_contract;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn styling_preserves_plain_help() {
        let plain = command("find").unwrap();
        assert_eq!(render(&plain, false), plain);
        assert!(render(&plain, true).contains('\x1b'));
    }

    #[test]
    fn self_update_help_matches_the_serialized_result_without_network_or_install() {
        // The CLI has no injectable release server. Exercise its actual output
        // type without invoking self-update on an installed binary.
        for path in [None, Some("synthetic/dejavu".to_string())] {
            let result = crate::update::SelfUpdateResult {
                current: "1.0.0".into(),
                latest: "1.1.0".into(),
                updated: path.is_some(),
                path,
            };
            help_contract::assert_shape(
                &command("self-update").unwrap(),
                &serde_json::to_value(result).unwrap(),
                0,
            );
        }
    }
}
