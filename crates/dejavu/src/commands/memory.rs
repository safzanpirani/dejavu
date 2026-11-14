use std::time::Duration;

use crate::args::{self, Args, die};
use crate::embed::{self, HttpEmbedder};
use crate::index::default_index_path;
use crate::js;
use crate::memory::{self, Disk};
use crate::memory_embed::{self, AdvanceOptions, RankedMatch};
use crate::{Common, Outcome};

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64)
}

fn note(common: Common, text: &str) {
    if !common.quiet {
        eprintln!("{}", args::dim_stderr(text));
    }
}

/// Keyword search, joined with semantic search when an embedding key is stored.
/// A semantic failure (offline, provider error, busy index) costs only the
/// semantic leg: the keyword results still print.
fn search(
    query: &str,
    root: &str,
    options: &memory::SearchOptions,
    semantic: bool,
    common: Common,
) -> Result<Vec<RankedMatch>, String> {
    // A bad DEJAVU_EMBED_* setting costs only the semantic leg, as an outage does.
    let config = if semantic {
        embed::config_from_env().unwrap_or_else(|error| {
            note(
                common,
                &format!("semantic search unavailable, keyword results only: {error}"),
            );
            None
        })
    } else {
        None
    };
    let Some(config) = config else {
        return Ok(memory_embed::keyword_only(memory::search_memories(
            query, root, options, &Disk,
        )?));
    };
    // Fusion reads deeper into both rankings than it returns.
    let depth = options.limit.max(10) * 2;
    let wide = memory::SearchOptions {
        limit: depth,
        ..options.clone()
    };
    let mut keyword = memory::search_memories(query, root, &wide, &Disk)?;
    let embedder = HttpEmbedder::new(config);
    match memory_embed::semantic_search(
        query,
        root,
        options,
        &Disk,
        &default_index_path(),
        &embedder,
        now_ms(),
        depth,
    ) {
        Ok((found, info)) => {
            for text in &info.notes {
                note(common, &format!("semantic: {text}"));
            }
            if info.waiting > 0 && !common.json {
                note(
                    common,
                    &format!(
                        "semantic: {} passages embedded, {} waiting (dejavu memory embed)",
                        info.vectors, info.waiting
                    ),
                );
            }
            Ok(memory_embed::fuse(keyword, found, options.limit))
        }
        Err(error) => {
            note(
                common,
                &format!("semantic search unavailable, keyword results only: {error}"),
            );
            keyword.truncate(options.limit);
            Ok(memory_embed::keyword_only(keyword))
        }
    }
}

fn embed_command(mut args: Args, root: &str, common: Common) -> Outcome {
    let immediate = args.flag(&["--now"]);
    let status_only = args.flag(&["--status"]);
    let retry_failed = args.flag(&["--retry-failed"]);
    args.reject_unknown_flags();
    if let Some(extra) = args.first() {
        die(&format!(
            "memory embed accepts no positional arguments (unexpected: '{extra}')"
        ));
    }
    let mut database = memory_embed::open_store(&default_index_path())?;
    let now = now_ms();
    let wiki = memory::default_wiki_pages();
    let synced = memory_embed::sync(&mut database, root, Some(&wiki), &Disk, now)?;
    let advanced = if status_only {
        None
    } else {
        let Some(config) = embed::config_from_env()? else {
            return Err(format!(
                "semantic memory search is off: store an embeddings API key in {} (or set DEJAVU_EMBED_KEY); see dejavu memory embed --help",
                embed::key_file_path().display()
            ));
        };
        if retry_failed {
            memory_embed::requeue_failed(&database)?;
        }
        let progress = |done: usize, total: usize| {
            if !common.quiet && !common.json {
                eprintln!("{}", args::dim_stderr(&format!("embedded {done}/{total}")));
            }
        };
        let embedder = HttpEmbedder::new(config);
        Some(memory_embed::advance(
            &database,
            &embedder,
            &AdvanceOptions {
                now_ms: now,
                immediate,
                timeout: Duration::from_secs(120),
                max_standard: None,
                rate_limit_wait: Some(Duration::from_secs(62)),
                progress: Some(&progress),
            },
        )?)
    };
    let status = memory_embed::status(&database, &[root, &wiki])?;
    let pages = memory_embed::status(&database, &[&wiki])?;
    if common.json {
        let mut value = serde_json::json!({"sync": synced});
        if let Some(advanced) = &advanced {
            value["advance"] = serde_json::to_value(advanced).map_err(|e| e.to_string())?;
        }
        value["status"] = serde_json::to_value(&status).map_err(|e| e.to_string())?;
        value["wiki"] = serde_json::to_value(&pages).map_err(|e| e.to_string())?;
        println!("{}", js::pretty(&value));
    } else {
        if let Some(advanced) = &advanced {
            println!(
                "{} embedded · {} collected from batches · {} submitted to a batch · {} failed",
                advanced.embedded, advanced.collected, advanced.submitted, advanced.failed
            );
            for text in &advanced.notes {
                println!("note: {text}");
            }
        }
        println!(
            "{} files ({} never sent) · {} chunks: {} embedded, {} pending, {} in batches, {} to retry, {} failed, {} withheld",
            status.files,
            status.skipped_files,
            status.chunks,
            status.done,
            status.pending,
            status.submitted,
            status.retry,
            status.failed,
            status.withheld
        );
        println!(
            "wiki ({wiki}): {} pages ({} never sent) · {} chunks: {} embedded, {} pending, {} withheld",
            pages.files,
            pages.skipped_files,
            pages.chunks,
            pages.done,
            pages.pending,
            pages.withheld
        );
    }
    Ok(0)
}

