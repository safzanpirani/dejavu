//! `test/cli-memory.test.ts`: the memory commands through the built binary.

use std::process::Command;

fn dejavu() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dejavu"));
    command.env("DEJAVU_NO_UPDATE_CHECK", "1");
    command
}

fn temp_root(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("dejavu-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    root
}

#[test]
fn memory_show_accepts_the_exact_leading_hyphen_key_from_memory_list() {
    let root = temp_root("memory-cli");
    let dir = root.join("-Users-example-project").join("memory");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("MEMORY.md"), "# Synthetic project memory").unwrap();
    let root_arg = root.to_str().unwrap();

    let list = dejavu()
        .args(["memory", "list", "--root", root_arg, "--json"])
        .output()
        .unwrap();
    assert!(list.status.success());
    let projects: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    let project = projects[0]["project"].as_str().unwrap();
    assert_eq!(project, "-Users-example-project");

    let show = dejavu()
        .args(["memory", "show", project, "--root", root_arg])
        .output()
        .unwrap();
    assert!(show.status.success());
    assert!(String::from_utf8_lossy(&show.stdout).contains("Synthetic project memory"));

    let invalid = dejavu()
        .args(["memory", "show", "--typo", "--root", root_arg])
        .output()
        .unwrap();
    assert_eq!(invalid.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("unknown flag"));
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn memory_reads_claude_config_dir_projects() {
    let root = temp_root("memory-config");
    let dir = root.join("projects").join("-p").join("memory");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("notes.md"), "synthetic").unwrap();
    let files = dejavu()
        .args(["memory", "list", "--files"])
        .env("CLAUDE_CONFIG_DIR", &root)
        .output()
        .unwrap();
    assert!(files.status.success());
    let expected = format!("-p/notes.md\t{}\n", dir.join("notes.md").display());
    assert_eq!(String::from_utf8_lossy(&files.stdout), expected);
    std::fs::remove_dir_all(&root).unwrap();
}
