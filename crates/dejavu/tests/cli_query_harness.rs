//! All model invocations use synthetic transcripts and fake PATH executables.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
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
        let fixture = Self(std::env::temp_dir().join(format!(
            "dejavu-cli-harness-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )));
        for sub in [
            "bin",
            "config/dejavu",
            ".config/dejavu",
            "claude/projects/test",
        ] {
            std::fs::create_dir_all(fixture.0.join(sub)).unwrap();
        }
        std::fs::write(fixture.locator(), "{\"type\":\"user\",\"uuid\":\"u1\",\"message\":{\"role\":\"user\",\"content\":\"We chose SQLite.\"}}\n").unwrap();
        let fake = r#"#!/bin/sh
base=$FAKE_ROOT
printf '%s\n' "$@" >> "$base/argv"
case "$(basename "$0")" in
  codex)
    prev=
    for arg in "$@"; do
      [ "$prev" = --output-last-message ] && out=$arg
      prev=$arg
    done
    cat > "$base/prompt"
    printf '%s' "$FAKE_ANSWER" > "$out"
    echo '{"type":"turn.completed","usage":{"input_tokens":90,"output_tokens":9}}'
    ;;
  ruddr)
    case "$1" in
      run) cat > "$base/prompt" ;;
      status) echo '{"status":"completed","tokenUsage":{"inputTokens":90,"outputTokens":9}}' ;;
      result) printf '%s' "$FAKE_ANSWER" ;;
      *) exit 8 ;;
    esac
    ;;
  pi)
    cat > "$base/prompt"
    printf '%s' "$FAKE_ANSWER"
    ;;
  *) exit 9 ;;
esac
"#;
        for name in ["codex", "ruddr", "pi"] {
            let path = fixture.0.join("bin").join(name);
            std::fs::write(&path, fake).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        fixture
    }

    fn locator(&self) -> PathBuf {
        self.0.join("claude/projects/test/session.jsonl")
    }

    fn command(&self, profile: bool) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dejavu"));
        command
            .env("HOME", &self.0)
            .env("USERPROFILE", &self.0)
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .env("XDG_CACHE_HOME", self.0.join("cache"))
            .env("DEJAVU_NO_UPDATE_CHECK", "1")
            .env("CLAUDE_CONFIG_DIR", self.0.join("claude"))
            .env("FAKE_ROOT", &self.0)
            .env(
                "FAKE_ANSWER",
                if profile {
                    r#"{"observations":[]}"#
                } else {
                    "SQLite."
                },
            )
            // Exclude the inherited PATH entirely so no installed model CLI can run.
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.0.join("bin").display()),
            )
            .env_remove("DEJAVU_QUERY_HARNESS")
            .env_remove("DEJAVU_QUERY_MODEL")
            .env_remove("DEJAVU_QUERY_EFFORT")
            .env_remove("DEJAVU_QUERY_VIA_PI")
            .arg(if profile { "profile" } else { "query" })
            .arg(self.locator());
        if profile {
            command.arg("--explain");
        } else {
            command.arg("Which database?");
        }
        command
            .arg("--json")
            .arg("--agent-dir")
            .arg(self.0.join("pi"));
        command
    }

    fn config(&self, json: &str) {
        std::fs::write(self.0.join("config/dejavu/config.json"), json).unwrap();
    }
    fn args(&self) -> String {
        std::fs::read_to_string(self.0.join("argv")).unwrap()
    }
}

