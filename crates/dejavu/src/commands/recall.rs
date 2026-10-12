use crate::args::{self, Args, die};
use crate::commands::search::report_skipped_stores;
use crate::recall::{self, RecallOptions};
use crate::search::Disk;
use crate::sources::parse_source;
use crate::{Common, DEFAULT_MAX_PARALLEL, Outcome, js};

pub fn run(mut args: Args, common: Common) -> Outcome {
    crate::args::configure_stderr(&args);
    args.shift();
    let source = parse_source(
        &args
            .value(&["-s", "--source"])
            .unwrap_or_else(|| "all".into()),
    )?;
    let limit = args.integer(&["-n", "--limit"], "--limit", recall::DEFAULT_RECALL_LIMIT);
    let project = args.value(&["-p", "--project"]);
    let since = args.value(&["--since"]);
    let keywords = args.values(&["-k", "--keyword"]);
    let candidates = args.integer(
        &["--candidates"],
        "--candidates",
        recall::DEFAULT_RECALL_CANDIDATES,
    );
    let threshold = match args.value(&["--threshold"]) {
        Some(raw) => raw
            .parse::<f64>()
            .map_err(|_| format!("--threshold must be a number between 0 and 1 (got '{raw}')"))?,
        None => recall::DEFAULT_RECALL_THRESHOLD,
    };
    let model = args
        .value(&["--model"])
        .unwrap_or_else(|| recall::DEFAULT_RECALL_MODEL.into());
    let paths_only = args.flag(&["--paths"]);
    let max_parallel = args.integer(&["--max-parallel"], "--max-parallel", DEFAULT_MAX_PARALLEL);
    let no_index = args.flag(&["--no-index"]);
    let color = args::color_choice(&mut args, common.json);
    args.reject_unknown_flags();
    if args.is_empty() {
        die("recall needs a description of the session");
    }
    let options = RecallOptions {
        source,
        limit,
        project,
        since,
        keywords,
        candidates,
        threshold,
        model,
        max_parallel,
        no_index,
    };
    let result = recall::recall(&args.items.join(" "), &options, &Disk)?;
    if paths_only {
        let paths: Vec<&str> = result.hits.iter().map(|hit| hit.path.as_str()).collect();
        println!("{}", paths.join("\n"));
    } else if common.json {
        println!("{}", js::pretty(&result));
    } else {
        println!(
            "{}",
            crate::render::render_recall(
                &serde_json::to_value(&result).map_err(|e| e.to_string())?,
                color
            )
        );
    }
    report_skipped_stores(&result.skipped_stores, common.quiet);
    if !common.quiet && !common.json {
        eprintln!(
            "{}",
            args::dim_stderr(&format!(
                "{} candidates · {} input tokens · ${:.4} · {} · total {}ms",
                result.candidates,
                result.input_tokens,
                result.cost_usd,
                result.model,
                result.elapsed_ms
            ))
        );
    }
    Ok(0)
}
