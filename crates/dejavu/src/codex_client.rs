//! One ephemeral completion through `codex exec`. A port of `codex-client.ts`.
//!
//! Also holds what the query transports share: [`Cancel`], the SIGINT/SIGTERM
//! [`SignalGuard`], and [`run_child`], a bounded subprocess runner.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde::Serialize;

pub const DEFAULT_CODEX_QUERY_MODEL: &str = "gpt-6-luna";
pub const CODEX_QUERY_REASONING: &str = "medium";
const TIMEOUT: Duration = Duration::from_millis(120_000);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// A model answer. `transport` is `"codex"`, `"ruddr"`, `"http"`, or `"pi"`; a missing
/// `usage` serializes as an absent key, as in the TypeScript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Completion {
    pub answer: String,
    pub transport: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<QueryUsage>,
}

static SIGNALLED: AtomicBool = AtomicBool::new(false);

/// A cancellation flag, the `AbortSignal` of the TypeScript. A `Cancel` made by
/// [`SignalGuard::cancel`] also trips on SIGINT or SIGTERM while the guard lives.
#[derive(Debug, Clone, Default)]
pub struct Cancel {
    flag: Arc<AtomicBool>,
    signals: bool,
}

impl Cancel {
    pub fn new() -> Cancel {
        Cancel::default()
    }

    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst) || (self.signals && SIGNALLED.load(Ordering::SeqCst))
    }
}

/// Turns SIGINT and SIGTERM into cancellation until dropped, as `cli.ts` did with
/// `process.once("SIGINT", cancel)` around `query` and `profile --explain`.
/// The default handlers come back on drop. On Windows the console delivers
/// Ctrl+C to the child as well, so the guard installs nothing there.
pub struct SignalGuard(());

impl SignalGuard {
    pub fn install() -> SignalGuard {
        SIGNALLED.store(false, Ordering::SeqCst);
        #[cfg(unix)]
        sys::set_handlers(true);
        SignalGuard(())
    }

    pub fn cancel(&self) -> Cancel {
        Cancel {
            flag: Arc::default(),
            signals: true,
        }
    }
}

impl Drop for SignalGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        sys::set_handlers(false);
    }
}

#[cfg(unix)]
mod sys {
    use std::os::raw::c_int;
    use std::sync::atomic::Ordering;

    const SIGINT: c_int = 2;
    const SIGTERM: c_int = 15;
    const SIGKILL: c_int = 9;
    const SIG_DFL: usize = 0;

    unsafe extern "C" {
        fn signal(signum: c_int, handler: usize) -> usize;
        fn kill(pid: c_int, sig: c_int) -> c_int;
    }

    extern "C" fn on_signal(_: c_int) {
        super::SIGNALLED.store(true, Ordering::SeqCst);
    }

    pub fn set_handlers(install: bool) {
        let handler = if install {
            on_signal as extern "C" fn(c_int) as usize
        } else {
            SIG_DFL
        };
        // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
        unsafe {
            signal(SIGINT, handler);
            signal(SIGTERM, handler);
        }
    }

    /// SIGKILL to a whole process group.
    pub fn kill_group(pid: u32) {
        // SAFETY: kill(2) has no memory-safety preconditions.
        unsafe {
            kill(-(pid as c_int), SIGKILL);
        }
    }
}

/// What a bounded child run produced.
#[derive(Debug)]
pub struct ChildRun {
    pub status: Option<ExitStatus>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
    pub cancelled: bool,
}

/// `exitCode` as Bun reported it: the code, or 128 + the signal number.
pub fn exit_code(status: Option<ExitStatus>) -> i32 {
    let Some(status) = status else { return -1 };
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    status.code().unwrap_or(-1)
}

