use std::path::PathBuf;
use std::process::{Command, Output};

struct Fixture(PathBuf);

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(args: &[&str], fixture: &Fixture) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dejavu"))
        .env("DEJAVU_NO_UPDATE_CHECK", "1")
        .env_remove("NO_COLOR")
        .env("CLAUDE_CONFIG_DIR", &fixture.0)
        .env("OPENCODE_DB", fixture.0.join("broken.db"))
        .args(args)
        .output()
        .unwrap()
}

fn plain_stderr(output: &Output, expected: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(expected), "{stderr}");
    assert!(!stderr.contains('\x1b'), "{stderr:?}");
}

#[test]
fn piped_diagnostics_are_plain_even_when_stdout_color_is_forced() {
    let fixture =
        Fixture(std::env::temp_dir().join(format!("dejavu-stderr-color-{}", std::process::id())));
    let directory = fixture.0.join("projects/demo");
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("fixture.jsonl");
    std::fs::write(
        &path,
        "{\"type\":\"user\",\"uuid\":\"m1\",\"message\":{\"role\":\"user\",\"content\":\"hello needle\"}}\n",
    )
    .unwrap();
    std::fs::write(fixture.0.join("broken.db"), "not a database").unwrap();
    let path = path.to_str().unwrap();

    for args in [
        vec!["needle", "--source", "claude", "--no-index", "--color"],
        vec![
            "find",
            "needle",
            "--source",
            "claude",
            "--no-index",
            "--color",
        ],
        vec!["transcript", path, "--color"],
    ] {
        let output = run(&args, &fixture);
        assert!(output.status.success(), "{args:?}");
        plain_stderr(&output, "claude");
        assert!(output.stdout.contains(&0x1b), "{args:?}");

        let args: Vec<_> = args
            .iter()
            .map(|arg| {
                if *arg == "--color" {
                    "--no-color"
                } else {
                    *arg
                }
            })
            .collect();
        let output = run(&args, &fixture);
        assert!(output.status.success());
        plain_stderr(&output, "claude");
        assert!(!output.stdout.contains(&0x1b));
    }

    let output = run(&["show", path], &fixture);
    assert!(output.status.success());
    plain_stderr(&output, "1 message");

    let output = run(
        &["needle", "--source", "opencode", "--no-index", "--color"],
        &fixture,
    );
    assert!(output.status.success());
    plain_stderr(&output, "skipped unreadable opencode store");

    for args in [
        vec!["find", "--color"],
        vec!["find", "--no-color", "--limit=0"],
        vec!["--unknown"],
        vec!["transcript", path, "--full", "--max-chars=1", "--no-color"],
    ] {
        let output = run(&args, &fixture);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        plain_stderr(&output, "✗ ");
    }
}
