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

#[cfg(windows)]
#[test]
fn transcript_accepts_short_names_under_a_long_configured_root() {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetShortPathNameW(long: *const u16, short: *mut u16, size: u32) -> u32;
    }

    let mut fixture = Fixture(std::env::temp_dir().join(format!(
        "dejavu-stderr-color-short-names-{}",
        std::process::id()
    )));
    let directory = fixture.0.join("projects/demo");
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("fixture.jsonl");
    std::fs::write(
        &path,
        "{\"type\":\"user\",\"uuid\":\"m1\",\"message\":{\"role\":\"user\",\"content\":\"hello needle\"}}\n",
    )
    .unwrap();

    let wide: Vec<u16> = fixture.0.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: wide is NUL-terminated; the first call only queries the buffer size.
    let size = unsafe { GetShortPathNameW(wide.as_ptr(), std::ptr::null_mut(), 0) };
    assert!(size > 0, "{}", std::io::Error::last_os_error());
    let mut short = vec![0u16; size as usize];
    // SAFETY: short has the capacity reported by GetShortPathNameW.
    let len = unsafe { GetShortPathNameW(wide.as_ptr(), short.as_mut_ptr(), size) };
    assert!(len > 0 && len < size, "{}", std::io::Error::last_os_error());
    let short_root = PathBuf::from(std::ffi::OsString::from_wide(&short[..len as usize]));
    let long_root = std::fs::canonicalize(&fixture.0).unwrap();
    let short_path = short_root.join("projects/demo").join("fixture.jsonl");
    let long_path = long_root
        .join("projects")
        .join("demo")
        .join("fixture.jsonl");
    eprintln!(
        "short root: {}; long root: {}",
        short_root.display(),
        long_root.display()
    );
    // Check both alias directions and retain the caller's spelling in JSON output.
    for (root, path) in [(long_root, short_path), (short_root, long_path)] {
        fixture.0 = root;
        let path = path.to_str().unwrap();
        let args = ["transcript", path, "--json"];
        let output = run(&args, &fixture);
        assert!(
            output.status.success(),
            "{args:?}: {}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        let view: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(view["source"], "claude");
        assert_eq!(view["path"], path);
        assert!(String::from_utf8_lossy(&output.stdout).contains("hello needle"));
    }
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
        assert!(
            output.status.success(),
            "{args:?}: {}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
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
        assert!(
            output.status.success(),
            "{args:?}: {}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
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
