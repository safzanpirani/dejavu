use crate::args::{Args, die};
use crate::commands::search::report_skipped_stores;
use crate::index::{default_index_path, refresh_transcript_index, transcript_index_status};
use crate::sources::discover_stores;
use crate::types::SourceSelector;
use crate::{Common, DEFAULT_MAX_PARALLEL, Outcome, js};

pub fn run(mut args: Args, common: Common) -> Outcome {
    args.shift();
    let verb = args.shift().unwrap_or_else(|| "status".into());
    args.reject_unknown_flags();
    if let Some(extra) = args.first() {
        die(&format!(
            "index {verb} accepts no positional arguments (unexpected: '{extra}')"
        ));
    }
    let path = default_index_path();
    match verb.as_str() {
        "status" => {
            let result = transcript_index_status(&path);
            if common.json {
                println!("{}", js::pretty(&result));
            } else if result.exists {
                println!(
                    "{} files · {} messages · {} bytes · schema v{} · {}",
                    result.files, result.messages, result.bytes, result.schema_version, result.path
                );
            } else {
                println!("not built · {}", result.path);
            }
            Ok(0)
        }
        "update" | "rebuild" => {
            let stores = discover_stores(SourceSelector::All);
            let result =
                refresh_transcript_index(&stores, &path, verb == "rebuild", DEFAULT_MAX_PARALLEL)?;
            if common.json {
                println!("{}", js::pretty(&result));
            } else {
                println!(
                    "{} files · {} messages · {} indexed · {} removed · {}ms · {}",
                    result.files,
                    result.messages,
                    result.indexed,
                    result.removed,
                    result.elapsed_ms,
                    result.path
                );
            }
            report_skipped_stores(&result.skipped, common.quiet || common.json);
            Ok(0)
        }
        _ => die(&format!(
            "unknown index command '{verb}' (status|update|rebuild)"
        )),
    }
}
