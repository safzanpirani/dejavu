//! Claude's cross-project Markdown memory corpus: `<root>/<project>/memory/*.md`.
//! A port of `memory.ts`.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemoryFile {
    pub project: String,
    pub name: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemoryProject {
    pub project: String,
    pub path: String,
    pub files: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemoryMatch {
    pub project: String,
    pub name: String,
    pub path: String,
    pub count: usize,
    pub snippets: Vec<String>,
    /// The frontmatter `description`, when the file has one.
    pub description: Option<String>,
    /// The frontmatter `type` (top level or under `metadata`), when present.
    #[serde(rename = "type")]
    pub kind: Option<String>,
}

/// Which memory files a search reads and how many results it returns.
#[derive(Debug, Clone, Default)]
pub struct SearchOptions {
    pub limit: usize,
    pub snippets: usize,
    /// Case-insensitive substring of the project key.
    pub project: Option<String>,
    /// Exact frontmatter type, compared without case.
    pub kind: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ShownMemory {
    pub file: MemoryFile,
    pub content: String,
}

/// File access, replaceable in tests.
pub trait MemoryStore {
    /// Every `*/memory/*.md` file below `root`, as full paths.
    fn glob(&self, root: &str) -> Result<Vec<String>, String>;
    fn read(&self, path: &str) -> Result<String, String>;
}

/// The real filesystem, matching `Bun.Glob("*/memory/*.md")`: no dot entries
/// and no symbolic links, at either directory level or for the file.
pub struct Disk;

impl MemoryStore for Disk {
    fn glob(&self, root: &str) -> Result<Vec<String>, String> {
        let entries = std::fs::read_dir(root).map_err(|error| fs_error(&error, root))?;
        let mut paths = Vec::new();
        for project in entries.flatten() {
            let project_name = project.file_name().to_string_lossy().into_owned();
            if project_name.starts_with('.') || !project.file_type().is_ok_and(|kind| kind.is_dir())
            {
                continue;
            }
            let memory = project.path().join("memory");
            if !std::fs::symlink_metadata(&memory).is_ok_and(|meta| meta.is_dir()) {
                continue;
            }
            let Ok(files) = std::fs::read_dir(&memory) else {
                continue;
            };
            for file in files.flatten() {
                let name = file.file_name().to_string_lossy().into_owned();
                if name.starts_with('.')
                    || !name.ends_with(".md")
                    || !file.file_type().is_ok_and(|kind| kind.is_file())
                {
                    continue;
                }
                paths.push(join(root, &format!("{project_name}/memory/{name}")));
            }
        }
        paths.sort_by(|a, b| utf16_cmp(a, b));
        Ok(paths)
    }

    fn read(&self, path: &str) -> Result<String, String> {
        let bytes = std::fs::read(path).map_err(|error| fs_error(&error, path))?;
        let text = String::from_utf8_lossy(&bytes);
        // Bun's text() decodes like TextDecoder, which drops a leading BOM.
        Ok(text.strip_prefix('\u{feff}').unwrap_or(&text).to_string())
    }
}

/// Node-style error text, as Bun reported a failed scan or read.
fn fs_error(error: &std::io::Error, path: &str) -> String {
    let code = match error.kind() {
        std::io::ErrorKind::NotFound => "ENOENT: no such file or directory",
        std::io::ErrorKind::PermissionDenied => "EACCES: permission denied",
        std::io::ErrorKind::NotADirectory => "ENOTDIR: not a directory",
        std::io::ErrorKind::IsADirectory => "EISDIR: is a directory",
        _ => return format!("{error}, open '{path}'"),
    };
    format!("{code}, open '{path}'")
}

/// `path.join(root, relative)` for the memory layout: separators collapse,
/// `.` segments drop, and `..` segments pop.
fn join(root: &str, relative: &str) -> String {
    let sep = std::path::MAIN_SEPARATOR;
    let combined = format!("{root}{sep}{relative}");
    let absolute = combined.starts_with(['/', '\\']);
    let mut parts: Vec<&str> = Vec::new();
    for part in combined.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." if parts.last().is_some_and(|last| *last != "..") => {
                parts.pop();
            }
            ".." if absolute => {}
            _ => parts.push(part),
        }
    }
    let joined = parts.join(&sep.to_string());
    if absolute {
        format!("{sep}{joined}")
    } else if joined.is_empty() {
        ".".into()
    } else {
        joined
    }
}

