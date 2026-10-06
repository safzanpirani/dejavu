//! Transcript path helpers (`transcript-paths.ts`) plus the POSIX `node:path`
//! functions the TypeScript used (`join`, `resolve`, `normalize`).
//! Functions with a `_with` suffix take `home` (or roots) explicitly; the plain
//! versions read `$HOME` and the default store roots, as the TypeScript defaults did.

use crate::js;
use crate::sources::default_roots;
use crate::types::TranscriptSource;
use std::borrow::Cow;
use std::sync::OnceLock;

/// `process.env.HOME ?? ""`, read once.
pub fn env_home() -> &'static str {
    static HOME: OnceLock<String> = OnceLock::new();
    HOME.get_or_init(crate::sources::home_dir)
}

/// `compactHome(path)`: strips a leading `$HOME/`.
pub fn compact_home(path: &str) -> &str {
    compact_home_with(path, env_home())
}

/// `compactHome(path, home)`.
pub fn compact_home_with<'a>(path: &'a str, home: &str) -> &'a str {
    if !home.is_empty()
        && path.len() > home.len()
        && path.starts_with(home)
        && (path.as_bytes()[home.len()] == b'/'
            || (cfg!(windows) && path.as_bytes()[home.len()] == b'\\'))
    {
        &path[home.len() + 1..]
    } else {
        path
    }
}

/// `text.toLowerCase()`, without allocating a Unicode-aware copy for ASCII text.
pub fn js_lower(text: &str) -> Cow<'_, str> {
    if text.is_ascii() {
        if text.bytes().any(|b| b.is_ascii_uppercase()) {
            Cow::Owned(text.to_ascii_lowercase())
        } else {
            Cow::Borrowed(text)
        }
    } else {
        Cow::Owned(text.to_lowercase())
    }
}

/// `text.toLowerCase().indexOf(query.toLowerCase())` in UTF-16 units.
fn lower_index_of(text: &str, query: &str) -> Option<usize> {
    let haystack = js_lower(text);
    let needle = js_lower(query);
    let byte = haystack.find(needle.as_ref())?;
    Some(if haystack.is_ascii() {
        byte
    } else {
        js::len(&haystack[..byte])
    })
}

/// `snippetAround(text, query, 100)`.
pub fn snippet_around(text: &str, query: &str) -> String {
    snippet_around_radius(text, query, 100)
}

/// `snippetAround(text, query, radius)`: the first case-insensitive match with
/// `radius` UTF-16 units of context and `...` where it cut, or the first
/// `2 * radius` units when the query does not occur.
pub fn snippet_around_radius(text: &str, query: &str, radius: usize) -> String {
    let Some(index) = lower_index_of(text, query) else {
        return js::prefix(text, radius * 2).to_string();
    };
    let length = js::len(text);
    let start = index.saturating_sub(radius);
    let end = length.min(index + js::len(query) + radius);
    let body = js::slice(text, start, end);
    let mut out = String::with_capacity(body.len() + 6);
    if start > 0 {
        out.push_str("...");
    }
    out.push_str(body);
    if end < length {
        out.push_str("...");
    }
    out
}

/// `countOccurrences(text, query)`: non-overlapping case-insensitive matches; 0 for an empty query.
pub fn count_occurrences(text: &str, query: &str) -> usize {
    let needle = js_lower(query);
    if needle.is_empty() {
        return 0;
    }
    js_lower(text).matches(needle.as_ref()).count()
}

fn is_sep(byte: u8) -> bool {
    byte == b'/' || byte == b'\\'
}

fn digits(bytes: &[u8], at: usize, count: usize) -> bool {
    bytes.len() >= at + count && bytes[at..at + count].iter().all(u8::is_ascii_digit)
}