fn json(output: Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn query_and_profile_share_flag_env_file_resolution() {
    for profile in [false, true] {
        let f = Fixture::new();
        f.config(r#"{"query":{"harness":"ruddr","model":"claude/file-model","effort":"low"}}"#);
        let result = json(f.command(profile).output().unwrap());
        if profile {
            assert_eq!(result["explanation"]["model"], "claude/file-model");
            assert_eq!(result["explanation"]["reasoningEffort"], "low");
        } else {
            assert_eq!(result["model"]["provider"], "claude");
            assert_eq!(result["model"]["id"], "file-model");
            assert_eq!(result["transport"], "ruddr");
            assert_eq!(result["usage"]["inputTokens"], 90);
            assert!(result.get("costUsd").is_none());
        }
        assert!(f.args().contains("--effort\nlow\n"));
        let result = json(
            f.command(profile)
                .env("DEJAVU_QUERY_HARNESS", "codex")
                .env("DEJAVU_QUERY_MODEL", "env-model")
                .env("DEJAVU_QUERY_EFFORT", "high")
                .output()
                .unwrap(),
        );
        let actual = if profile {
            &result["explanation"]
        } else {
            &result["model"]
        };
        assert_eq!(actual[if profile { "model" } else { "id" }], "env-model");
        assert_eq!(actual["reasoningEffort"], "high");
        assert!(f.args().contains("model_reasoning_effort=\"high\""));
        let result = json(
            f.command(profile)
                .env("DEJAVU_QUERY_HARNESS", "codex")
                .env("DEJAVU_QUERY_MODEL", "env-model")
                .env("DEJAVU_QUERY_EFFORT", "high")
                .args([
                    "--harness",
                    "ruddr",
                    "--model",
                    "pi/flag-model",
                    "--effort",
                    "medium",
                ])
                .output()
                .unwrap(),
        );
        let actual = if profile {
            &result["explanation"]
        } else {
            &result["model"]
        };
        assert_eq!(
            actual[if profile { "model" } else { "id" }],
            if profile {
                "pi/flag-model"
            } else {
                "flag-model"
            }
        );
        assert_eq!(actual["reasoningEffort"], "medium");
    }
}

#[test]
fn defaults_and_home_config_fallback_work_without_real_home_access() {
    for profile in [false, true] {
        let f = Fixture::new();
        let result = json(f.command(profile).output().unwrap());
        let actual = if profile {
            &result["explanation"]
        } else {
            &result["model"]
        };
        assert_eq!(actual[if profile { "model" } else { "id" }], "gpt-6-luna");
        assert_eq!(actual["reasoningEffort"], "medium");
        assert!(
            f.args()
                .starts_with("exec\n--ignore-user-config\n--ephemeral\n")
        );
        std::fs::write(
            f.0.join(".config/dejavu/config.json"),
            r#"{"query":{"model":"home-model"}}"#,
        )
        .unwrap();
        let result = json(
            f.command(profile)
                .env_remove("XDG_CONFIG_HOME")
                .output()
                .unwrap(),
        );
        let actual = if profile {
            &result["explanation"]
        } else {
            &result["model"]
        };
        assert_eq!(actual[if profile { "model" } else { "id" }], "home-model");
    }
}

#[test]
fn malformed_config_is_reported_before_a_model_runs() {
    for profile in [false, true] {
        let f = Fixture::new();
        f.config("{broken");
        let output = f.command(profile).output().unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("invalid dejavu config"));
        assert!(!f.0.join("argv").exists());
    }
}

#[test]
fn legacy_pi_routing_still_works_and_ruddr_overrides_it() {
    let f = Fixture::new();
    let legacy = json(
        f.command(false)
            .args(["--model", "example/test"])
            .env("DEJAVU_QUERY_VIA_PI", "1")
            .output()
            .unwrap(),
    );
    assert_eq!(legacy["transport"], "pi");
    assert!(f.args().contains("--no-tools"));
    let ruddr = json(
        f.command(false)
            .args(["--harness", "ruddr", "--model", "pi/test"])
            .env("DEJAVU_QUERY_VIA_PI", "1")
            .output()
            .unwrap(),
    );
    assert_eq!(ruddr["transport"], "ruddr");
}

#[test]
fn profile_model_failure_preserves_measurements() {
    let f = Fixture::new();
    let output = f
        .command(true)
        .args(["--harness", "ruddr", "--model", "unknown/test"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["sessions"].as_array().unwrap().len(), 1);
    assert!(
        report["explanation"]["error"]
            .as_str()
            .unwrap()
            .contains("unsupported ruddr provider")
    );
    assert!(!f.0.join("argv").exists());
}
