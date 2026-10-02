//! dejavu: search and query coding-agent transcripts. A port of the Bun CLI;
//! command names, flags, text output, `--json` shapes, and exit codes match it.

// Modules export API for commands that are not ported yet.
#![allow(dead_code)]

mod args;
mod codex_client;
mod commands;
pub mod find;
pub mod index;
pub mod js;
mod memory;
mod model_client;
pub mod opencode;
pub mod pack;
pub mod paths;
pub mod pool;
mod profile;
mod query;
pub mod reader;
pub mod render;
pub mod scan;
pub mod scrub;
pub mod search;
pub mod sources;
pub mod types;
mod update;
pub mod view;
pub mod window;
pub mod wyhash;

use args::{Args, die};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const DEFAULT_SEARCH_LIMIT: usize = 10;
pub const DEFAULT_SNIPPET_LIMIT: usize = 3;
pub const DEFAULT_FIND_LIMIT: usize = 5;
pub const DEFAULT_MAX_PARALLEL: usize = 4;
pub const DEFAULT_PLACEHOLDER: &str = "[redacted]";

/// Flags every command accepts.
#[derive(Debug, Clone, Copy, Default)]
pub struct Common {
    pub json: bool,
    pub quiet: bool,
}

/// What a command returns: an exit code, or an error message that `main`
/// prints as `✗ message` with exit 1.
pub type Outcome = Result<i32, String>;

fn help() -> String {
    let (b, r) = (args::BOLD, args::RESET);
    format!(
        "{B}dejavu{R}: search and query coding-agent transcripts

  {B}dejavu{R} <token-or-exact-phrase> [flags]
  {B}dejavu find{R} <term> [term...] [flags]
  {B}dejavu pack{R} <term> [term...] [flags]
  {B}dejavu show{R} <transcript-locator> [flags]
  {B}dejavu transcript{R} <transcript-locator> [flags]
  {B}dejavu scrub{R} <transcript-locator> [--drop N|A-B]... [--pattern TEXT]... [flags]
  {B}dejavu query{R} <transcript-locator> <question> [flags]
  {B}dejavu profile{R} <transcript-locator>... [--explain] [--json]
  {B}dejavu profile{R} --project SUBSTR [--since 7d] [--limit 10] [--explain]
  {B}dejavu memory list{R} [--files] [--root DIR] [--json]
  {B}dejavu memory search{R} <phrase> [--limit N] [--snippets N] [--root DIR] [--json]
  {B}dejavu memory show{R} <project-or-file> [--root DIR] [--json]
  {B}dejavu index{R} <status|update|rebuild> [--json]
  {B}dejavu self-update{R} [--check] [--json]
  {B}dejavu --version{R}

search flags
  -s, --source NAME      all, claude, codex, pi, opencode, or droid (default all)
  -n, --limit N          transcripts to return (default {default_search_limit})
      --snippets N       snippets per transcript (integer >= 1; default {default_snippet_limit})
      --max-parallel N   local store/file workers (default {default_max_parallel})
      --no-index         bypass the transcript index and scan files directly

find flags (multi-term session finder, ranked, user messages weighted)
  -s, --source NAME      restrict to one source
  -n, --limit N          sessions to return (default {default_find_limit})
  -p, --project SUBSTR   only sessions whose project path contains SUBSTR
      --since WHEN       YYYY-MM-DD or 7d / 2w / 3m
      --user             require every term to appear in user messages
      --paths            print matching transcript locators only, one per line
      --max-parallel N   local store/candidate workers (default {default_max_parallel})
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
      --placeholder TEXT replacement text (default \"{default_placeholder}\")
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

Search covers detected Claude, Codex, Pi, OpenCode, and Droid stores by default.
Each agent's variable replaces its home-directory store: CLAUDE_CONFIG_DIR
($CLAUDE_CONFIG_DIR/projects), CODEX_HOME ($CODEX_HOME/sessions),
PI_CODING_AGENT_DIR ($PI_CODING_AGENT_DIR/sessions), XDG_DATA_HOME
($XDG_DATA_HOME/opencode/*.db) or OPENCODE_DB for OpenCode, and
FACTORY_HOME_OVERRIDE ($FACTORY_HOME_OVERRIDE/.factory/sessions). Without
PI_CODING_AGENT_DIR, Pi search also covers sibling profiles (~/.pi/*/sessions).
Memory commands read Claude's cross-project Markdown memory corpus.
Memory selectors accept exact listed project keys, unique project substrings, or file paths.
Memory search --snippets also requires an integer >= 1.
It is case-insensitive literal fixed-string search, not semantic search.
Use one distinctive token or exact phrase per call.",
        B = b,
        R = r,
        default_search_limit = DEFAULT_SEARCH_LIMIT,
        default_snippet_limit = DEFAULT_SNIPPET_LIMIT,
        default_find_limit = DEFAULT_FIND_LIMIT,
        default_max_parallel = DEFAULT_MAX_PARALLEL,
        default_placeholder = DEFAULT_PLACEHOLDER,
    )
}

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut args = Args::new(raw.clone());
    if raw.is_empty() || args.flag(&["-h", "--help"]) {
        println!("{}", help());
        return;
    }
    if raw.len() == 1 && (raw[0] == "--version" || raw[0] == "-V") {
        println!("{VERSION}");
        return;
    }
    let common = Common {
        json: args.flag(&["--json"]),
        quiet: args.flag(&["-q", "--quiet"]),
    };
    let outcome = match args.first() {
        Some("self-update") => commands::self_update::run(args, common),
        Some("profile") => commands::profile::run(args, common),
        Some("index") => commands::index::run(args, common),
        Some("memory") => commands::memory::run(args, common),
        Some("find") => commands::find::run(args, common),
        Some("pack") => commands::pack::run(args, common),
        Some("show") => commands::show::run(args, common),
        Some("transcript" | "view") => commands::transcript::run(args, common),
        Some("scrub") => commands::scrub::run(args, common),
        Some("query") => commands::query::run(args, common),
        _ => commands::search::run(args, common),
    };
    match outcome {
        Ok(code) => {
            commands::self_update::print_update_notice(&raw);
            std::process::exit(code)
        }
        Err(message) => die(&message),
    }
}
