//! `test/cli-memory.test.ts`: the memory commands through the built binary.

use std::process::Command;

fn dejavu() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dejavu"));
    command
        .env("DEJAVU_NO_UPDATE_CHECK", "1")
        .env_remove("DEJAVU_EMBED_KEY")
        .env("DEJAVU_EMBED_KEY_FILE", "/nonexistent/embed-key")
        // Never the real agent wiki; a test that wants pages sets its own.
        .env(
            "WIKI_HOME",
            std::env::temp_dir().join(format!("dejavu-no-wiki-{}", std::process::id())),
        );
    command
}

fn temp_root(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("dejavu-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    root
}

#[test]
fn memory_show_accepts_the_exact_leading_hyphen_key_from_memory_list() {
    let root = temp_root("memory-cli");
    let dir = root.join("-Users-example-project").join("memory");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("MEMORY.md"), "# Synthetic project memory").unwrap();
    let root_arg = root.to_str().unwrap();

    let list = dejavu()
        .args(["memory", "list", "--root", root_arg, "--json"])
        .output()
        .unwrap();
    assert!(list.status.success());
    let projects: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    let project = projects[0]["project"].as_str().unwrap();
    assert_eq!(project, "-Users-example-project");

    let show = dejavu()
        .args(["memory", "show", project, "--root", root_arg])
        .output()
        .unwrap();
    assert!(show.status.success());
    assert!(String::from_utf8_lossy(&show.stdout).contains("Synthetic project memory"));

    let invalid = dejavu()
        .args(["memory", "show", "--typo", "--root", root_arg])
        .output()
        .unwrap();
    assert_eq!(invalid.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("unknown flag"));
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn memory_reads_claude_config_dir_projects() {
    let root = temp_root("memory-config");
    let dir = root.join("projects").join("-p").join("memory");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("notes.md"), "synthetic").unwrap();
    let files = dejavu()
        .args(["memory", "list", "--files"])
        .env("CLAUDE_CONFIG_DIR", &root)
        .output()
        .unwrap();
    assert!(files.status.success());
    let expected = format!("-p/notes.md\t{}\n", dir.join("notes.md").display());
    assert_eq!(String::from_utf8_lossy(&files.stdout), expected);
    std::fs::remove_dir_all(&root).unwrap();
}

/// An OpenAI-format embeddings endpoint on localhost. Vectors point along the
/// topic a text mentions, so a paraphrase lands near its note. Returns the
/// base URL and every request body received.
fn fake_embeddings_server() -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let bodies = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = bodies.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut length = 0;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let body = String::from_utf8(body).unwrap();
            let request: serde_json::Value = serde_json::from_str(&body).unwrap();
            seen.lock().unwrap().push(body);
            let data: Vec<serde_json::Value> = request["input"]
                .as_array()
                .unwrap()
                .iter()
                .enumerate()
                .map(|(index, text)| {
                    let text = text.as_str().unwrap().to_lowercase();
                    let axis = |words: &[&str]| {
                        f64::from(u8::from(words.iter().any(|word| text.contains(word))))
                    };
                    serde_json::json!({"index": index, "embedding": [
                        axis(&["storage", "partition"]), axis(&["deploy"]), axis(&["pancake"]), 0.05
                    ]})
                })
                .collect();
            let reply = serde_json::json!({"data": data}).to_string();
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            );
        }
    });
    (url, bodies)
}