/// The Claude root: `$CLAUDE_CONFIG_DIR/projects`, else `~/.claude/projects`.
/// This is the `claude` entry of `transcriptStoreRoots` in `source-registry.ts`.
pub fn default_memory_root() -> String {
    claude_projects_root(
        std::env::var_os("CLAUDE_CONFIG_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from),
        home_dir(),
    )
}

fn claude_projects_root(config_dir: Option<PathBuf>, home: PathBuf) -> String {
    let base = match config_dir {
        Some(dir) => absolute(&dir),
        None => home.join(".claude"),
    };
    base.join("projects").to_string_lossy().into_owned()
}

fn absolute(path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    PathBuf::from(join(&joined.to_string_lossy(), ""))
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// The text before the first `/memory/` segment.
fn memory_dir_parent(path: &str) -> &str {
    let unix = path.find("/memory/");
    let windows = path.find("\\memory\\");
    match (unix, windows) {
        (Some(a), Some(b)) => &path[..a.min(b)],
        (Some(index), None) | (None, Some(index)) => &path[..index],
        (None, None) => path,
    }
}

fn project_from_path(path: &str) -> String {
    let parent = memory_dir_parent(path);
    parent
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(parent)
        .to_string()
}

fn basename(path: &str) -> String {
    path.rsplit(['/', '\\']).next().unwrap_or(path).to_string()
}

pub fn memory_files(root: &str, store: &dyn MemoryStore) -> Result<Vec<MemoryFile>, String> {
    let mut paths = store.glob(root)?;
    paths.sort_by(|a, b| utf16_cmp(a, b));
    Ok(paths
        .into_iter()
        .map(|path| MemoryFile {
            project: project_from_path(&path),
            name: basename(&path),
            path,
        })
        .collect())
}

pub fn list_memories(root: &str, store: &dyn MemoryStore) -> Result<Vec<MemoryProject>, String> {
    let mut grouped: Vec<MemoryProject> = Vec::new();
    for file in memory_files(root, store)? {
        match grouped
            .iter_mut()
            .find(|project| project.project == file.project)
        {
            Some(project) => project.files += 1,
            None => {
                let sep = if file.path.contains("/memory/") {
                    "/"
                } else {
                    "\\"
                };
                grouped.push(MemoryProject {
                    path: format!("{}{sep}memory", memory_dir_parent(&file.path)),
                    project: file.project,
                    files: 1,
                })
            }
        }
    }
    grouped.sort_by(|a, b| locale_compare(&a.project, &b.project));
    Ok(grouped)
}

/// The fields Claude's memory frontmatter carries, plus the line where the body starts.
#[derive(Debug, Default, PartialEq, Eq)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
    kind: Option<String>,
    body_start: usize,
}

/// Reads a leading `---` block line by line: `name` and `description` at the
/// top level, `type` at any depth (Claude nests it under `metadata`).
fn frontmatter(lines: &[&str]) -> Frontmatter {
    let mut front = Frontmatter::default();
    if lines.first().map(|line| line.trim()) != Some("---") {
        return front;
    }
    let Some(end) = lines.iter().skip(1).position(|line| line.trim() == "---") else {
        return front;
    };
    for line in &lines[1..=end] {
        let trimmed = line.trim_start();
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let value = unquote(value.trim());
        if value.is_empty() {
            continue;
        }
        let top_level = trimmed.len() == line.len();
        match key.trim() {
            "name" if top_level => front.name = Some(value.into()),
            "description" if top_level => front.description = Some(value.into()),
            "type" if front.kind.is_none() => front.kind = Some(value.into()),
            _ => {}
        }
    }
    front.body_start = end + 2;
    front
}

fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            return inner;
        }
    }
    value
}