/// Runs `command` with `input` on stdin, collecting stdout and stderr, until it
/// exits, `timeout` passes, or `cancel` trips. With `own_group` the child leads
/// a new process group (Unix), and a stop kills that whole group; only this
/// invocation's group is ours to kill. Returns the spawn error unchanged.
pub fn run_child(
    mut command: Command,
    input: Vec<u8>,
    timeout: Option<Duration>,
    cancel: &Cancel,
    own_group: bool,
) -> std::io::Result<ChildRun> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    if own_group {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn()?;
    let mut stdin = child.stdin.take();
    std::thread::spawn(move || {
        if let Some(stdin) = stdin.as_mut() {
            // A child that exits without reading closes the pipe; that is not an error here.
            let _ = stdin.write_all(&input);
        }
    });
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let deadline = timeout.map(|limit| Instant::now() + limit);
    let mut status = None;
    let (mut out, mut err) = (None, None);
    let (mut timed_out, mut cancelled) = (false, false);
    loop {
        if status.is_none() {
            status = child.try_wait()?;
        }
        out = out.or_else(|| stdout.try_recv().ok());
        err = err.or_else(|| stderr.try_recv().ok());
        if status.is_some() && out.is_some() && err.is_some() {
            break;
        }
        if cancel.is_cancelled() {
            cancelled = true;
        } else if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            timed_out = true;
        }
        if cancelled || timed_out {
            stop(&mut child, own_group);
            // Grandchildren outside the group may still hold the pipes; do not wait on them.
            status = status.or(child.wait().ok());
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(ChildRun {
        status,
        stdout: out.unwrap_or_default(),
        stderr: err.unwrap_or_default(),
        timed_out,
        cancelled,
    })
}

fn drain(pipe: Option<impl Read + Send + 'static>) -> mpsc::Receiver<Vec<u8>> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes);
        }
        let _ = sender.send(bytes);
    });
    receiver
}

fn stop(child: &mut Child, own_group: bool) {
    #[cfg(unix)]
    if own_group {
        sys::kill_group(child.id());
    }
    let _ = own_group;
    // The process may already have exited.
    let _ = child.kill();
}