#[test]
fn memory_search_finds_a_paraphrase_by_meaning_once_memory_is_embedded() {
    let root = temp_root("memory-semantic");
    let dir = root.join("-p").join("memory");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("space.md"),
        "---\nname: space\ndescription: Root partition notes\n---\n\nThe root partition ran out of room.",
    )
    .unwrap();
    std::fs::write(dir.join("ship.md"), "Deploy on Friday.").unwrap();
    std::fs::write(dir.join("MEMORY.md"), "- index line about the partition").unwrap();
    std::fs::write(
        dir.join("DO NOT READ UNLESS ASKED.md"),
        "gated partition plan",
    )
    .unwrap();
    let (url, bodies) = fake_embeddings_server();
    let run = |args: &[&str], base: &str| {
        dejavu()
            .args(args)
            .args(["--root", root.to_str().unwrap()])
            .env("DEJAVU_EMBED_KEY", "test-key")
            .env("DEJAVU_EMBED_BASE_URL", base)
            .env("DEJAVU_EMBED_MODEL", "fake-embed")
            .env("DEJAVU_EMBED_DIMS", "4")
            .env("DEJAVU_INDEX_PATH", root.join("index.sqlite"))
            .output()
            .unwrap()
    };
    let json = |output: &std::process::Output| -> serde_json::Value {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    };

    let embedded = json(&run(&["memory", "embed", "--json"], &url));
    assert_eq!(embedded["advance"]["embedded"], 2);
    assert_eq!(embedded["status"]["skippedFiles"], 2);
    let sent = bodies.lock().unwrap().join("\n");
    assert!(sent.contains("ran out of room"));
    assert!(!sent.contains("index line") && !sent.contains("gated partition plan"));

    // No file contains "storage": only the meaning matches.
    let hits = json(&run(
        &["memory", "search", "storage is full", "--json"],
        &url,
    ));
    assert_eq!(hits.as_array().unwrap().len(), 1);
    assert_eq!(hits[0]["name"], "space.md");
    assert_eq!(hits[0]["match"], "semantic");
    assert_eq!(hits[0]["description"], "Root partition notes");
    assert_eq!(
        hits[0]["snippets"][0],
        "The root partition ran out of room."
    );
    assert!(hits[0]["similarity"].as_f64().unwrap() > 0.9);

    // A keyword hit that is also the closest passage is reported as both.
    let both = json(&run(&["memory", "search", "partition", "--json"], &url));
    assert_eq!(both[0]["name"], "space.md");
    assert_eq!(both[0]["match"], "both");

    let requests = bodies.lock().unwrap().len();
    let literal = json(&run(
        &[
            "memory",
            "search",
            "storage is full",
            "--no-semantic",
            "--json",
        ],
        &url,
    ));
    assert_eq!(literal.as_array().unwrap().len(), 0);
    assert_eq!(bodies.lock().unwrap().len(), requests);

    // An unreachable provider costs only the semantic leg.
    let offline = run(
        &["memory", "search", "deploy", "--json"],
        "http://127.0.0.1:9/v1",
    );
    let found = json(&offline);
    assert_eq!(found[0]["name"], "ship.md");
    assert_eq!(found[0]["match"], "keyword");
    assert!(String::from_utf8_lossy(&offline.stderr).contains("keyword results only"));
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn memory_search_falls_back_to_keywords_on_a_bad_embedding_setting() {
    let root = temp_root("memory-bad-embed-env");
    let dir = root.join("-p").join("memory");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("ship.md"), "Deploy on Friday.").unwrap();
    for (name, value) in [
        ("DEJAVU_EMBED_DIMS", "many"),
        ("DEJAVU_EMBED_API", "carrier-pigeon"),
    ] {
        let search = dejavu()
            .args(["memory", "search", "deploy", "--json"])
            .args(["--root", root.to_str().unwrap()])
            .env("DEJAVU_EMBED_KEY", "test-key")
            .env("DEJAVU_EMBED_BASE_URL", "http://127.0.0.1:9/v1")
            .env("DEJAVU_INDEX_PATH", root.join("index.sqlite"))
            .env(name, value)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&search.stderr);
        assert!(search.status.success(), "{name}: {stderr}");
        assert!(
            stderr.contains("keyword results only") && stderr.contains(name),
            "{stderr}"
        );
        let found: serde_json::Value = serde_json::from_slice(&search.stdout).unwrap();
        assert_eq!(found[0]["name"], "ship.md");
        assert_eq!(found[0]["match"], "keyword");

        let embed = dejavu()
            .args(["memory", "embed", "--root", root.to_str().unwrap()])
            .env("DEJAVU_EMBED_KEY", "test-key")
            .env("DEJAVU_INDEX_PATH", root.join("index.sqlite"))
            .env(name, value)
            .output()
            .unwrap();
        assert_eq!(embed.status.code(), Some(1), "{name}");
    }
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn memory_search_and_embed_cover_wiki_pages_but_never_gated_ones() {
    let root = temp_root("memory-wiki");
    let dir = root.join("projects").join("-p").join("memory");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("space.md"),
        "---\nname: space\ndescription: Root partition notes\n---\n\nThe root partition ran out of room.",
    )
    .unwrap();
    let wiki = root.join("wiki");
    let pages = wiki.join("pages");
    std::fs::create_dir_all(pages.join("drafts")).unwrap();
    std::fs::create_dir_all(wiki.join("inbox")).unwrap();
    std::fs::write(
        pages.join("disk-cleanup.md"),
        "---\ntitle: Disk cleanup\nsummary: Clearing caches\nkind: topic\nscope: /Users/me/api\n---\n\nThe partition filled; clear the build caches.",
    )
    .unwrap();
    std::fs::write(
        pages.join("gated-vault.md"),
        "---\ntitle: Gated\nsummary: DO NOT READ UNLESS VAULT\nkind: topic\ngated: VAULT MENTIONED\n---\n\nsecret partition plan for the vault",
    )
    .unwrap();
    std::fs::write(
        pages.join("drafts").join("draft.md"),
        "draft partition notes",
    )
    .unwrap();
    std::fs::write(wiki.join("inbox").join("note.md"), "inbox partition notes").unwrap();
    std::fs::write(wiki.join("index.md"), "wiki index partition").unwrap();
    let (url, bodies) = fake_embeddings_server();
    let run = |args: &[&str]| {
        dejavu()
            .args(args)
            .args([
                "--root",
                dir.parent().unwrap().parent().unwrap().to_str().unwrap(),
            ])
            .env("WIKI_HOME", &wiki)
            .env("DEJAVU_EMBED_KEY", "test-key")
            .env("DEJAVU_EMBED_BASE_URL", &url)
            .env("DEJAVU_EMBED_MODEL", "fake-embed")
            .env("DEJAVU_EMBED_DIMS", "4")
            .env("DEJAVU_INDEX_PATH", root.join("index.sqlite"))
            .output()
            .unwrap()
    };
    let json = |output: &std::process::Output| -> serde_json::Value {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    };

    let embedded = json(&run(&["memory", "embed", "--json"]));
    assert_eq!(embedded["advance"]["embedded"], 2);
    assert_eq!(embedded["wiki"]["files"], 2);
    assert_eq!(embedded["wiki"]["skippedFiles"], 1);
    assert_eq!(embedded["wiki"]["done"], 1);
    assert_eq!(embedded["status"]["files"], 3);
    let sent = bodies.lock().unwrap().join("\n");
    assert!(sent.contains("clear the build caches"));
    for withheld in ["vault", "VAULT", "draft", "inbox", "wiki index"] {
        assert!(!sent.contains(withheld), "{withheld} was sent");
    }

    // By meaning, the wiki page lists first; it is labelled as a wiki page.
    let hits = json(&run(&["memory", "search", "storage is full", "--json"]));
    let found: Vec<(&str, &str)> = hits
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| {
            (
                hit["name"].as_str().unwrap(),
                hit["source"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(found, [("disk-cleanup.md", "wiki"), ("space.md", "memory")]);
    assert_eq!(hits[0]["match"], "semantic");
    assert_eq!(hits[0]["description"], "Clearing caches");
    let text = run(&["memory", "search", "storage is full"]);
    let text = String::from_utf8_lossy(&text.stdout);
    assert!(
        text.contains(&format!(
            "\twiki:disk-cleanup\t{}",
            pages.join("disk-cleanup.md").display()
        )),
        "{text}"
    );

    // By keyword, gated pages and non-pages never match.
    let literal = json(&run(&[
        "memory",
        "search",
        "partition",
        "--no-semantic",
        "--json",
    ]));
    let names: Vec<&str> = literal
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["disk-cleanup.md", "space.md"]);
    for gated in ["vault", "draft", "inbox"] {
        let none = json(&run(&[
            "memory",
            "search",
            gated,
            "--no-semantic",
            "--json",
        ]));
        assert_eq!(none.as_array().unwrap().len(), 0, "{gated}");
    }
    std::fs::remove_dir_all(&root).unwrap();
}
