//! The Droid session running this process. Droid exports no session id to the
//! commands it runs, so it is inferred: the nearest `droid` ancestor process,
//! its working directory's session folder, and that folder's newest transcript.
//! Droid appends each tool call to the transcript before running the tool, so
//! the session asking is the one written last.

use std::path::Path;

/// Ancestor hops checked before giving up, against cycles in a stale table.
const MAX_HOPS: usize = 64;

/// The active Droid session id, when an ancestor process is Droid.
pub fn active_droid_session() -> Option<String> {
    let table = process_table()?;
    let pid = droid_ancestor(&table, std::process::id())?;
    let cwd = process_cwd(pid)?;
    let dir = Path::new(&crate::sources::default_roots().droid).join(session_dir_name(&cwd));
    newest_transcript(&dir)
}

/// One `ps` row: pid, parent pid, and command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessRow {
    pub pid: u32,
    pub ppid: u32,
    pub args: String,
}

/// Parses `ps -o pid=,ppid=,args=` output.
pub fn parse_ps(text: &str) -> Vec<ProcessRow> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.trim_start().splitn(2, char::is_whitespace);
            let pid = fields.next()?.parse().ok()?;
            let rest = fields.next()?.trim_start();
            let mut fields = rest.splitn(2, char::is_whitespace);
            let ppid = fields.next()?.parse().ok()?;
            let args = fields.next().unwrap_or("").trim().to_string();
            Some(ProcessRow { pid, ppid, args })
        })
        .collect()
}

/// Whether a command line runs Droid, directly or as `node .../droid`.
fn is_droid(args: &str) -> bool {
    let name = |token: &str| {
        let base = token.rsplit(['/', '\\']).next().unwrap_or(token);
        base == "droid" || base == "droid.exe"
    };
    let mut tokens = args.split_whitespace();
    match tokens.next() {
        Some(first) if name(first) => true,
        Some(first) if first.rsplit('/').next() == Some("node") => tokens.next().is_some_and(name),
        _ => false,
    }
}

/// The nearest ancestor of `start` (excluding `start`) running Droid.
pub fn droid_ancestor(table: &[ProcessRow], start: u32) -> Option<u32> {
    let parent = |pid: u32| table.iter().find(|row| row.pid == pid).map(|row| row.ppid);
    let mut pid = parent(start)?;
    for _ in 0..MAX_HOPS {
        if pid <= 1 {
            return None;
        }
        let row = table.iter().find(|row| row.pid == pid)?;
        if is_droid(&row.args) {
            return Some(pid);
        }
        pid = row.ppid;
    }
    None
}

/// Droid's session folder for a working directory: every `/` becomes `-`.
pub fn session_dir_name(cwd: &str) -> String {
    cwd.replace('/', "-")
}

/// The file stem of the most recently modified `.jsonl` in `dir`.
pub fn newest_transcript(dir: &Path) -> Option<String> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl"))
        .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
        .max_by_key(|(modified, _)| *modified)
        .and_then(|(_, path)| Some(path.file_stem()?.to_string_lossy().into_owned()))
}

/// Parses `lsof -Fn` output: the first `n` field is the path.
pub fn parse_lsof_cwd(text: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.strip_prefix('n'))
        .filter(|path| !path.is_empty())
        .map(str::to_string)
}

#[cfg(unix)]
fn process_table() -> Option<Vec<ProcessRow>> {
    let output = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,args="])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    Some(parse_ps(&String::from_utf8_lossy(&output.stdout)))
}

#[cfg(not(unix))]
fn process_table() -> Option<Vec<ProcessRow>> {
    None
}

#[cfg(unix)]
fn process_cwd(pid: u32) -> Option<String> {
    if let Ok(path) = std::fs::read_link(format!("/proc/{pid}/cwd")) {
        return Some(path.to_string_lossy().into_owned());
    }
    let output = std::process::Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    parse_lsof_cwd(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(not(unix))]
fn process_cwd(_pid: u32) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pid: u32, ppid: u32, args: &str) -> ProcessRow {
        ProcessRow {
            pid,
            ppid,
            args: args.into(),
        }
    }

    #[test]
    fn parses_ps_rows_with_padded_columns_and_spaced_args() {
        let text =
            "    1     0 /sbin/launchd\n35431 34980 droid\n41455 35431 /bin/bash -c echo hi\n\n";
        assert_eq!(
            parse_ps(text),
            [
                row(1, 0, "/sbin/launchd"),
                row(35431, 34980, "droid"),
                row(41455, 35431, "/bin/bash -c echo hi"),
            ]
        );
    }

    #[test]
    fn finds_the_nearest_droid_ancestor_through_wrapper_shells() {
        let table = [
            row(1, 0, "/sbin/launchd"),
            row(10, 1, "-zsh"),
            row(20, 10, "droid"),
            row(30, 20, "/bin/bash -c watch_owner"),
            row(40, 30, "/bin/bash -c dejavu last"),
            row(50, 40, "dejavu last"),
        ];
        assert_eq!(droid_ancestor(&table, 50), Some(20));
        assert_eq!(droid_ancestor(&table, 20), None);
        let node = [
            row(1, 0, "init"),
            row(5, 1, "node /opt/cli/bin/droid exec"),
            row(6, 5, "dejavu"),
        ];
        assert_eq!(droid_ancestor(&node, 6), Some(5));
        let plain = [row(1, 0, "init"), row(5, 1, "claude"), row(6, 5, "dejavu")];
        assert_eq!(droid_ancestor(&plain, 6), None);
        let cycle = [row(5, 6, "sh"), row(6, 5, "sh")];
        assert_eq!(droid_ancestor(&cycle, 5), None);
    }

    #[test]
    fn droid_names_session_folders_by_replacing_slashes() {
        assert_eq!(
            session_dir_name("/Users/dev/Development/projects/dejavu"),
            "-Users-dev-Development-projects-dejavu"
        );
        assert_eq!(session_dir_name("/Users/dev/.config"), "-Users-dev-.config");
        assert_eq!(session_dir_name("/private/tmp/a-1"), "-private-tmp-a-1");
    }

    #[test]
    fn reads_the_cwd_from_lsof_field_output() {
        assert_eq!(
            parse_lsof_cwd("p35431\nfcwd\nn/Users/dev/app\n"),
            Some("/Users/dev/app".into())
        );
        assert_eq!(parse_lsof_cwd("p35431\n"), None);
    }

    #[test]
    fn picks_the_most_recently_written_transcript() {
        let dir = std::env::temp_dir().join(format!("dejavu-droid-active-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        for (name, age) in [("older", Some(old)), ("newest", None), ("notes", None)] {
            let ext = if name == "notes" {
                "settings.json"
            } else {
                "jsonl"
            };
            let file = std::fs::File::create(dir.join(format!("{name}.{ext}"))).unwrap();
            if let Some(time) = age {
                file.set_modified(time).unwrap();
            }
        }
        assert_eq!(newest_transcript(&dir), Some("newest".into()));
        assert_eq!(newest_transcript(&dir.join("missing")), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