/// `dateFromPath(path)`: the first `YYYY-MM-DDT`, else `sessions/YYYY/MM/DD`, else `unknown`.
pub fn date_from_path(path: &str) -> String {
    let b = path.as_bytes();
    for i in 0..b.len() {
        if digits(b, i, 4)
            && b.get(i + 4) == Some(&b'-')
            && digits(b, i + 5, 2)
            && b.get(i + 7) == Some(&b'-')
            && digits(b, i + 8, 2)
            && b.get(i + 10) == Some(&b'T')
        {
            return path[i..i + 10].to_string();
        }
    }
    for (i, _) in path.match_indices("sessions") {
        let p = i + 8;
        if b.get(p).is_some_and(|&c| is_sep(c))
            && digits(b, p + 1, 4)
            && b.get(p + 5).is_some_and(|&c| is_sep(c))
            && digits(b, p + 6, 2)
            && b.get(p + 8).is_some_and(|&c| is_sep(c))
            && digits(b, p + 9, 2)
        {
            return format!(
                "{}-{}-{}",
                &path[p + 1..p + 5],
                &path[p + 6..p + 8],
                &path[p + 9..p + 11]
            );
        }
    }
    "unknown".to_string()
}

/// `projectFromPiPath(path)` with `$HOME`.
pub fn project_from_pi_path(path: &str) -> String {
    project_from_pi_path_with(path, env_home())
}

/// `projectFromPiPath(path, home)`: decodes `sessions/--<encoded cwd>--/`.
pub fn project_from_pi_path_with(path: &str, home: &str) -> String {
    // /sessions[/\\](--.*?--)[/\\]/
    let b = path.as_bytes();
    for (i, _) in path.match_indices("sessions") {
        let p = i + 8;
        if !(b.get(p).is_some_and(|&c| is_sep(c))
            && b.get(p + 1) == Some(&b'-')
            && b.get(p + 2) == Some(&b'-'))
        {
            continue;
        }
        let open = p + 3;
        let mut k = open;
        while k + 2 < b.len() {
            // `.` stops at line terminators: \n, \r, U+2028, U+2029.
            if matches!(b[k], b'\n' | b'\r')
                || (b[k] == 0xE2 && b[k + 1] == 0x80 && matches!(b[k + 2], 0xA8 | 0xA9))
            {
                break;
            }
            if b[k] == b'-' && b[k + 1] == b'-' && is_sep(b[k + 2]) {
                return decode_project(&path[open..k], home);
            }
            k += 1;
        }
    }
    "~".to_string()
}

/// `projectFromClaudePath(path)` with `$HOME` and the default Claude store root.
pub fn project_from_claude_path(path: &str) -> String {
    project_from_claude_path_with(path, env_home(), &default_roots().claude)
}

/// `projectFromClaudePath(path, home, projectsRoot)`: decodes the project directory
/// under the configured projects root, else under any `.claude/projects/`.
pub fn project_from_claude_path_with(path: &str, home: &str, projects_root: &str) -> String {
    project_from_encoded_dir(path, home, projects_root, ".claude", "projects")
}

/// `projectFromDroidPath(path)` with `$HOME` and the default Droid store root.
pub fn project_from_droid_path(path: &str) -> String {
    project_from_droid_path_with(path, env_home(), &default_roots().droid)
}

/// Decodes a Droid session directory (`sessions/-Users-dev-app/<id>.jsonl`)
/// under the configured sessions root, else under any `.factory/sessions/`.
/// Droid encodes the working directory the way Claude does.
pub fn project_from_droid_path_with(path: &str, home: &str, sessions_root: &str) -> String {
    project_from_encoded_dir(path, home, sessions_root, ".factory", "sessions")
}