/// A private temporary directory, removed on drop (`mkdtemp`).
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(prefix: &str) -> std::io::Result<TempDir> {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let base = std::env::temp_dir();
        loop {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.subsec_nanos());
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = base.join(format!(
                "{prefix}{}-{nanos:x}{unique:x}",
                std::process::id()
            ));
            #[cfg_attr(not(unix), allow(unused_mut))]
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&path) {
                Ok(()) => return Ok(TempDir(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Test seams for [`complete_via_codex_with`].
#[derive(Debug, Clone, Default)]
pub struct CodexDeps {
    /// Replaces `codex` (the program and any leading arguments).
    pub command: Option<Vec<String>>,
    pub timeout: Option<Duration>,
    pub effort: Option<String>,
}

/// A single ephemeral completion, shared by `query` and `profile --explain`.
pub fn complete_via_codex(
    model: &str,
    effort: Option<&str>,
    prompt: &str,
    cancel: &Cancel,
) -> Result<Completion, String> {
    complete_via_codex_with(
        model,
        prompt,
        cancel,
        &CodexDeps {
            effort: effort.map(str::to_string),
            ..CodexDeps::default()
        },
    )
}

pub fn complete_via_codex_with(
    model: &str,
    prompt: &str,
    cancel: &Cancel,
    deps: &CodexDeps,
) -> Result<Completion, String> {
    let cancelled = || "query was cancelled".to_string();
    if cancel.is_cancelled() {
        return Err(cancelled());
    }
    let directory = TempDir::new("dejavu-query-").map_err(|error| error.to_string())?;
    let output = directory.path().join("answer.txt");
    let timeout = deps.timeout.unwrap_or(TIMEOUT);
    let base = deps.command.clone().unwrap_or_else(|| vec!["codex".into()]);
    let mut command = Command::new(&base[0]);
    command
        .args(&base[1..])
        .current_dir(directory.path())
        .args(codex_args(model, &output, deps.effort.as_deref()));
    let run = run_child(
        command,
        prompt.as_bytes().to_vec(),
        Some(timeout),
        cancel,
        true,
    )
    .map_err(|_| {
        "could not start codex exec; install Codex and make sure codex is on PATH".to_string()
    })?;
    if run.cancelled || cancel.is_cancelled() {
        return Err(cancelled());
    }
    if run.timed_out {
        return Err(format!(
            "query model timed out after {}ms",
            timeout.as_millis()
        ));
    }
    let mut usage = None;
    let mut failed = None;
    let mut last_error = None;
    let mut completed = false;
    for line in String::from_utf8_lossy(&run.stdout).split('\n') {
        let Ok(serde_json::Value::Object(event)) = serde_json::from_str::<serde_json::Value>(line)
        else {
            continue;
        };
        let kind = event.get("type").and_then(|kind| kind.as_str());
        // Event messages come from the model API, unlike stderr, which may log the prompt.
        match kind {
            Some("turn.failed") => {
                failed = Some(
                    event_message(event.get("error").and_then(|error| error.get("message")))
                        .unwrap_or_else(|| "turn failed".into()),
                )
            }
            Some("error") => {
                last_error =
                    Some(event_message(event.get("message")).unwrap_or_else(|| "error".into()))
            }
            Some("turn.completed") => {
                completed = true;
                let tokens = |key: &str| {
                    event
                        .get("usage")
                        .and_then(|usage| usage.get(key))
                        .and_then(|value| value.as_f64())
                };
                if let (Some(input), Some(output)) =
                    (tokens("input_tokens"), tokens("output_tokens"))
                {
                    usage = Some(QueryUsage {
                        input_tokens: input as u64,
                        output_tokens: output as u64,
                    });
                }
            }
            _ => {}
        }
    }
    // Codex reports retries as error events, so one only fails a turn that never completed.
    let failure = failed.or(if completed { None } else { last_error });
    if let Some(failure) = failure {
        return Err(format!(
            "codex query failed: {failure}; check codex login status and access to {model}"
        ));
    }
    let code = exit_code(run.status);
    if code != 0 {
        return Err(format!(
            "codex exec failed (exit {code}); check codex login status and access to {model}"
        ));
    }
    let answer = match std::fs::read(&output) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.to_string()),
    };
    let answer = js_trim(&answer);
    if answer.is_empty() {
        return Err("query model returned an empty response".into());
    }
    Ok(Completion {
        answer: answer.to_string(),
        transport: "codex",
        usage,
    })
}

/// Shared Codex lockdown settings, in the original exec argument order.
pub fn codex_config(effort: Option<&str>) -> Vec<String> {
    vec![
        "model_provider=\"openai\"".into(),
        format!(
            "model_reasoning_effort={}",
            serde_json::to_string(effort.unwrap_or(CODEX_QUERY_REASONING)).unwrap()
        ),
        "approval_policy=\"never\"".into(),
        "project_doc_max_bytes=0".into(),
        "skills.include_instructions=false".into(),
        "skills.bundled.enabled=false".into(),
        "features.shell_tool=false".into(),
        "features.apps=false".into(),
        "features.multi_agent=false".into(),
        "features.skill_search=false".into(),
        "web_search=\"disabled\"".into(),
    ]
}

fn codex_args(model: &str, output: &Path, effort: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = [
        "exec",
        "--ignore-user-config",
        "--ephemeral",
        "--skip-git-repo-check",
        "--sandbox",
        "read-only",
        "--model",
        model,
        "--color",
        "never",
        "--json",
        "--output-last-message",
        &output.to_string_lossy(),
    ]
    .iter()
    .map(|arg| arg.to_string())
    .collect();
    for setting in codex_config(effort) {
        args.extend(["-c".into(), setting]);
    }
    args.push("-".into());
    args
}

fn event_message(value: Option<&serde_json::Value>) -> Option<String> {
    let text = js_trim(value?.as_str()?);
    (!text.is_empty()).then(|| crate::js::prefix(text, 300).to_string())
}

/// `String.prototype.trim`: Unicode whitespace plus the byte-order mark.
pub fn js_trim(text: &str) -> &str {
    text.trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{feff}')
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A fake `codex` as a shell script: `$1` is the mode, the rest are the real arguments.
    const FAKE: &str = r#"#!/bin/sh
mode=$1; shift
out=
prev=
for arg in "$@"; do
  [ "$prev" = "--output-last-message" ] && out=$arg
  prev=$arg
done
prompt=$(cat)
case $mode in
  slow) sleep 10; exit 0 ;;
  fail) echo private-transcript-sentinel >&2; exit 3 ;;
  missing) exit 0 ;;
  event-fail)
    printf 'partial answer' > "$out"
    echo '{"type":"turn.failed","error":{"message":"unauthorized (401)"}}'
    exit 0 ;;
  auth-fail)
    echo private-transcript-sentinel >&2
    echo '{"type":"error","message":"Reconnecting... 1/5 (unauthorized (401))"}'
    echo '{"type":"error","message":"workspace routing discovery unauthorized (401)"}'
    exit 1 ;;
  retry-ok)
    printf 'recovered' > "$out"
    echo '{"type":"error","message":"Reconnecting... 1/5"}'
    echo '{"type":"turn.completed"}'
    exit 0 ;;
  empty) printf '   ' > "$out" ;;
  *)
    { printf 'PROMPT=%s\n' "$prompt"; printf 'CWD=%s\n' "$(pwd)"; for arg in "$@"; do printf 'ARG=%s\n' "$arg"; done; } > "$out" ;;
