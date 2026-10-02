use crate::args::{Args, die};
use crate::js;
use crate::memory::{self, Disk};
use crate::{Common, Outcome};

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
            let limit = args.integer(&["-n", "--limit"], "--limit", 20);
            let snippets = args.integer(&["--snippets"], "--snippets", 3);
            args.reject_unknown_flags();
            let query = args.items.join(" ");
            let query = query.trim();
            if query.is_empty() {
                die("memory search needs one token or exact phrase");
            }
            let result = memory::search_memories(query, &root, limit, snippets, &Disk)?;
            if common.json {
                println!("{}", js::pretty(&result));
            } else {
                let blocks: Vec<String> = result
                    .iter()
                    .map(|found| {
                        format!(
                            "{}\t{}/{}\t{}\n  {}",
                            found.count,
                            found.project,
                            found.name,
                            found.path,
                            found.snippets.join("\n  ")
                        )
                    })
                    .collect();
                println!("{}", blocks.join("\n"));
            }
            Ok(0)
        }
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
            "unknown memory command '{verb}' (list|search|show)"
        )),
    }
}
