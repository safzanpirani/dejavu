use crate::args::{self, Args, die};
use crate::search::{self, Disk, SearchOptions};
use crate::sources::parse_source;
use crate::types::StoreDiagnostic;
use crate::{
    Common, DEFAULT_MAX_PARALLEL, DEFAULT_SEARCH_LIMIT, DEFAULT_SNIPPET_LIMIT, Outcome, js,
};

/// `reportSkippedStores`: one dim stderr line per unreadable store.
pub fn report_skipped_stores(diagnostics: &[StoreDiagnostic], quiet: bool) {
    if quiet {
        return;
    }
    for diagnostic in diagnostics {
        eprintln!(
            "{}",
            args::dim(&format!(
                "skipped unreadable {} store {}: {}",
                diagnostic.source, diagnostic.path, diagnostic.error
            ))
        );
    }
}

pub fn run(mut args: Args, common: Common) -> Outcome {
    let source = parse_source(
        &args
            .value(&["-s", "--source"])
            .unwrap_or_else(|| "all".into()),
    )?;
    let limit = args.integer(&["-n", "--limit"], "--limit", DEFAULT_SEARCH_LIMIT);
    let snippets = args.integer(&["--snippets"], "--snippets", DEFAULT_SNIPPET_LIMIT);
    let max_parallel = args.integer(&["--max-parallel"], "--max-parallel", DEFAULT_MAX_PARALLEL);
    let no_index = args.flag(&["--no-index"]);
    args.reject_unknown_flags();
    let query = args.items.join(" ");
    let query = query.trim();
    if query.is_empty() {
        die("need one token or exact phrase to search");
    }
    let options = SearchOptions {
        source,
        limit,
        snippets,
        max_parallel,
        no_index,
    };
    let result = search::search_sessions(query, options, &Disk)?;
    if common.json {
        println!("{}", js::pretty(&result));
    } else {
        println!(
            "{}",
            crate::render::render_search(
                &serde_json::to_value(&result).map_err(|e| e.to_string())?
            )
        );
    }
    report_skipped_stores(&result.skipped_stores, common.quiet);
    if !common.quiet && !common.json {
        let sources: Vec<&str> = result.sources.iter().map(|s| s.as_str()).collect();
        eprintln!(
            "{}",
            args::dim(&format!("{} · {}ms", sources.join(","), result.elapsed_ms))
        );
    }
    Ok(0)
}
