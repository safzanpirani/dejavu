//! Optional Ruddr completion with private, automatically removed run directories.

use std::process::Command;
use std::time::{Duration, Instant};

use crate::codex_client::{
    Cancel, Completion, QueryUsage, TempDir, codex_config, exit_code, js_trim, run_child,
};
use crate::model_client::ResolvedQueryModel;

pub fn complete_via_ruddr(
    model: &ResolvedQueryModel,
    prompt: &str,
    cancel: &Cancel,
) -> Result<Completion, String> {
    complete_with(model, prompt, cancel, "ruddr", Duration::from_secs(120))
}

fn complete_with(
    model: &ResolvedQueryModel,
    prompt: &str,
    cancel: &Cancel,
    program: &str,
    timeout: Duration,
) -> Result<Completion, String> {
    if cancel.is_cancelled() {
        return Err("query was cancelled".into());
    }
    let cwd = TempDir::new("dejavu-ruddr-cwd-").map_err(|e| e.to_string())?;
    let state = TempDir::new("dejavu-ruddr-state-").map_err(|e| e.to_string())?;
    let deadline = Instant::now() + timeout;
    let invoke = |args: Vec<String>, input: &str| -> Result<Vec<u8>, String> {
        let mut command = Command::new(program);
        command.args(&args).current_dir(cwd.path());
        let run = run_child(
            command,
            input.as_bytes().to_vec(),
            Some(deadline.saturating_duration_since(Instant::now())),
            cancel,
            true,
        )
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    "could not start ruddr; install Ruddr and make sure ruddr is on PATH, or select --harness codex".into()
                } else {
                    format!("could not start ruddr: {error}")
                }
            })?;
        if run.cancelled || cancel.is_cancelled() {
            return Err("query was cancelled".into());
        }
        if run.timed_out {
            return Err(format!(
                "query model timed out after {}ms",
                timeout.as_millis()
            ));
        }
        let code = exit_code(run.status);
        if code != 0 {
            let stderr = String::from_utf8_lossy(&run.stderr);
            let detail = js_trim(&stderr);
            return Err(format!(
                "ruddr {} failed (exit {code}){}",
                args[0],
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {}", crate::js::prefix(detail, 1000))
                }
            ));
        }
        Ok(run.stdout)
    };
    let mut args: Vec<String> = ["run", "--provider", &model.provider, "--model", &model.id]
        .into_iter()
        .map(str::to_string)
        .collect();
    if let Some(effort) = &model.reasoning_effort {
        args.extend(["--effort".into(), effort.clone()]);
    }
    args.extend(
        [
            "--sandbox",
            "read-only",
            "--ephemeral",
            "--prompt-file",
            "-",
            "--cwd",
        ]
        .into_iter()
        .map(str::to_string),
    );
    args.push(cwd.path().to_string_lossy().into_owned());
    args.extend([
        "--state-dir".into(),
        state.path().to_string_lossy().into_owned(),
    ]);
    if model.provider == "codex" {
        for config in codex_config(model.reasoning_effort.as_deref()) {
            args.extend(["--config".into(), config]);
        }
    }
    invoke(args, prompt)?;
    let state_arg = state.path().to_string_lossy().into_owned();
    let status = invoke(
        vec![
            "status".into(),
            "--state-dir".into(),
            state_arg.clone(),
            "--json".into(),
        ],
        "",
    )?;
    let status: serde_json::Value = serde_json::from_slice(&status)
        .map_err(|error| format!("invalid ruddr status JSON: {error}"))?;
    if status.get("status").and_then(|v| v.as_str()) != Some("completed") {
        return Err(format!(
            "ruddr query did not complete (status: {})",
            status
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("missing")
        ));
    }
    let usage = status.get("tokenUsage").and_then(|usage| {
        Some(QueryUsage {
            input_tokens: usage.get("inputTokens")?.as_u64()?,
            output_tokens: usage.get("outputTokens")?.as_u64()?,
        })
    });
    let answer = invoke(vec!["result".into(), "--state-dir".into(), state_arg], "")?;
    let answer = String::from_utf8_lossy(&answer);
    let answer = js_trim(&answer);
    if answer.is_empty() {
        return Err("query model returned an empty response".into());
    }
    Ok(Completion {
        answer: answer.into(),
        transport: "ruddr",
        usage,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::query_config::{Harness, QuerySettings, resolve_model};
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    const FAKE: &str = r#"#!/bin/sh
base=$(dirname "$0")
if [ -f "$base/delay-start" ]; then sleep 1; fi
command=$1
shift
printf '%s\n' "$command" >> "$base/calls"
printf '%s\n' "$@" > "$base/$command.args"
prev=
for arg in "$@"; do
  [ "$prev" = --cwd ] && cwd=$arg
  [ "$prev" = --state-dir ] && state=$arg
  prev=$arg
done
case "$command" in
  run)
    printf '%s\n' "$cwd" "$state" > "$base/dirs"
    [ -d "$cwd" ] && [ -d "$state" ] && [ -z "$(ls -A "$cwd")" ] || exit 8
    cat > "$state/prompt"
    cp "$state/prompt" "$base/prompt"
    touch "$base/ready"
    case "$(cat "$state/prompt")" in
      fail) echo 'Droid refuses --ephemeral' >&2; exit 2 ;;
      slow) sleep 60 ;;
    esac
    ;;
  status)
    case "$(cat "$state/prompt")" in
      status-fail) echo 'status unavailable' >&2; exit 3 ;;
      bad-json) echo '{'; exit 0 ;;
      failed-turn) echo '{"status":"failed"}'; exit 0 ;;
      no-usage) echo '{"status":"completed"}'; exit 0 ;;
    esac
    echo '{"status":"completed","tokenUsage":{"inputTokens":120,"cachedInputTokens":40,"outputTokens":8}}'
    ;;
  result)
    case "$(cat "$state/prompt")" in
      result-fail) echo 'result unavailable' >&2; exit 4 ;;
      empty) exit 0 ;;
    esac
    printf '  canned answer\n'
    ;;
  *) exit 9 ;;
