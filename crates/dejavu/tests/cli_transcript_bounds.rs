//! `test/cli-transcript-bounds.test.ts`: `show` and `transcript` bounds through the built binary.

use serde_json::{Value, json};
use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dejavu"))
        .env("DEJAVU_NO_UPDATE_CHECK", "1")
        .args(args)
        .output()
        .unwrap()
}

fn stdout_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn removes_tool_only_turns_and_applies_explicit_transcript_json_bounds() {
    let root = std::env::temp_dir().join(format!("dejavu-bounds-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let directory = root.join(".claude/projects/demo");
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("fixture.jsonl");
    let messages = [
        json!({ "role": "user", "content": "hello needle" }),
        json!({ "role": "assistant", "content": [{ "type": "tool_use", "id": "t1", "name": "shell", "input": { "command": "echo synthetic" } }] }),
        json!({ "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "t1", "content": "synthetic result" }] }),
        json!({ "role": "assistant", "content": [{ "type": "text", "text": "answer" }, { "type": "tool_use", "id": "t2", "name": "shell", "input": {} }] }),
        json!({ "role": "assistant", "content": "done" }),
    ];
    let lines: Vec<String> = messages
        .iter()
        .enumerate()
        .map(|(index, message)| {
            let parent = if index == 0 { Value::Null } else { json!(format!("m{}", index - 1)) };
            json!({ "type": message["role"], "uuid": format!("m{index}"), "parentUuid": parent, "message": message }).to_string()
        })
        .collect();
    std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
    let path = path.to_str().unwrap();

    let clean = run(&[
        "show",
        path,
        "--no-toolcalls",
        "--around",
        "needle",
        "--json",
    ]);
    assert!(clean.status.success());
    assert_eq!(
        stdout_json(&clean)["messages"],
        json!([{ "role": "user", "text": "hello needle" }, { "role": "assistant", "text": "answer" }, { "role": "assistant", "text": "done" }])
    );

    let page = run(&[
        "transcript",
        path,
        "--no-tools",
        "--from-event",
        "1",
        "--limit",
        "1",
        "--max-chars",
        "3",
        "--json",
    ]);
    assert!(page.status.success());
    let page = stdout_json(&page);
    assert_eq!(page["events"].as_array().unwrap().len(), 1);
    assert_eq!(page["events"][0]["kind"], "assistant");
    assert_eq!(page["events"][0]["index"], 3);
    assert_eq!(page["events"][0]["text"], "an…");
    assert_eq!(page["window"]["nextEvent"], 5);
    assert_eq!(page["window"]["usedChars"], 3);

    let full = run(&[
        "transcript",
        path,
        "--full",
        "--from-event",
        "3",
        "--limit",
        "1",
        "--json",
    ]);
    assert_eq!(stdout_json(&full)["events"][0]["text"], "answer");

    let separated = run(&["transcript", "--json", "--", path]);
    assert!(separated.status.success());
    assert!(
        !stdout_json(&separated)["events"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let dashed = run(&["show", "--json", "--", "--not-a-flag.jsonl"]);
    assert!(!String::from_utf8_lossy(&dashed.stderr).contains("unknown flag"));

    let cap = run(&["show", path, "--max-chars", "3", "--json"]);
    assert_eq!(stdout_json(&cap)["messages"][0]["text"], "hel [...]");

    for flags in [
        &["--budget-chars=0"][..],
        &["--max-chars="],
        &["--from-event=-1"],
        &["--full", "--tool-chars=3"],
    ] {
        let mut args = vec!["transcript", path];
        args.extend_from_slice(flags);
        let invalid = run(&args);
        assert_eq!(invalid.status.code(), Some(1), "{flags:?}");
        assert!(invalid.stdout.is_empty(), "{flags:?}");
    }

    let text = run(&["transcript", path, "--no-color", "--max-chars", "3"]);
    let text = String::from_utf8_lossy(&text.stdout);
    assert!(
        text.ends_with(
            "; clipped events #0, #3, #5 (read each with --full --from-event N --limit 1)]\n"
        ),
        "{text}"
    );
    let _ = std::fs::remove_dir_all(&root);
}