pub fn run(mut args: Args, common: Common) -> Outcome {
    args.shift();
    let verb = args.shift().unwrap_or_else(|| "list".into());
    let root = args
        .value(&["--root"])
        .unwrap_or_else(memory::default_memory_root);
    match verb.as_str() {
        "list" => {
            let files = args.flag(&["--files"]);
            args.reject_unknown_flags();
            if let Some(extra) = args.first() {
                die(&format!(
                    "memory list accepts no positional arguments (unexpected: '{extra}')"
                ));
            }
            if files {
                let result = memory::memory_files(&root, &Disk)?;
                if common.json {
                    println!("{}", js::pretty(&result));
                } else {
                    let lines: Vec<String> = result
                        .iter()
                        .map(|file| format!("{}/{}\t{}", file.project, file.name, file.path))
                        .collect();
                    println!("{}", lines.join("\n"));
                }
            } else {
                let result = memory::list_memories(&root, &Disk)?;
                if common.json {
                    println!("{}", js::pretty(&result));
                } else {
                    let lines: Vec<String> = result
                        .iter()
                        .map(|project| {
                            format!("{}\t{}\t{}", project.project, project.files, project.path)
                        })
                        .collect();
                    println!("{}", lines.join("\n"));
                }
            }
            Ok(0)
        }
        "search" => {
            let options = memory::SearchOptions {
                limit: args.integer(&["-n", "--limit"], "--limit", 20),
                snippets: args.integer(&["--snippets"], "--snippets", 3),
                project: args.value(&["-p", "--project"]),
                kind: args.value(&["--type"]),
                wiki: Some(memory::default_wiki_pages()),
            };
            let semantic = !args.flag(&["--no-semantic"]);
            args.reject_unknown_flags();
            let query = args.items.join(" ");
            let query = query.trim();
            if query.is_empty() {
                die("memory search needs at least one term");
            }
            let result =
                memory_embed::wiki_first(search(query, &root, &options, semantic, common)?);
            if common.json {
                println!("{}", js::pretty(&result));
            } else {
                let blocks: Vec<String> = result
                    .iter()
                    .map(|ranked| {
                        let found = &ranked.found;
                        let mut block =
                            format!("{}\t{}\t{}", found.count, found.label(), found.path);
                        if let Some(similarity) = ranked.similarity {
                            block.push_str(&format!("\t~{similarity:.2}"));
                        }
                        match (&found.kind, &found.description) {
                            (Some(kind), Some(description)) => {
                                block.push_str(&format!("\n  [{kind}] {description}"));
                            }
                            (None, Some(description)) => {
                                block.push_str(&format!("\n  {description}"));
                            }
                            _ => {}
                        }
                        for snippet in &found.snippets {
                            block.push_str(&format!("\n  {snippet}"));
                        }
                        block
                    })
                    .collect();
                println!("{}", blocks.join("\n"));
            }
            Ok(0)
        }
        "embed" => embed_command(args, &root, common),
        "show" => {
            // Claude project keys emitted by memory list begin with a single hyphen.
            args.reject_unknown(|arg| arg.starts_with("--"));
            let Some(selector) = args.shift() else {
                die("memory show needs a project slug, project substring, or memory file path");
            };
            if let Some(extra) = args.first() {
                die(&format!(
                    "memory show accepts one selector (unexpected: '{extra}')"
                ));
            }
            let result = memory::show_memory(&selector, &root, &Disk)?;
            println!(
                "{}",
                if common.json {
                    js::pretty(&result)
                } else {
                    result.content
                }
            );
            Ok(0)
        }
        _ => die(&format!(
            "unknown memory command '{verb}' (list|search|show|embed)"
        )),
    }
}