/// The project of a transcript kept in `<root>/<encoded cwd>/<session>.jsonl`,
/// where the fallback layout is `<dir>/<sub>/<encoded cwd>/`.
fn project_from_encoded_dir(path: &str, home: &str, root: &str, dir: &str, sub: &str) -> String {
    let mut encoded: Option<&str> = None;
    if path.len() > root.len() && path.starts_with(root) && path.as_bytes()[root.len()] == b'/' {
        let rest = &path[root.len() + 1..];
        if let Some(cut) = rest.find(['/', '\\']) {
            encoded = Some(&rest[..cut]);
        }
    }
    if encoded.is_none() {
        // /\.<dir>[/\\]<sub>[/\\]([^/\\]+)[/\\]/, e.g. `.claude/projects/<encoded>/`
        let b = path.as_bytes();
        for (i, _) in path.match_indices(dir) {
            let p = i + dir.len();
            if !(b.get(p).is_some_and(|&c| is_sep(c)) && path[p + 1..].starts_with(sub)) {
                continue;
            }
            let q = p + 1 + sub.len();
            if !b.get(q).is_some_and(|&c| is_sep(c)) {
                continue;
            }
            let start = q + 1;
            let Some(len) = path[start..].find(['/', '\\']) else {
                continue;
            };
            if len > 0 {
                encoded = Some(&path[start..start + len]);
                break;
            }
        }
    }
    match encoded {
        Some(encoded) if !encoded.is_empty() => {
            decode_project(encoded.strip_prefix('-').unwrap_or(encoded), home)
        }
        _ => "~".to_string(),
    }
}

/// `projectFromTranscriptPath(path, source)`: Claude, Pi, omp, and Droid decode the path;
/// agy looks its conversation up in `conversation_summaries.db`; others are `~`.
pub fn project_from_transcript_path(path: &str, source: TranscriptSource) -> String {
    match source {
        TranscriptSource::Claude => project_from_claude_path(path),
        TranscriptSource::Pi | TranscriptSource::Omp => project_from_pi_path(path),
        TranscriptSource::Droid => project_from_droid_path(path),
        TranscriptSource::Agy => crate::agy::project_for_path(path).unwrap_or_else(|| "~".into()),
        _ => "~".to_string(),
    }
}

/// `projectFromTranscriptText(path, source, text)`: a Codex transcript's first
/// `session_meta` cwd or a Droid transcript's `session_start` cwd (home-compacted);
/// other sources, and transcripts without that row, derive the project from the path.
pub fn project_from_transcript_text(path: &str, source: TranscriptSource, text: &str) -> String {
    let cwd = match source {
        TranscriptSource::Codex => codex_session_cwd(text),
        TranscriptSource::Droid => droid_session_cwd(text),
        // The encoded directory name turns both '/' and '-' into '-', so the
        // recorded cwd is the only lossless project.
        TranscriptSource::Claude => row_cwd(text, None),
        TranscriptSource::Pi | TranscriptSource::Omp => row_cwd(text, Some("session")),
        // agy keeps the workspace in conversation_summaries.db, read by path.
        TranscriptSource::Opencode | TranscriptSource::Agy => None,
    };
    match cwd {
        Some(cwd) => compact_home(&cwd).to_string(),
        None => project_from_transcript_path(path, source),
    }
}

/// The `cwd` of a Droid transcript's `session_start` row, when it is a non-empty
/// string. Droid writes that row first; later rows never carry a `cwd`.
pub fn droid_session_cwd(text: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Start {
        #[serde(rename = "type")]
        kind: Option<serde_json::Value>,
        cwd: Option<serde_json::Value>,
    }
    for line in text.split('\n') {
        if line.trim().is_empty() {
            continue;
        }
        let Some(row) = crate::reader::parse_json_line::<Start>(line) else {
            continue;
        };
        if row.kind.as_ref().and_then(|v| v.as_str()) != Some("session_start") {
            continue;
        }
        return match row.cwd {
            Some(serde_json::Value::String(cwd)) if !cwd.is_empty() => Some(cwd),
            _ => None,
        };
    }
    None
}

/// The first non-empty string `cwd` of a row whose `type` is `kind` (any row
/// when `kind` is `None`): a Claude entry or a Pi or omp `session` header.
fn row_cwd(text: &str, kind: Option<&str>) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Row {
        #[serde(rename = "type")]
        kind: Option<serde_json::Value>,
        cwd: Option<serde_json::Value>,
    }
    for line in text.split('\n') {
        if !line.contains("\"cwd\"") {
            continue;
        }
        let Some(row) = crate::reader::parse_json_line::<Row>(line) else {
            continue;
        };
        if kind.is_some() && row.kind.as_ref().and_then(|v| v.as_str()) != kind {
            continue;
        }
        if let Some(serde_json::Value::String(cwd)) = row.cwd
            && !cwd.is_empty()
        {
            return Some(cwd);
        }
    }
    None
}