/// Every whitespace-separated term must occur in a file, case-insensitively.
/// Files rank by terms found in the file name, frontmatter `name`, and
/// `description`, then the exact phrase, then occurrences (capped per term).
/// Snippets come from body lines, most distinct terms first.
pub fn search_memories(
    query: &str,
    root: &str,
    options: &SearchOptions,
    store: &dyn MemoryStore,
) -> Result<Vec<MemoryMatch>, String> {
    let mut terms: Vec<String> = Vec::new();
    for term in query.split_whitespace().map(str::to_lowercase) {
        if !terms.contains(&term) {
            terms.push(term);
        }
    }
    if terms.is_empty() {
        return Err("memory search needs at least one term".into());
    }
    let phrase = (terms.len() > 1).then(|| terms.join(" "));
    let project = options.project.as_deref().map(str::to_lowercase);
    let mut ranked = Vec::new();
    for file in memory_files(root, store)? {
        if project
            .as_deref()
            .is_some_and(|wanted| !file.project.to_lowercase().contains(wanted))
        {
            continue;
        }
        let Ok(text) = store.read(&file.path) else {
            continue;
        };
        let lower = text.to_lowercase();
        let counts: Vec<usize> = terms
            .iter()
            .map(|term| lower.matches(term).count())
            .collect();
        if counts.contains(&0) {
            continue;
        }
        let lines: Vec<&str> = text
            .split('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line))
            .collect();
        let front = frontmatter(&lines);
        if let Some(wanted) = options.kind.as_deref()
            && !front
                .kind
                .as_deref()
                .is_some_and(|kind| kind.eq_ignore_ascii_case(wanted))
        {
            continue;
        }
        let label = format!("{} {}", file.name, front.name.as_deref().unwrap_or("")).to_lowercase();
        let description = front.description.as_deref().unwrap_or("").to_lowercase();
        let mut score: usize = terms
            .iter()
            .zip(&counts)
            .map(|(term, count)| {
                4 * usize::from(label.contains(term))
                    + 2 * usize::from(description.contains(term))
                    + count.min(&5)
            })
            .sum();
        if phrase
            .as_deref()
            .is_some_and(|phrase| lower.contains(phrase))
        {
            score += 5;
        }
        let mut matching: Vec<(usize, &str)> = lines[front.body_start.min(lines.len())..]
            .iter()
            .filter_map(|line| {
                let line_lower = line.to_lowercase();
                let hits = terms
                    .iter()
                    .filter(|term| line_lower.contains(*term))
                    .count();
                (hits > 0).then_some((hits, *line))
            })
            .collect();
        matching.sort_by_key(|(hits, _)| std::cmp::Reverse(*hits));
        ranked.push((
            score,
            MemoryMatch {
                project: file.project,
                name: file.name,
                path: file.path,
                count: counts.iter().sum(),
                snippets: matching
                    .into_iter()
                    .take(options.snippets)
                    .map(|(_, line)| line.to_string())
                    .collect(),
                description: front.description,
                kind: front.kind,
            },
        ));
    }
    ranked.sort_by(|(a_score, a), (b_score, b)| {
        b_score
            .cmp(a_score)
            .then_with(|| b.count.cmp(&a.count))
            .then_with(|| locale_compare(&a.path, &b.path))
    });
    Ok(ranked
        .into_iter()
        .take(options.limit)
        .map(|(_, found)| found)
        .collect())
}

pub fn show_memory(
    selector: &str,
    root: &str,
    store: &dyn MemoryStore,
) -> Result<ShownMemory, String> {
    let files = memory_files(root, store)?;
    let candidates: Vec<&MemoryFile> = match files.iter().find(|file| file.path == selector) {
        Some(exact) => vec![exact],
        None => files
            .iter()
            .filter(|file| {
                file.project == selector
                    || format!("{}/{}", file.project, file.name) == selector
                    || file.project.contains(selector)
            })
            .collect(),
    };
    if candidates.is_empty() {
        return Err(format!("no Claude memory matches '{selector}'"));
    }
    let chosen = if candidates.len() == 1 {
        Some(candidates[0])
    } else {
        candidates
            .iter()
            .copied()
            .find(|file| file.name == "MEMORY.md")
    };
    let Some(chosen) = chosen else {
        return Err(format!(
            "memory selector '{selector}' is ambiguous; use a project/name from 'dejavu memory list --files'"
        ));
    };
    let content = store.read(&chosen.path)?;
    Ok(ShownMemory {
        file: chosen.clone(),
        content,
    })
}

/// JavaScript's default `Array.prototype.sort` order: UTF-16 code units.
pub fn utf16_cmp(a: &str, b: &str) -> Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// `a.localeCompare(b)` under ICU's root collation, for the ASCII text that
/// Claude project keys and memory paths contain: punctuation, then digits,
/// then letters with case ignored; ties go to lowercase first, left to right.
/// Other characters sort after ASCII by code point.
pub fn locale_compare(a: &str, b: &str) -> Ordering {
    const ORDER: &str = " _-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$0123456789";
    let primary = |ch: char| -> u32 {
        if ch.is_ascii_alphabetic() {
            100 + (ch.to_ascii_lowercase() as u32 - 'a' as u32)
        } else if let Some(index) = ORDER.find(ch).filter(|_| ch.is_ascii()) {
            index as u32
        } else if (ch as u32) < 32 || ch as u32 == 127 {
            0
        } else {
            1000 + ch as u32
        }
    };
    let tertiary = |ch: char| u8::from(ch.is_ascii_uppercase());
    a.chars()
        .map(primary)
        .cmp(b.chars().map(primary))
        .then_with(|| a.chars().map(tertiary).cmp(b.chars().map(tertiary)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct Fake {
        files: HashMap<String, String>,
    }

    impl MemoryStore for Fake {
        fn glob(&self, _root: &str) -> Result<Vec<String>, String> {
            Ok(self.files.keys().cloned().collect())
        }
        fn read(&self, path: &str) -> Result<String, String> {
            self.files
                .get(path)
                .cloned()
                .ok_or_else(|| "missing".into())
        }
    }

    fn fake() -> Fake {
        let files = [
            (
                "/mem/-Users-me-Development-a/memory/MEMORY.md",
                "# Index\n- production deploy notes",
            ),
            (
                "/mem/-Users-me-Development-a/memory/deploy.md",
                "Production deploy passed.\nproduction stayed healthy.",
            ),
            (
                "/mem/-Users-me-Development-b/memory/MEMORY.md",
                "# Index\n- unrelated",
            ),
        ];
        Fake {
            files: files
                .iter()
                .map(|(path, text)| (path.to_string(), text.to_string()))
                .collect(),
        }
    }

    #[test]
    fn lists_projects_and_file_counts() {
        assert_eq!(
            list_memories("/mem", &fake()).unwrap(),
            vec![
                MemoryProject {
                    project: "-Users-me-Development-a".into(),
                    path: "/mem/-Users-me-Development-a/memory".into(),
                    files: 2
                },
                MemoryProject {
                    project: "-Users-me-Development-b".into(),
                    path: "/mem/-Users-me-Development-b/memory".into(),
                    files: 1
                },
            ]
        );
    }

    fn options() -> SearchOptions {
        SearchOptions {
            limit: 20,
            snippets: 3,
            ..SearchOptions::default()
        }
    }

    #[test]
    fn searches_every_project_and_ranks_by_occurrence_count() {
        let result = search_memories("production", "/mem", &options(), &fake()).unwrap();
        let ranked: Vec<(&str, usize)> =
            result.iter().map(|m| (m.name.as_str(), m.count)).collect();
        assert_eq!(ranked, [("deploy.md", 2), ("MEMORY.md", 1)]);
        assert_eq!(
            result[0].snippets,
            ["Production deploy passed.", "production stayed healthy."]
        );
        assert!(
            search_memories("  ", "/mem", &options(), &fake())
                .unwrap_err()
                .contains("at least one term")
        );
    }

    fn fact(name: &str, description: &str, kind: &str, body: &str) -> String {
        format!(
            "---\nname: {name}\ndescription: \"{description}\"\nmetadata:\n  type: {kind}\n---\n\n{body}"
        )
    }

    fn corpus() -> Fake {
        let files = [
            (
                "/mem/-Users-me-api/memory/staging-deploy.md",
                fact(
                    "staging-deploy",
                    "Deploy the API to staging",
                    "reference",
                    "Run the pipeline.\nThe deploy waits for staging health.",
                ),
            ),
            (
                "/mem/-Users-me-web/memory/notes.md",
                fact(
                    "notes",
                    "Assorted notes",
                    "project",
                    "deploy deploy deploy deploy deploy deploy\nstaging later",
                ),
            ),
            (
                "/mem/-Users-me-web/memory/deploy-only.md",
                "deploy without the other word".to_string(),
            ),
        ];
        Fake {
            files: files
                .into_iter()
                .map(|(path, text)| (path.to_string(), text))
                .collect(),
        }
    }

    #[test]
    fn every_term_must_match_and_frontmatter_hits_outrank_body_counts() {
        let result = search_memories("Staging DEPLOY", "/mem", &options(), &corpus()).unwrap();
        let names: Vec<&str> = result.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["staging-deploy.md", "notes.md"]);
        assert_eq!(
            result[0].description.as_deref(),
            Some("Deploy the API to staging")
        );
        assert_eq!(result[0].kind.as_deref(), Some("reference"));
        // Frontmatter lines never become snippets; lines with both terms lead.
        assert_eq!(result[0].snippets, ["The deploy waits for staging health."]);
        let json = serde_json::to_value(&result[0]).unwrap();
        assert_eq!(json["type"], "reference");
    }

    #[test]
    fn project_and_type_filters_narrow_the_corpus() {
        let by_project = SearchOptions {
            project: Some("ME-WEB".into()),
            ..options()
        };
        let names: Vec<String> = search_memories("deploy", "/mem", &by_project, &corpus())
            .unwrap()
            .into_iter()
            .map(|m| m.name)
            .collect();
        assert_eq!(names, ["notes.md", "deploy-only.md"]);
        let by_type = SearchOptions {
            kind: Some("Reference".into()),
            ..options()
        };
        let names: Vec<String> = search_memories("deploy", "/mem", &by_type, &corpus())
            .unwrap()
            .into_iter()
            .map(|m| m.name)
            .collect();
        assert_eq!(names, ["staging-deploy.md"]);
    }

    #[test]
    fn frontmatter_needs_a_closing_fence() {
        let lines = ["---", "name: x", "body"];
        assert_eq!(frontmatter(&lines), Frontmatter::default());
        let lines = ["---", "name: 'x'", "type: user", "---", "body"];
        let front = frontmatter(&lines);
        assert_eq!(front.name.as_deref(), Some("x"));
        assert_eq!(front.kind.as_deref(), Some("user"));
        assert_eq!(front.body_start, 4);
    }

    #[test]
    fn shows_the_project_index_by_a_unique_project_substring() {
        let result = show_memory("Development-a", "/mem", &fake()).unwrap();
        assert_eq!(result.file.name, "MEMORY.md");
        assert!(result.content.contains("production deploy notes"));
        assert!(
            show_memory("nothing", "/mem", &fake())
                .unwrap_err()
                .contains("no Claude memory matches")
        );
        let deploy = show_memory("-Users-me-Development-a/deploy.md", "/mem", &fake()).unwrap();
        assert_eq!(deploy.file.name, "deploy.md");
    }

    #[test]
    fn json_keys_follow_the_typescript_order() {
        let shown = show_memory("Development-b", "/mem", &fake()).unwrap();
        assert_eq!(
            serde_json::to_string(&shown).unwrap(),
            r##"{"file":{"project":"-Users-me-Development-b","name":"MEMORY.md","path":"/mem/-Users-me-Development-b/memory/MEMORY.md"},"content":"# Index\n- unrelated"}"##
        );
    }

    #[test]
    fn collates_like_icu_root() {
        let mut keys = vec![
            "-Users-b",
            "-Users-B",
            "-Users-a",
            "-Users-_x",
            "-Users-.x",
            "-Users-1",
            "-Users-ab",
            "-Users-a-b",
            "-users-a",
            "-Users-z",
        ];
        keys.sort_by(|a, b| locale_compare(a, b));
        assert_eq!(
            keys,
            [
                "-Users-_x",
                "-Users-.x",
                "-Users-1",
                "-users-a",
                "-Users-a",
                "-Users-a-b",
                "-Users-ab",
                "-Users-b",
                "-Users-B",
                "-Users-z"
            ]
        );
        assert_eq!(locale_compare("Ab", "aB"), Ordering::Greater);
        assert_eq!(locale_compare("a1", "a-1"), Ordering::Greater);
    }

    #[test]
    fn claude_root_comes_from_the_config_dir_or_home() {
        let sep = std::path::MAIN_SEPARATOR;
        assert_eq!(
            claude_projects_root(None, PathBuf::from("/home/me")),
            format!("/home/me{sep}.claude{sep}projects")
        );
        // `resolve` normalizes the configured directory; a Windows path needs a drive.
        #[cfg(unix)]
        {
            let root = claude_projects_root(
                Some(PathBuf::from("/etc/./claude/")),
                PathBuf::from("/home/me"),
            );
            assert_eq!(root, "/etc/claude/projects");
        }
    }

    #[cfg(unix)]
    #[test]
    fn disk_glob_skips_dot_entries_and_symlinks() {
        let root = std::env::temp_dir().join(format!("dejavu-memory-glob-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["a/memory", ".h/memory", "real/memory", "b/memory/dir.md"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        for (file, text) in [
            ("a/memory/x.md", "x"),
            ("a/memory/.y.md", "y"),
            (".h/memory/z.md", "z"),
            ("real/memory/r.md", "r"),
        ] {
            std::fs::write(root.join(file), text).unwrap();
        }
        std::fs::write(root.join("b/memory/bom.md"), "\u{feff}bom").unwrap();
        std::os::unix::fs::symlink("../../a/memory/x.md", root.join("b/memory/link.md")).unwrap();
        std::os::unix::fs::symlink("real", root.join("c")).unwrap();
        let root_text = root.to_string_lossy().into_owned();
        let found: Vec<String> = Disk
            .glob(&root_text)
            .unwrap()
            .iter()
            .map(|path| path[root_text.len() + 1..].to_string())
            .collect();
        assert_eq!(
            found,
            ["a/memory/x.md", "b/memory/bom.md", "real/memory/r.md"]
        );
        assert_eq!(
            Disk.read(&format!("{root_text}/b/memory/bom.md")).unwrap(),
            "bom"
        );
        assert_eq!(
            Disk.glob(&format!("{root_text}/missing")).unwrap_err(),
            format!("ENOENT: no such file or directory, open '{root_text}/missing'")
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
