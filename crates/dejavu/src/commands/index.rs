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
            advance_memory_embeddings(&path, common);
            Ok(0)
        }
        _ => die(&format!(
            "unknown index command '{verb}' (status|update|rebuild)"
        )),
    }
}

/// With an embedding key stored, an index update also moves the memory
/// embedding queue. Its failures are notes: the transcript index is done.
fn advance_memory_embeddings(path: &str, common: Common) {
    let outcome = (|| -> Result<Vec<String>, String> {
        let Some(config) = crate::embed::config_from_env()? else {
            return Ok(Vec::new());
        };
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as i64);
        let mut database = crate::memory_embed::open_store(path)?;
        crate::memory_embed::sync(
            &mut database,
            &crate::memory::default_memory_root(),
            &crate::memory::Disk,
            now_ms,
        )?;
        let embedder = crate::embed::HttpEmbedder::new(config);
        let advanced = crate::memory_embed::advance(
            &database,
            &embedder,
            &crate::memory_embed::AdvanceOptions {
                now_ms,
                immediate: false,
                timeout: std::time::Duration::from_secs(120),
                max_standard: None,
                rate_limit_wait: None,
                progress: None,
            },
        )?;
        Ok(advanced.notes)
    })();
    if common.quiet || common.json {
        return;
    }
    match outcome {
        Ok(notes) => notes
            .iter()
            .for_each(|text| eprintln!("memory embeddings: {text}")),
        Err(error) => eprintln!("memory embeddings: {error}"),
    }
}