/// The `payload.cwd` of the first `session_meta` row that has a non-empty one.
pub fn codex_session_cwd(text: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Meta {
        cwd: Option<serde_json::Value>,
    }
    #[derive(serde::Deserialize)]
    struct Row {
        #[serde(rename = "type")]
        kind: Option<serde_json::Value>,
        payload: Option<serde_json::Value>,
    }
    for line in text.split('\n') {
        // A session_meta row spells its type literally or through a \u escape.
        if !line.contains("session_meta") && !line.contains("\\u") {
            continue;
        }
        let Some(row) = crate::reader::parse_json_line::<Row>(line) else {
            continue;
        };
        if row.kind.as_ref().and_then(|v| v.as_str()) != Some("session_meta") {
            continue;
        }
        let cwd = row
            .payload
            .and_then(|payload| serde_json::from_value::<Meta>(payload).ok())
            .and_then(|meta| meta.cwd);
        if let Some(serde_json::Value::String(cwd)) = cwd
            && !cwd.is_empty()
        {
            return Some(cwd);
        }
    }
    None
}

fn decode_project(encoded_path: &str, home: &str) -> String {
    let trimmed = home.strip_prefix(['/', '\\']).unwrap_or(home);
    let home_encoded: String = trimmed
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':') {
                '-'
            } else {
                c
            }
        })
        .collect();
    let mut encoded = encoded_path;
    if !home_encoded.is_empty()
        && encoded.len() > home_encoded.len()
        && encoded.starts_with(&home_encoded)
        && encoded.as_bytes()[home_encoded.len()] == b'-'
    {
        encoded = &encoded[home_encoded.len() + 1..];
    } else if encoded == home_encoded {
        return "~".to_string();
    }
    if encoded.is_empty() {
        "~".to_string()
    } else {
        encoded.replace('-', "/")
    }
}

