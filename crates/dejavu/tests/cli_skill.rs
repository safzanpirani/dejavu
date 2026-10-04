//! Keep the shipped agent instructions on the supported CLI surface.
use std::process::Command;

#[test]
fn canonical_skill_teaches_current_commands_json_and_query_defaults() {
    let skill = include_str!("../../../skills/dejavu/SKILL.md");
    for obsolete in ["src/cli.ts", "gpt-5.6-luna"] {
        assert!(
            !skill.contains(obsolete),
            "obsolete skill instruction: {obsolete}"
        );
    }
    for command in [
        "search",
        "find",
        "last",
        "pack",
        "transcript",
        "memory",
        "query",
    ] {
        assert!(
            skill.contains(&format!("dejavu {command}")),
            "skill omits {command}"
        );
        let help = Command::new(env!("CARGO_BIN_EXE_dejavu"))
            .args([command, "--help"])
            .output()
            .unwrap();
        assert!(help.status.success());
        assert!(!help.stdout.contains(&0x1b));
    }
    let output = Command::new(env!("CARGO_BIN_EXE_dejavu"))
        .args(["query", "--help"])
        .output()
        .unwrap();
    let help = String::from_utf8(output.stdout).unwrap();
    for setting in [
        "--harness",
        "--effort",
        "DEJAVU_QUERY_HARNESS",
        "DEJAVU_QUERY_MODEL",
        "DEJAVU_QUERY_EFFORT",
        "XDG_CONFIG_HOME",
        "gpt-6-luna",
    ] {
        assert!(skill.contains(setting), "skill omits {setting}");
        assert!(help.contains(setting), "query help omits {setting}");
    }
    for contract in [
        ".matches[].path",
        ".hits[].path",
        ".[].path",
        ".events[]",
        "dejavu -- search",
        "--from-event",
        "--budget-chars",
        "NO_COLOR",
    ] {
        assert!(skill.contains(contract), "skill omits {contract}");
    }
}
