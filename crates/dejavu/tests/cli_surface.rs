//! Public command syntax, help, and JSON contracts through the built binary.
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let f = Self(std::env::temp_dir().join(format!(
            "dejavu-cli-surface-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )));
        std::fs::create_dir_all(f.0.join("claude/projects/demo/memory")).unwrap();
        std::fs::write(f.locator(), concat!(
            "{\"type\":\"user\",\"uuid\":\"u1\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"/demo\",\"message\":{\"role\":\"user\",\"content\":\"No sessions found. search failed. --help --no-color\"}}\n",
            "{\"type\":\"assistant\",\"uuid\":\"a1\",\"parentUuid\":\"u1\",\"timestamp\":\"2026-01-01T00:00:01Z\",\"message\":{\"role\":\"assistant\",\"content\":\"Try a search.\"}}\n"
        )).unwrap();
        std::fs::write(
            f.0.join("claude/projects/demo/memory/MEMORY.md"),
            "Synthetic needle memory",
        )
        .unwrap();
        f
    }
    fn locator(&self) -> String {
        // Use the CLI's portable slash convention for transcript locators.
        self.0
            .join("claude/projects/demo/session.jsonl")
            .to_string_lossy()
            .replace('\\', "/")
    }
    fn command(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_dejavu"));
        c.env("DEJAVU_NO_UPDATE_CHECK", "1")
            .env("CLAUDE_CONFIG_DIR", self.0.join("claude"))
            .env("CODEX_HOME", self.0.join("codex"))
            .env("PI_CODING_AGENT_DIR", self.0.join("pi"))
            .env("OPENCODE_DB", self.0.join("absent.db"))
            .env("FACTORY_HOME_OVERRIDE", self.0.join("factory"))
            .env("DEJAVU_INDEX_PATH", self.0.join("index.sqlite"))
            .env_remove("NO_COLOR")
            .env_remove("CODEX_THREAD_ID")
            .env_remove("CLAUDE_SESSION_ID")
            .env_remove("CLAUDE_CODE_SESSION_ID");
        c
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }
    fn json(&self, args: &[&str]) -> serde_json::Value {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
}

#[test]
fn explicit_search_matches_bare_search_and_literal_search_remains_available() {
    let f = Fixture::new();
    for no_index in [false, true] {
        let mut args = vec!["No sessions found", "--source", "claude", "--json"];
        if no_index {
            args.push("--no-index");
        }
        let mut bare = f.json(&args);
        args.insert(0, "search");
        let mut explicit = f.json(&args);
        bare.as_object_mut().unwrap().remove("elapsedMs");
        explicit.as_object_mut().unwrap().remove("elapsedMs");
        assert_eq!(explicit, bare);
        assert_eq!(explicit["query"], "No sessions found");
        assert_eq!(explicit["matches"].as_array().unwrap().len(), 1);
    }
    for (args, query) in [
        (vec!["--json", "--", "search"], "search"),
        (vec!["search", "search", "--json"], "search"),
        (vec!["search failed", "--json"], "search failed"),
        (vec!["search", "--json", "--", "--help"], "--help"),
    ] {
        assert_eq!(f.json(&args)["query"], query);
    }
    assert_eq!(f.run(&["search", "--json"]).status.code(), Some(1));
}

#[path = "support/help_contract.rs"]
mod help_contract;

fn help(f: &Fixture, topic: &[&str]) -> String {
    let mut args = topic.to_vec();
    args.push("--help");
    let out = f.run(&args);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn help_routes_each_command_and_nested_topic_without_ansi_in_pipes() {
    let f = Fixture::new();
    for topic in [
        vec!["search"],
        vec!["find"],
        vec!["pack"],
        vec!["last"],
        vec!["show"],
        vec!["transcript"],
        vec!["view"],
        vec!["scrub"],
        vec!["query"],
        vec!["profile"],
        vec!["memory"],
        vec!["index"],
        vec!["self-update"],
        vec!["memory", "list"],
        vec!["memory", "search"],
        vec!["memory", "show"],
        vec!["index", "status"],
        vec!["index", "update"],
        vec!["index", "rebuild"],
    ] {
        let text = help(&f, &topic);
        assert!(
            text.starts_with(&format!("dejavu {}\n", topic.join(" "))),
            "{text}"
        );
        for required in ["Usage:", "Flags:", "JSON", "jq '", "Exit codes:"] {
            assert!(text.contains(required), "{topic:?}: missing {required}");
        }
        assert!(!text.contains('\x1b'));
        let mut args = vec!["help"];
        args.extend_from_slice(&topic);
        assert_eq!(f.run(&args).stdout, text.as_bytes());
        args.extend(["--color", "--no-color"]);
        let out = f.command().env("NO_COLOR", "").args(args).output().unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout, text.as_bytes());
    }
    let overview = help(&f, &[]);
    assert!(overview.len() < 2500);
    assert!(!overview.contains('\x1b'));
    assert_eq!(f.run(&["help"]).stdout, overview.as_bytes());
    assert_eq!(f.run(&["--help", "--color"]).stdout, overview.as_bytes());
    assert_eq!(
        f.run(&["--json", "find", "--help"]).stdout,
        help(&f, &["find"]).as_bytes()
    );
    assert_eq!(f.run(&["help", "bogus"]).status.code(), Some(1));
    assert_eq!(f.run(&["memory", "bogus", "--help"]).status.code(), Some(1));
}

#[test]
fn documented_json_keys_match_real_fixture_commands() {
    let f = Fixture::new();
    let locator = f.locator();
    for (topic, args, variant) in [
        (vec!["search"], vec!["search", "found", "--json"], 0),
        (vec!["find"], vec!["find", "found", "--json"], 0),
        (vec!["pack"], vec!["pack", "found", "--json"], 0),
        (vec!["last"], vec!["last", &locator, "--json"], 0),
        (
            vec!["last"],
            vec!["last", "--anywhere", "--list", "--json"],
            0,
        ),
        (vec!["show"], vec!["show", &locator, "--json"], 0),
        (
            vec!["transcript"],
            vec!["transcript", &locator, "--json"],
            0,
        ),
        (
            vec!["transcript"],
            vec!["transcript", &locator, "--limit", "1", "--json"],
            0,
        ),
        (vec!["view"], vec!["view", &locator, "--json"], 0),
        (
            vec!["scrub"],
            vec![
                "scrub",
                &locator,
                "--pattern",
                "found",
                "--dry-run",
                "--json",
            ],
            0,
        ),
        (vec!["profile"], vec!["profile", &locator, "--json"], 0),
        (vec!["memory", "list"], vec!["memory", "list", "--json"], 0),
        (
            vec!["memory", "list"],
            vec!["memory", "list", "--files", "--json"],
            1,
        ),
        (
            vec!["memory", "search"],
            vec!["memory", "search", "needle", "--json"],
            0,
        ),
        (
            vec!["memory", "show"],
            vec!["memory", "show", "demo", "--json"],
            0,
        ),
        (
            vec!["index", "status"],
            vec!["index", "status", "--json"],
            0,
        ),
        (
            vec!["index", "update"],
            vec!["index", "update", "--json"],
            0,
        ),
        (
            vec!["index", "rebuild"],
            vec!["index", "rebuild", "--json"],
            0,
        ),
    ] {
        let value = f.json(&args);
        help_contract::assert_shape(&help(&f, &topic), &value, variant);
        if topic[0] == "memory" || topic[0] == "index" {
            let parent_variant = match topic.as_slice() {
                ["memory", "list"] => variant,
                ["memory", "search"] => 2,
                ["memory", "show"] => 3,
                ["index", "status"] => 0,
                _ => 1,
            };
            help_contract::assert_shape(&help(&f, &topic[..1]), &value, parent_variant);
        }
    }
}

#[cfg(unix)]
#[test]
fn query_json_help_matches_a_fake_model_without_paid_calls() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    std::fs::create_dir_all(f.0.join("bin")).unwrap();
    let fake = f.0.join("bin/codex");
    std::fs::write(
        &fake,
        r#"#!/bin/sh
for arg in "$@"; do
  [ "$prev" = --output-last-message ] && out=$arg
  prev=$arg
done
cat >/dev/null
printf 'Synthetic answer' > "$out"
printf '%s\n' '{"type":"turn.completed","usage":{"input_tokens":4,"output_tokens":2}}'
"#,
    )
    .unwrap();
    std::fs::set_permissions(fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    for topic in ["query", "profile"] {
        let mut cmd = f.command();
        cmd.env(
            "PATH",
            format!("{}:/usr/bin:/bin", f.0.join("bin").display()),
        )
        .env("XDG_CONFIG_HOME", f.0.join("config"))
        .env_remove("DEJAVU_QUERY_HARNESS")
        .env_remove("DEJAVU_QUERY_MODEL")
        .env_remove("DEJAVU_QUERY_EFFORT")
        .env_remove("DEJAVU_QUERY_VIA_PI")
        .args([topic, &f.locator(), "--json"]);
        if topic == "query" {
            cmd.arg("What happened?");
        } else {
            cmd.arg("--explain");
        }
        let out = cmd.output().unwrap();
        // The profile fake intentionally returns non-JSON explanation text.
        // It still emits the deterministic report plus an explanation error.
        assert_eq!(
            out.status.code(),
            Some(if topic == "query" { 0 } else { 1 })
        );
        let value = serde_json::from_slice(&out.stdout).unwrap();
        help_contract::assert_shape(&help(&f, &[topic]), &value, 0);
    }
}

#[test]
fn pack_help_documents_omission_entries_from_a_bounded_fixture() {
    let f = Fixture::new();
    let value = f.json(&["pack", "found", "--budget-chars", "1", "--json"]);
    let text = help(&f, &["pack"]);
    help_contract::assert_shape(&text, &value, 0);
    help_contract::assert_shape(&text, &value["omitted"], 1);
    assert_eq!(
        PathBuf::from(value["omitted"][0]["path"].as_str().unwrap()),
        PathBuf::from(f.locator())
    );
    assert!(value["omitted"][0]["events"].as_u64().unwrap() > 0);
    assert!(value["omitted"][0]["nextEvent"].is_u64());
}
