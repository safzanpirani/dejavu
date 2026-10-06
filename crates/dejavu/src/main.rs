//! dejavu: search and query coding-agent transcripts. A port of the Bun CLI;
//! command names, flags, text output, `--json` shapes, and exit codes match it.

// Modules export API for commands that are not ported yet.
#![allow(dead_code)]

pub mod agy;
mod args;
mod codex_client;
mod commands;
mod droid_active;
pub mod find;
mod help;
pub mod index;
pub mod js;
pub mod last;
mod markdown;
mod memory;
mod model_client;
pub mod opencode;
pub mod pack;
pub mod paths;
pub mod pool;
mod profile;
mod query;
mod query_config;
pub mod reader;
pub mod render;
mod ruddr_client;
pub mod scan;
pub mod scrub;
pub mod search;
pub mod sources;
pub mod types;
mod update;
pub mod view;
pub mod virtual_store;
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

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut args = Args::new(raw.clone());
    args::configure_stderr(&args);
    let help_flag = args.flag(&["-h", "--help"]);
    let explicit_help = args.first() == Some("help");
    if raw.is_empty() || help_flag || explicit_help {
        let text = help::requested(args.clone(), explicit_help).unwrap_or_else(|e| die(&e));
        let color = args::help_color(&args);
        commands::transcript::print_stdout(&help::render(&text, color));
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
        Some("search") => {
            args.shift();
            commands::search::run(args, common)
        }
        Some("find") => commands::find::run(args, common),
        Some("last") => commands::last::run(args, common),
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
