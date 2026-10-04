//! Exercise the public pack shape and output size with an isolated real CLI.
use serde_json::{Value, json};
use std::process::Command;

struct Cleanup(std::path::PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn pack_keeps_useful_text_and_metadata_with_bounded_event_count() {
    let root = std::env::temp_dir().join(format!("dejavu-pack-budget-{}", std::process::id()));
    let _cleanup = Cleanup(root.clone());
    let claude = root.join("claude");
    let project = claude.join("projects/synthetic");
    std::fs::create_dir_all(&project).unwrap();
    let text = format!("{}NEEDLE{}", "before ".repeat(500), " after".repeat(100));
    let lines: Vec<String> = (0..75)
        .map(|i| {
            let role = if i % 2 == 0 { "user" } else { "assistant" };
            json!({ "type": role, "uuid": format!("m{i}"),
                "parentUuid": if i == 0 { Value::Null } else { json!(format!("m{}", i - 1)) },
                "timestamp": "2026-01-01T00:00:00Z",
                "message": { "role": role, "content": text }
            })
            .to_string()
        })
        .collect();
    std::fs::write(project.join("fixture.jsonl"), lines.join("\n")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_dejavu"))
        .env("DEJAVU_NO_UPDATE_CHECK", "1")
        .env("CLAUDE_CONFIG_DIR", &claude)
        .env("DEJAVU_INDEX_PATH", root.join("index.sqlite"))
        .args([
            "pack",
            "needle",
            "--source",
            "claude",
            "--no-index",
            "--limit",
            "1",
            "--budget-chars",
            "1200",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.len() < 4000, "{} bytes", output.stdout.len());
    assert_eq!(output.stdout.iter().filter(|&&b| b == b'\n').count(), 1);
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    let session = &result["sessions"][0];
    let events = session["events"].as_array().unwrap();
    assert_eq!(events.len(), 3);
    for event in events {
        let text = event["text"].as_str().unwrap();
        assert!(text.contains("NEEDLE"));
        assert!(text.len() >= 200);
        assert!(event["ref"]["line"].is_number());
        assert!(event["index"].is_number());
        assert_eq!(event["timestamp"], "2026-01-01T00:00:00Z");
    }
    assert_eq!(session["window"]["availableEvents"], 75);
    assert!(session["window"]["nextEvent"].is_number());
    assert_eq!(result["omitted"][0]["events"], 72);
    assert_eq!(result["omitted"][0]["neighborhoods"], 72);
    assert!(result["usedChars"].as_u64().unwrap() <= 1200);
}