esac
"#;

    fn fixture() -> (TempDir, String) {
        let dir = TempDir::new("dejavu-fake-ruddr-").unwrap();
        let path = dir.path().join("ruddr");
        std::fs::write(&path, FAKE).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        (dir, path.to_string_lossy().into_owned())
    }

    fn model(id: &str, effort: Option<&str>) -> ResolvedQueryModel {
        resolve_model(
            Path::new("/unused"),
            &QuerySettings {
                harness: Harness::Ruddr,
                model: id.into(),
                effort: effort.map(str::to_string),
            },
        )
        .unwrap()
    }

    fn assert_clean(dir: &TempDir) {
        let dirs = std::fs::read_to_string(dir.path().join("dirs"))
            .expect("fake ruddr must record its run directories before cleanup is checked");
        assert_eq!(dirs.lines().count(), 2);
        for path in dirs.lines() {
            assert!(!Path::new(path).exists(), "leaked {path}");
        }
    }

    #[test]
    fn records_exact_argv_stdin_usage_and_cleanup_for_every_provider() {
        for provider in ["codex", "claude", "pi", "omp", "opencode", "droid"] {
            let (dir, program) = fixture();
            let model = model(&format!("{provider}/test-model"), Some("high"));
            let result = complete_with(
                &model,
                "synthetic prompt",
                &Cancel::new(),
                &program,
                Duration::from_secs(30),
            )
            .unwrap();
            assert_eq!(result.answer, "canned answer");
            assert_eq!(result.transport, "ruddr");
            assert_eq!(
                result.usage,
                Some(QueryUsage {
                    input_tokens: 120,
                    output_tokens: 8
                })
            );
            let dirs = std::fs::read_to_string(dir.path().join("dirs")).unwrap();
            let dirs: Vec<_> = dirs.lines().collect();
            let mut expected: Vec<String> = [
                "--provider",
                provider,
                "--model",
                "test-model",
                "--effort",
                "high",
                "--sandbox",
                "read-only",
                "--ephemeral",
                "--prompt-file",
                "-",
                "--cwd",
                dirs[0],
                "--state-dir",
                dirs[1],
            ]
            .into_iter()
            .map(str::to_string)
            .collect();
            if provider == "codex" {
                for config in codex_config(Some("high")) {
                    expected.extend(["--config".into(), config]);
                }
            }
            assert_eq!(
                std::fs::read_to_string(dir.path().join("run.args"))
                    .unwrap()
                    .lines()
                    .collect::<Vec<_>>(),
                expected
            );
            assert_eq!(
                std::fs::read_to_string(dir.path().join("status.args")).unwrap(),
                format!("--state-dir\n{}\n--json\n", dirs[1])
            );
            assert_eq!(
                std::fs::read_to_string(dir.path().join("result.args")).unwrap(),
                format!("--state-dir\n{}\n", dirs[1])
            );
            assert_eq!(
                std::fs::read_to_string(dir.path().join("calls")).unwrap(),
                "run\nstatus\nresult\n"
            );
            assert_eq!(
                std::fs::read_to_string(dir.path().join("prompt")).unwrap(),
                "synthetic prompt"
            );
            assert_clean(&dir);
        }
    }

    #[test]
    fn cleans_up_on_run_status_result_timeout_and_cancellation_failures() {
        for (prompt, expected) in [
            ("fail", "Droid refuses --ephemeral"),
            ("status-fail", "status unavailable"),
            ("bad-json", "invalid ruddr status JSON"),
            ("failed-turn", "did not complete"),
            ("result-fail", "result unavailable"),
            ("empty", "empty response"),
            ("slow", "timed out"),
        ] {
            let (dir, program) = fixture();
            let timeout = if prompt == "slow" {
                // Exercise startup slower than the old 150ms deadline. Allow the
                // fake to record its directories before timing out its 60s sleep.
                std::fs::write(dir.path().join("delay-start"), "").unwrap();
                Duration::from_secs(5)
            } else {
                Duration::from_secs(30)
            };
            let error = complete_with(
                &model("claude/test", None),
                prompt,
                &Cancel::new(),
                &program,
                timeout,
            )
            .unwrap_err();
            assert!(error.contains(expected), "{error}");
            assert_clean(&dir);
        }
        let (dir, program) = fixture();
        // A fixed 200ms cancellation could kill the shell before it logged dirs.
        std::fs::write(dir.path().join("delay-start"), "").unwrap();
        let cancel = Cancel::new();
        let ready = dir.path().join("ready");
        let (started, result) = std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                complete_with(
                    &model("test", None),
                    "slow",
                    &cancel,
                    &program,
                    Duration::from_secs(30),
                )
            });
            let deadline = Instant::now() + Duration::from_secs(30);
            while !ready.exists() && !worker.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            let started = ready.exists();
            cancel.cancel();
            // Join even on a failed readiness check so no worker outlives the fixture.
            (started, worker.join().unwrap())
        });
        assert!(started, "fake ruddr never became ready: {result:?}");
        assert_eq!(result.unwrap_err(), "query was cancelled");
        assert_clean(&dir);
    }

    #[test]
    fn missing_ruddr_has_an_actionable_error_and_never_falls_back() {
        let dir = TempDir::new("dejavu-missing-ruddr-").unwrap();
        let error = complete_with(
            &model("test", None),
            "prompt",
            &Cancel::new(),
            dir.path().join("missing").to_str().unwrap(),
            Duration::from_secs(30),
        )
        .unwrap_err();
        assert!(error.contains("install Ruddr"));
        assert!(error.contains("PATH"));
        assert!(error.contains("--harness codex"));
    }

    #[test]
    fn missing_usage_is_unknown_and_non_codex_effort_can_be_omitted() {
        let (dir, program) = fixture();
        let result = complete_with(
            &model("claude/test", None),
            "no-usage",
            &Cancel::new(),
            &program,
            Duration::from_secs(30),
        )
        .unwrap();
        assert_eq!(result.usage, None);
        assert!(
            !std::fs::read_to_string(dir.path().join("run.args"))
                .unwrap()
                .contains("--effort")
        );
        assert_clean(&dir);
    }
}
