//! `show` and `transcript` accept a unique session id prefix, as a short git hash.
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Fixture(PathBuf);

impl Fixture {
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_dejavu"))
            .env("CLAUDE_CONFIG_DIR", self.0.join("claude"))
            .env("CODEX_HOME", self.0.join("codex"))
            .env("DEJAVU_INDEX_PATH", self.0.join("index.sqlite"))
            .env("DEJAVU_NO_UPDATE_CHECK", "1")
            .env("HOME", &self.0)
            .current_dir(&self.0)
            .args(args)
            .output()
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn session(dir: &Path, id: &str, text: &str) {
    let line = json!({ "type": "user", "uuid": "m0", "timestamp": "2026-09-02T10:00:00Z", "message": { "role": "user", "content": text } });
    std::fs::write(dir.join(format!("{id}.jsonl")), format!("{line}\n")).unwrap();
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn show_and_transcript_resolve_a_unique_session_id_prefix() {
    let root = std::env::temp_dir().join(format!("dejavu-prefix-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let fixture = Fixture(root.clone());
    let projects = root.join("claude/projects/demo");
    std::fs::create_dir_all(&projects).unwrap();
    session(
        &projects,
        "358a49ad-1111-2222-3333-444444444444",
        "first session",
    );
    session(
        &projects,
        "358a49ad-9999-2222-3333-444444444444",
        "second session",
    );
    session(
        &projects,
        "7f00e1b2-1111-2222-3333-444444444444",
        "third session",
    );

    let shown = fixture.run(&["show", "7f00e1b2", "--json"]);
    assert!(shown.status.success(), "{}", stderr(&shown));
    let value: Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(
        value["messages"],
        json!([{ "role": "user", "text": "third session" }])
    );

    let transcript = fixture.run(&["transcript", "358A49AD-1", "--json"]);
    assert!(transcript.status.success(), "{}", stderr(&transcript));
    assert!(String::from_utf8_lossy(&transcript.stdout).contains("first session"));

    let ambiguous = fixture.run(&["show", "358a49ad"]);
    assert!(!ambiguous.status.success());
    let message = stderr(&ambiguous);
    assert!(
        message.contains("session id prefix 358a49ad matches 2 sessions"),
        "{message}"
    );
    assert!(
        message.contains("358a49ad-1111-2222-3333-444444444444"),
        "{message}"
    );
    assert!(
        message.contains("358a49ad-9999-2222-3333-444444444444"),
        "{message}"
    );

    let missing = fixture.run(&["show", "0badc0de"]);
    assert!(!missing.status.success());
    assert!(stderr(&missing).contains("no transcript found for session id prefix 0badc0de"));
}
