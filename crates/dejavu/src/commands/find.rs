use crate::args::{self, Args, die};
use crate::commands::search::report_skipped_stores;
use crate::find::{self, FindOptions};
use crate::search::Disk;
use crate::sources::parse_source;
use crate::{Common, DEFAULT_FIND_LIMIT, DEFAULT_MAX_PARALLEL, Outcome, js};

pub fn run(mut args: Args, common: Common) -> Outcome {
    args.shift();
    let source = parse_source(
        &args
            .value(&["-s", "--source"])
            .unwrap_or_else(|| "all".into()),
    )?;
    let limit = args.integer(&["-n", "--limit"], "--limit", DEFAULT_FIND_LIMIT);
    let project = args.value(&["-p", "--project"]);
    let since = args.value(&["--since"]);
    let user_only = args.flag(&["--user"]);
    let paths_only = args.flag(&["--paths"]);
    let max_parallel = args.integer(&["--max-parallel"], "--max-parallel", DEFAULT_MAX_PARALLEL);
    let no_index = args.flag(&["--no-index"]);
    args.reject_unknown_flags();
    if args.is_empty() {
        die("find needs one or more terms");
    }
    let options = FindOptions {
        source,
        limit,
        project,
        since,
        user_only,
        max_parallel,
        no_index,
    };
    let result = find::find_sessions(&args.items, &options, &Disk)?;
    if paths_only {
        let paths: Vec<&str> = result.hits.iter().map(|hit| hit.path.as_str()).collect();
        println!("{}", paths.join("\n"));
    } else if common.json {
        println!("{}", js::pretty(&result));
    } else {
        println!("{}", find::render_find(&result));
    }
    report_skipped_stores(&result.skipped_stores, common.quiet);
    if !common.quiet && !common.json {
        let timings: Vec<String> = result
            .store_timings
            .entries()
            .into_iter()
            .map(|(store, ms)| format!("{store} {ms}ms"))
            .collect();
        eprintln!(
            "{}",
            args::dim(&format!(
                "{} · total {}ms",
                timings.join(" · "),
                result.elapsed_ms
            ))
        );
    }
    Ok(0)
}
