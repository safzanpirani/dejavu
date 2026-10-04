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
