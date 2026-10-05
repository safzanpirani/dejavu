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
            // agy reads no variable for its store, so isolate it through HOME.
            .env("HOME", &self.0)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let value: Value = serde_json::from_slice(&out.stdout).unwrap();
        if value["truncated"] == true {
            assert!(String::from_utf8_lossy(&out.stderr).contains("40 eligible candidates"));
        }
        value
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

fn codex(text: &str, date: Option<&str>) -> String {
    let mut row = json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":text}]}});
    if let Some(date) = date {
        row["timestamp"] = json!(date);
    }
    row.to_string()
}
fn write_codex(fixture: &Fixture, name: &str, project: &str, messages: &[String]) {
    let directory = fixture.0.join("codex/sessions");
    std::fs::create_dir_all(&directory).unwrap();
    let header = json!({"type":"session_meta","payload":{"cwd":project}});
    std::fs::write(
        directory.join(name),
        format!("{header}\n{}\n", messages.join("\n")),
    )
    .unwrap();
}

#[test]
fn source_project_and_activity_filters_precede_every_retrieval_cap() {
    let fixture = Fixture::new();
    for i in 0..820 {
        write_codex(
            &fixture,
            &format!("other-{i}.jsonl"),
            "/work/other",
            &[codex("deploy deploy deploy", Some("2026-09-22"))],
        );
    }
    for i in 0..45 {
        // Recent unrelated activity must not make an old dated match pass --since.
        write_codex(
            &fixture,
            &format!("old-{i}.jsonl"),
            "/work/shipwatch",
            &[
                codex("deploy deploy", Some("2026-09-01")),
                codex("unrelated activity", Some("2026-09-22")),
            ],
        );
    }
    write_codex(
        &fixture,
        "rollout-2026-08-01T00-00-00-target.jsonl",
        "/work/shipwatch",
        &[codex("deploy now", Some("2026-09-22"))],
    );
    for direct in [false, true] {
        let mut args = vec![
            "find",
            "deploy",
            "--source",
            "codex",
            "--project",
            "shipwatch",
            "--since",
            "2026-09-20",
            "--json",
        ];
        if direct {
            args.push("--no-index");
        }
        let result = fixture.run(&args);
        assert_eq!(result["hits"].as_array().unwrap().len(), 1);
        assert!(
            result["hits"][0]["path"]
                .as_str()
                .unwrap()
                .ends_with("target.jsonl")
        );
        assert_eq!(result["truncated"], false);
    }
    let capped = fixture.run(&["find", "deploy", "--source", "codex", "--json", "-n", "100"]);
    assert_eq!(capped["truncated"], true);
    assert_eq!(capped["hits"].as_array().unwrap().len(), 40);
}

#[test]
fn undated_matches_use_session_activity_in_indexed_and_direct_find() {
    let fixture = Fixture::new();
    write_codex(
        &fixture,
        "rollout-2026-08-01T00-00-00-undated.jsonl",
        "/work/demo",
        &[
            codex("needle without a timestamp", None),
            codex("later activity", Some("2026-09-22")),
        ],
    );
    for direct in [false, true] {
        let mut args = vec![
            "find",
            "needle",
            "--source",
            "codex",
            "--since",
            "2026-09-20",
            "--json",
        ];
        if direct {
            args.push("--no-index");
        }
        let result = fixture.run(&args);
        assert_eq!(result["hits"][0]["date"], "2026-09-22");
    }
}

#[test]
fn mixed_terms_check_short_terms_before_capping_long_term_candidates() {
    let fixture = Fixture::new();
    for i in 0..60 {
        write_codex(
            &fixture,
            &format!("long-{i}.jsonl"),
            "/work/demo",
            &[codex(
                "deployment deployment deployment",
                Some("2026-09-22"),
            )],
        );
    }
    write_codex(
        &fixture,
        "both.jsonl",
        "/work/demo",
        &[codex(
            "deployment with rg and ÉX and 😀a",
            Some("2026-09-22"),
        )],
    );
    write_codex(
        &fixture,
        "short-only.jsonl",
        "/work/demo",
        &[codex("rg rg rg", Some("2026-09-22"))],
    );
    for direct in [false, true] {
        for short in ["rg", "éx", "😀a"] {
            let mut args = vec!["find", "deployment", short, "--source", "codex", "--json"];
            if direct {
                args.push("--no-index");
            }
            let result = fixture.run(&args);
            assert_eq!(result["requiredTerms"], json!(["deployment", short]));
            assert_eq!(result["hits"].as_array().unwrap().len(), 1);
            assert!(
                result["hits"][0]["path"]
                    .as_str()
                    .unwrap()
                    .ends_with("both.jsonl")
            );
        }
    }
    let short = fixture.run(&["find", "rg", "--source", "codex", "--json"]);
    assert_eq!(short["hits"].as_array().unwrap().len(), 2);
}
