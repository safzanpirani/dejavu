use crate::args::{self, Args, die};
use crate::commands::search::report_skipped_stores;
use crate::find::{FindOptions, find_sessions};
use crate::pack::{self, PackOptions};
use crate::search::Disk;
use crate::sources::parse_source;
use crate::{Common, DEFAULT_MAX_PARALLEL, Outcome, js};

pub fn run(mut args: Args, common: Common) -> Outcome {
    args.shift();
    let source = parse_source(
        &args
            .value(&["-s", "--source"])
            .unwrap_or_else(|| "all".into()),
    )?;
    let limit = args.integer(&["-n", "--limit"], "--limit", 3);
    let project = args.value(&["-p", "--project"]);
    let since = args.value(&["--since"]);
    let user_only = args.flag(&["--user"]);
    let no_index = args.flag(&["--no-index"]);
    let max_parallel = args.integer(&["--max-parallel"], "--max-parallel", DEFAULT_MAX_PARALLEL);
    let budget_chars = args.bound("--budget-chars", 1);
    let max_chars = args.bound("--max-chars", 1);
    let context = args.bound("--context", 0);
    let exclude_sessions = args.values(&["--exclude-session"]);
    args.reject_unknown_flags();
    if args.is_empty() || args.items.iter().any(|term| term.trim().is_empty()) {
        die("pack needs one or more nonempty terms");
    }
    let options = PackOptions {
        find: FindOptions {
            source,
            project,
            since,
            user_only,
            max_parallel,
            no_index,
            ..FindOptions::default()
        },
        limit,
        budget_chars,
        max_chars,
        context,
        exclude_sessions,
    };
    let env_exclusions = pack::active_session_ids();
    let result = pack::pack_sessions(
        &args.items,
        &options,
        &env_exclusions,
        |terms, find_options| find_sessions(terms, find_options, &Disk),
        &pack::view_without_tools,
    )?;
    if common.json {
        println!("{}", js::pretty(&result));
    } else {
        println!("{}", pack::render_pack(&result));
    }
    report_skipped_stores(&result.skipped_stores, common.quiet);
    if !common.quiet {
        for skipped in &result.skipped_sessions {
            eprintln!(
                "{}",
                args::dim(&format!("skipped {}: {}", skipped.path, skipped.error))
            );
        }
    }
    Ok(0)
}
