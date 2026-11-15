//! Results carry a `dejavu://<harness>/<id>` address, and commands that take a
//! locator open it.
use serde_json::{Value, json};
use std::path::PathBuf;
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

fn line(uuid: &str, text: &str) -> String {
    json!({ "type": "user", "uuid": uuid, "timestamp": "2026-09-02T10:00:00Z", "message": { "role": "user", "content": text } }).to_string()
}

/// A reply names its parent: a Claude transcript is read as the chain that ends at its last message.
fn reply(uuid: &str, parent: &str, text: &str) -> String {
    json!({ "type": "assistant", "uuid": uuid, "parentUuid": parent, "timestamp": "2026-09-02T10:00:05Z", "message": { "role": "assistant", "content": [{ "type": "text", "text": text }] } }).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn a_found_session_is_opened_by_the_address_its_result_carries() {
    let root = std::env::temp_dir().join(format!("dejavu-address-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let fixture = Fixture(root.clone());
    let id = "7f00e1b2-1111-2222-3333-444444444444";
    let projects = root.join("claude/projects/-work-demo");
    std::fs::create_dir_all(projects.join(id).join("subagents")).unwrap();
    std::fs::write(
        projects.join(format!("{id}.jsonl")),
        format!(
            "{}\n{}\n",
            line("m0", "walrus opening"),
            reply("m1", "m0", "walrus follow-up")
        ),
    )
    .unwrap();
    std::fs::write(
        projects.join(id).join("subagents/agent-1.jsonl"),
        format!("{}\n", line("s0", "narwhal subagent")),
    )
    .unwrap();

    let found = fixture.run(&["find", "walrus", "--json"]);
    assert!(found.status.success(), "{}", stderr(&found));
    let value: Value = serde_json::from_slice(&found.stdout).unwrap();
    let address = format!("dejavu://claude/{id}");
    assert_eq!(value["hits"][0]["address"], json!(address));

    let shown = fixture.run(&["show", &address, "--json"]);
    assert!(shown.status.success(), "{}", stderr(&shown));
    let value: Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(value["messages"][0]["text"], json!("walrus opening"));

    // The part after the id names a transcript stored under the session.
    let searched = fixture.run(&["search", "narwhal", "--json"]);
    let value: Value = serde_json::from_slice(&searched.stdout).unwrap();
    let part = format!("{address}/subagents/agent-1");
    assert_eq!(value["matches"][0]["address"], json!(part));
    let shown = fixture.run(&["show", &part, "--json"]);
    assert!(shown.status.success(), "{}", stderr(&shown));
    let value: Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(value["messages"][0]["text"], json!("narwhal subagent"));

    // #event-<n> starts the transcript at that event.
    let all = fixture.run(&["transcript", &address, "--json"]);
    let all: Value = serde_json::from_slice(&all.stdout).unwrap();
    let from = fixture.run(&["transcript", &format!("{address}#event-1"), "--json"]);
    assert!(from.status.success(), "{}", stderr(&from));
    let from: Value = serde_json::from_slice(&from.stdout).unwrap();
    assert_eq!(all["events"].as_array().unwrap().len(), 2);
    assert_eq!(from["events"].as_array().unwrap().len(), 1);
    assert!(from["events"][0].to_string().contains("walrus follow-up"));

    let last = fixture.run(&["last", &address]);
    assert!(last.status.success(), "{}", stderr(&last));
    assert!(String::from_utf8_lossy(&last.stdout).contains(&format!("Address: {address}")));

    // The harness is part of the address: the same id under another one is not found.
    let other = fixture.run(&["show", &format!("dejavu://codex/{id}")]);
    assert!(!other.status.success());
    assert!(
        stderr(&other).contains("no codex transcript found"),
        "{}",
        stderr(&other)
    );

    let malformed = fixture.run(&["show", "dejavu://cursor/abc"]);
    assert!(!malformed.status.success());
    assert!(
        stderr(&malformed).contains("unknown harness cursor"),
        "{}",
        stderr(&malformed)
    );
}