esac
echo 'diagnostic noise'
echo 'null'
echo '{"type":"item.completed","item":{"type":"agent_message","text":"not the final output file"}}'
echo '{"type":"turn.completed","usage":{"input_tokens":120,"cached_input_tokens":60,"output_tokens":8}}'
"#;

    fn fixture() -> &'static Path {
        static PATH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        PATH.get_or_init(|| {
            use std::os::unix::fs::PermissionsExt;
            let dir =
                std::env::temp_dir().join(format!("dejavu-codex-test-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("fake-codex.sh");
            std::fs::write(&path, FAKE).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        })
    }

    fn run(mode: &str, cancel: &Cancel, timeout_ms: u64) -> Result<Completion, String> {
        let deps = CodexDeps {
            command: Some(vec![fixture().to_string_lossy().into_owned(), mode.into()]),
            timeout: Some(Duration::from_millis(timeout_ms)),
            effort: None,
        };
        complete_via_codex_with(
            "gpt-6-luna",
            "private transcript and question",
            cancel,
            &deps,
        )
    }

    #[test]
    fn passes_context_through_stdin_reads_only_the_final_answer_reports_usage_and_cleans_up() {
        let result = run("ok", &Cancel::new(), 5000).unwrap();
        assert_eq!(result.transport, "codex");
        assert_eq!(
            result.usage,
            Some(QueryUsage {
                input_tokens: 120,
                output_tokens: 8
            })
        );
        let lines: Vec<&str> = result.answer.lines().collect();
        let args: Vec<&str> = lines
            .iter()
            .filter_map(|line| line.strip_prefix("ARG="))
            .collect();
        assert_eq!(lines[0], "PROMPT=private transcript and question");
        assert!(!args.contains(&"private transcript and question"));
        for expected in [
            "--ephemeral",
            "--ignore-user-config",
            "model_reasoning_effort=\"medium\"",
            "model_provider=\"openai\"",
            "features.shell_tool=false",
        ] {
            assert!(args.contains(&expected), "{expected}");
        }
        let after = |flag: &str| args[args.iter().position(|arg| *arg == flag).unwrap() + 1];
        assert_eq!(after("--model"), "gpt-6-luna");
        assert_eq!(after("--sandbox"), "read-only");
        assert_eq!(*args.last().unwrap(), "-");
        let cwd = lines[1].strip_prefix("CWD=").unwrap();
        assert_ne!(Path::new(cwd), std::env::current_dir().unwrap());
        assert!(!Path::new(cwd).exists());
    }

    #[test]
    fn fails_on_a_nonzero_exit_without_leaking_child_stderr() {
        let error = run("fail", &Cancel::new(), 5000).unwrap_err();
        assert!(error.contains("codex exec failed (exit 3)"), "{error}");
        assert!(!error.contains("sentinel"));
    }

    #[test]
    fn rejects_missing_empty_and_failed_final_answers() {
        assert!(
            run("missing", &Cancel::new(), 5000)
                .unwrap_err()
                .contains("empty response")
        );
        assert!(
            run("empty", &Cancel::new(), 5000)
                .unwrap_err()
                .contains("empty response")
        );
        assert!(
            run("event-fail", &Cancel::new(), 5000)
                .unwrap_err()
                .contains("codex query failed: unauthorized (401)")
        );
    }

    #[test]
    fn reports_the_api_error_behind_a_failed_exit_but_never_child_stderr() {
        let message = run("auth-fail", &Cancel::new(), 5000).unwrap_err();
        assert!(
            message.contains("codex query failed: workspace routing discovery unauthorized (401)"),
            "{message}"
        );
        assert!(!message.contains("private-transcript-sentinel"));
    }

    #[test]
    fn ignores_retry_errors_in_a_turn_that_completed() {
        assert_eq!(
            run("retry-ok", &Cancel::new(), 5000).unwrap().answer,
            "recovered"
        );
    }

    #[test]
    fn bounds_a_stalled_child() {
        let started = Instant::now();
        assert_eq!(
            run("slow", &Cancel::new(), 100).unwrap_err(),
            "query model timed out after 100ms"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn handles_cancellation_before_and_during_a_query() {
        let before = Cancel::new();
        before.cancel();
        assert_eq!(run("ok", &before, 5000).unwrap_err(), "query was cancelled");
        let during = Cancel::new();
        let trigger = during.clone();
        let timer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            trigger.cancel();
        });
        let started = Instant::now();
        assert_eq!(
            run("slow", &during, 5000).unwrap_err(),
            "query was cancelled"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        timer.join().unwrap();
    }

    #[test]
    fn default_argv_is_unchanged_and_effort_only_changes_its_setting() {
        let expected = [
            "exec",
            "--ignore-user-config",
            "--ephemeral",
            "--skip-git-repo-check",
            "--sandbox",
            "read-only",
            "--model",
            "gpt-6-luna",
            "--color",
            "never",
            "--json",
            "--output-last-message",
            "answer.txt",
            "-c",
            "model_provider=\"openai\"",
            "-c",
            "model_reasoning_effort=\"medium\"",
            "-c",
            "approval_policy=\"never\"",
            "-c",
            "project_doc_max_bytes=0",
            "-c",
            "skills.include_instructions=false",
            "-c",
            "skills.bundled.enabled=false",
            "-c",
            "features.shell_tool=false",
            "-c",
            "features.apps=false",
            "-c",
            "features.multi_agent=false",
            "-c",
            "features.skill_search=false",
            "-c",
            "web_search=\"disabled\"",
            "-",
        ];
        assert_eq!(
            codex_args("gpt-6-luna", Path::new("answer.txt"), None),
            expected
        );
        let deps = CodexDeps {
            command: Some(vec![fixture().to_string_lossy().into_owned(), "ok".into()]),
            effort: Some("high".into()),
            ..CodexDeps::default()
        };
        let result =
            complete_via_codex_with("gpt-6-luna", "prompt", &Cancel::new(), &deps).unwrap();
        let mut actual: Vec<_> = result
            .answer
            .lines()
            .filter_map(|line| line.strip_prefix("ARG="))
            .collect();
        actual[12] = "answer.txt";
        let mut expected = expected;
        expected[16] = "model_reasoning_effort=\"high\"";
        assert_eq!(actual, expected);
    }

    #[test]
    fn reports_a_missing_codex() {
        let deps = CodexDeps {
            command: Some(vec!["/nonexistent/codex".into()]),
            timeout: None,
            effort: None,
        };
        let error = complete_via_codex_with("m", "p", &Cancel::new(), &deps).unwrap_err();
        assert_eq!(
            error,
            "could not start codex exec; install Codex and make sure codex is on PATH"
        );
    }

    #[test]
    fn completions_serialize_like_the_typescript() {
        let completion = Completion {
            answer: "a".into(),
            transport: "codex",
            usage: Some(QueryUsage {
                input_tokens: 1,
                output_tokens: 2,
            }),
        };
        assert_eq!(
            serde_json::to_string(&completion).unwrap(),
            r#"{"answer":"a","transport":"codex","usage":{"inputTokens":1,"outputTokens":2}}"#
        );
        let bare = Completion {
            answer: "a".into(),
            transport: "pi",
            usage: None,
        };
        assert_eq!(
            serde_json::to_string(&bare).unwrap(),
            r#"{"answer":"a","transport":"pi"}"#
        );
    }
}