/// Node's `normalizeString`: resolves `.` and `..` segments and drops empty ones.
fn normalize_segments(path: &str, allow_above_root: bool) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|last| *last != "..") {
                    parts.pop();
                } else if allow_above_root {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

/// `path.posix.normalize(path)`.
pub fn normalize(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let absolute = path.starts_with('/');
    let mut out = normalize_segments(path, !absolute);
    if out.is_empty() && !absolute {
        out.push('.');
    }
    if !out.is_empty() && path.ends_with('/') {
        out.push('/');
    }
    if absolute { format!("/{out}") } else { out }
}

/// `path.posix.join(...parts)`.
pub fn join(parts: &[&str]) -> String {
    let joined = parts
        .iter()
        .filter(|part| !part.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("/");
    if joined.is_empty() {
        ".".to_string()
    } else {
        normalize(&joined)
    }
}

/// A Windows drive (`C:\`, `C:/`) or UNC (`\\server`) path.
pub fn is_windows_absolute(path: &str) -> bool {
    let b = path.as_bytes();
    (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'/' | b'\\'))
        || path.starts_with("\\\\")
}

/// `path.posix.isAbsolute(path)`; on Windows, drive and UNC paths too.
pub fn is_absolute(path: &str) -> bool {
    path.starts_with('/') || (cfg!(windows) && is_windows_absolute(path))
}

/// `path.posix.resolve(path)` against the current directory. On Windows the
/// POSIX rules would turn `C:\x` into `<cwd>/C:\x`, so native paths go through
/// the platform's own joining instead.
pub fn resolve(path: &str) -> String {
    if cfg!(windows) && is_windows_absolute(path) {
        return path.to_string();
    }
    if is_absolute(path) {
        return resolve_from("/", path);
    }
    let cwd = std::env::current_dir()
        .map(|dir| dir.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "/".into());
    if cfg!(windows) {
        return std::path::Path::new(&cwd)
            .join(path)
            .to_string_lossy()
            .into_owned();
    }
    resolve_from(&cwd, path)
}

fn resolve_from(base: &str, path: &str) -> String {
    let combined = if is_absolute(path) {
        path.to_string()
    } else {
        format!("{base}/{path}")
    };
    format!("/{}", normalize_segments(&combined, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_windows_drive_and_unc_paths() {
        for path in [r"C:\Users\dev", "c:/x", r"\\nas\share"] {
            assert!(is_windows_absolute(path), "{path}");
        }
        for path in ["/home/dev", "C:", "C:x", "rel/x", r"\single", "1:/x"] {
            assert!(!is_windows_absolute(path), "{path}");
        }
        assert_eq!(is_absolute(r"C:\Users\dev"), cfg!(windows));
    }

    #[cfg(windows)]
    #[test]
    fn resolve_keeps_native_windows_paths() {
        assert_eq!(resolve(r"C:\Users\dev\.claude"), r"C:\Users\dev\.claude");
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(resolve("x"), cwd.join("x").to_string_lossy());
        assert_eq!(
            compact_home_with(r"C:\Users\dev\app", r"C:\Users\dev"),
            "app"
        );
    }

    #[test]
    fn prefers_the_recorded_cwd_over_the_lossy_encoded_directory() {
        let home = env_home();
        // A Windows home holds backslashes, which must be escaped inside JSON.
        let cwd = |rest: &str| serde_json::to_string(&format!("{home}/{rest}")).unwrap();
        let claude = format!(
            "{{\"type\":\"mode\",\"sessionId\":\"s\"}}\n{{\"type\":\"user\",\"cwd\":{}}}\n",
            cwd("Development/hul-tech")
        );
        assert_eq!(
            project_from_transcript_text(
                "/x/.claude/projects/-Users-dev-Development-hul-tech/a.jsonl",
                TranscriptSource::Claude,
                &claude
            ),
            "Development/hul-tech"
        );
        let pi = format!(
            "{{\"type\":\"session\",\"cwd\":{}}}\n{{\"type\":\"message\",\"cwd\":\"/elsewhere\"}}\n",
            cwd("Development/reason-leak")
        );
        assert_eq!(
            project_from_transcript_text(
                "/x/sessions/--Users-dev-Development-reason-leak--/a.jsonl",
                TranscriptSource::Pi,
                &pi
            ),
            "Development/reason-leak"
        );
        // Without a recorded cwd, the directory still decodes.
        assert_eq!(
            project_from_transcript_text(
                "/x/sessions/--a-b--/a.jsonl",
                TranscriptSource::Pi,
                "{\"type\":\"message\",\"cwd\":\"/elsewhere\"}"
            ),
            "a/b"
        );
    }

    #[test]
    fn decodes_pi_and_claude_project_directories() {
        assert_eq!(
            project_from_pi_path_with(
                "/Users/dev/.pi/agent/sessions/--Users-dev-Development-projects-dejavu--/x.jsonl",
                "/Users/dev"
            ),
            "Development/projects/dejavu"
        );
        assert_eq!(
            project_from_claude_path_with(
                "/Users/dev/.claude/projects/-Users-dev-Development-projects-dejavu/x.jsonl",
                "/Users/dev",
                "/elsewhere/projects"
            ),
            "Development/projects/dejavu"
        );
        assert_eq!(
            project_from_pi_path_with("/x/sessions/--Users-dev--/a.jsonl", "/Users/dev"),
            "~"
        );
        assert_eq!(
            project_from_pi_path_with("/x/sessions/no/a.jsonl", "/Users/dev"),
            "~"
        );
        assert_eq!(
            project_from_pi_path_with("/x/sessions/----/a.jsonl", ""),
            "~"
        );
        // The lazy group still has to end in `--` followed by a separator.
        assert_eq!(
            project_from_pi_path_with("/x/sessions/--a--b--/a.jsonl", ""),
            "a//b"
        );
        assert_eq!(
            project_from_claude_path_with("/nowhere/x.jsonl", "/Users/dev", "/r"),
            "~"
        );
    }

    #[test]
    fn resolves_projects_under_a_configured_claude_root() {
        assert_eq!(
            project_from_claude_path_with(
                "/home/owner/.claude-rakhi/projects/-home-owner-work-app/s.jsonl",
                "/home/owner",
                "/home/owner/.claude-rakhi/projects"
            ),
            "work/app"
        );
        // A file directly under the root has no project directory.
        assert_eq!(
            project_from_claude_path_with("/r/projects/s.jsonl", "/h", "/r/projects"),
            "~"
        );
    }

    #[test]
    fn dates_from_paths() {
        assert_eq!(
            date_from_path("/x/rollout-2026-09-20T10-00-00-abc.jsonl"),
            "2026-09-20"
        );
        assert_eq!(
            date_from_path("/x/sessions/2026/09/20/rollout.jsonl"),
            "2026-09-20"
        );
        assert_eq!(date_from_path("/x/sessions/26/09/20/a.jsonl"), "unknown");
    }

    #[test]
    fn snippets_and_counts() {
        assert_eq!(count_occurrences("Needle needle NEEDLE", "needle"), 3);
        assert_eq!(count_occurrences("aaaa", "aa"), 2);
        assert_eq!(count_occurrences("abc", ""), 0);
        let text = format!("{}Needle{}", "a".repeat(150), "b".repeat(150));
        let snippet = snippet_around(&text, "needle");
        assert_eq!(
            snippet,
            format!("...{}Needle{}...", "a".repeat(100), "b".repeat(100))
        );
        assert_eq!(snippet_around("short", "missing"), "short");
        // JavaScript would start at the lone low surrogate; Rust keeps the whole pair.
        assert_eq!(
            snippet_around_radius("😀😀needle", "needle", 1),
            "...😀needle"
        );
        assert_eq!(compact_home_with("/Users/dev/x", "/Users/dev"), "x");
        assert_eq!(compact_home_with("/Users/dev", "/Users/dev"), "/Users/dev");
        assert_eq!(
            compact_home_with("/Users/devx/y", "/Users/dev"),
            "/Users/devx/y"
        );
    }

    #[test]
    fn node_path_semantics() {
        assert_eq!(
            join(&["/home/owner", ".claude", "projects"]),
            "/home/owner/.claude/projects"
        );
        assert_eq!(join(&["/a/", "../b", "./c/"]), "/b/c/");
        assert_eq!(join(&["", ""]), ".");
        assert_eq!(normalize("a/../.."), "..");
        assert_eq!(resolve("/a/b/../c/"), "/a/c");
        assert_eq!(resolve("/"), "/");
    }

    #[test]
    fn droid_projects_come_from_session_start_or_the_session_directory() {
        assert_eq!(
            project_from_droid_path_with(
                "/Users/dev/.factory/sessions/-Users-dev-Development-app/s.jsonl",
                "/Users/dev",
                "/elsewhere/sessions"
            ),
            "Development/app"
        );
        assert_eq!(
            project_from_droid_path_with(
                "/srv/fh/.factory/sessions/-Users-dev/s.jsonl",
                "/Users/dev",
                "/srv/fh/.factory/sessions"
            ),
            "~"
        );
        assert_eq!(
            project_from_droid_path_with("/x/s.jsonl", "/Users/dev", "/r"),
            "~"
        );
        let text = "{\"type\":\"session_start\",\"id\":\"s\",\"cwd\":\"/w/app\"}\n{\"type\":\"message\"}\n";
        assert_eq!(droid_session_cwd(text).as_deref(), Some("/w/app"));
        assert_eq!(
            droid_session_cwd("{\"type\":\"session_start\",\"cwd\":\"\"}"),
            None
        );
        assert_eq!(droid_session_cwd("not json\n"), None);
        assert_eq!(
            project_from_transcript_text("/x.jsonl", TranscriptSource::Droid, text),
            "/w/app"
        );
    }

    #[test]
    fn reads_the_codex_session_cwd() {
        let text = "{\"type\":\"session_meta\",\"payload\":{\"cwd\":\"\"}}\nnot json\n{\"type\":\"session_\\u006deta\",\"payload\":{\"cwd\":\"/w/app\"}}\n";
        assert_eq!(codex_session_cwd(text).as_deref(), Some("/w/app"));
        assert_eq!(
            project_from_transcript_text("/x.jsonl", TranscriptSource::Codex, "{}"),
            "~"
        );
    }
}
