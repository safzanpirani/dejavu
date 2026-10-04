//! Retrieval regressions use synthetic stores and a private index.
use serde_json::{Value, json};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Fixture(std::path::PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "dejavu-find-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(path.join("claude/projects/demo")).unwrap();
        Self(path)
    }
    fn run(&self, args: &[&str]) -> Value {
        let out = Command::new(env!("CARGO_BIN_EXE_dejavu"))
            .env("CLAUDE_CONFIG_DIR", self.0.join("claude"))
            .env("CODEX_HOME", self.0.join("codex"))
            .env("DEJAVU_INDEX_PATH", self.0.join("index.sqlite"))
            .env("DEJAVU_NO_UPDATE_CHECK", "1")
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn claude(role: &str, text: &str, date: &str) -> String {
    json!({"message":{"role":role,"content":text},"timestamp":date}).to_string()
}
#[test]
fn late_dialogue_survives_hundreds_of_matching_tool_rows() {
    let fixture = Fixture::new();
    let mut lines = vec![claude("user", "needle opening", "2026-09-04")];
    for _ in 0..450 {
        lines.push(json!({"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"needle tool output"}]}}).to_string());
    }
    for i in 0..49 {
        lines.push(claude(
            "assistant",
            &format!("needle dialogue {i}"),
            "2026-09-22",
        ));
    }
    std::fs::write(
        fixture.0.join("claude/projects/demo/session.jsonl"),
        lines.join("\n"),
    )
    .unwrap();
    for direct in [false, true] {
        let mut args = vec!["find", "needle", "--source", "claude", "--json"];
        if direct {
            args.push("--no-index");
        }
        let result = fixture.run(&args);
        assert_eq!(
            result["hits"][0]["termCounts"]["needle"],
            json!({"user":1,"assistant":49})
        );
        assert_eq!(result["hits"][0]["date"], "2026-09-22");
    }
}
