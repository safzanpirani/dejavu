//! User rules for memory search and embedding, read from
//! `~/.config/dejavu/memory-rules` (`DEJAVU_MEMORY_RULES` moves it). Each line
//! is `<action> <pattern>`; `#` starts a comment. The last matching line wins.
//!
//! - `off`: never searched or embedded; stored vectors are kept, so removing
//!   the rule re-embeds only passages edited meanwhile.
//! - `skip`: searched by keyword only; its vectors are dropped and its text
//!   is never sent.
//! - `freeze`: keeps the vectors it has and is not re-embedded when edited.
//!   A file that becomes gated still loses its vectors.
//!
//! A pattern is a path glob (`~/` expands to the home directory, `*` matches
//! within one path segment, `**` across segments, a trailing `/` covers the
//! whole directory) or `wiki:<slug glob>` for agent wiki pages.

use std::path::PathBuf;

use crate::memory::{MemoryFile, Source};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Off,
    Skip,
    Freeze,
}

impl Action {
    pub fn name(self) -> &'static str {
        match self {
            Action::Off => "off",
            Action::Skip => "skip",
            Action::Freeze => "freeze",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    Path(String),
    WikiSlug(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rules {
    entries: Vec<(Action, Target)>,
}

/// The rules file: `$DEJAVU_MEMORY_RULES`, else `~/.config/dejavu/memory-rules`.
pub fn rules_path() -> PathBuf {
    std::env::var_os("DEJAVU_MEMORY_RULES")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config/dejavu/memory-rules"))
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn slashes(text: &str) -> String {
    text.replace('\\', "/")
}

impl Rules {
    /// The rules in the rules file. A missing file has none; an unreadable or
    /// malformed one is an error, because a `skip` line may be what keeps a
    /// file from being sent.
    pub fn load() -> Result<Rules, String> {
        let path = rules_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => Rules::parse(&text, &home().to_string_lossy())
                .map_err(|error| format!("{}: {error}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Rules::default()),
            Err(error) => Err(format!("{}: {error}", path.display())),
        }
    }

    pub fn parse(text: &str, home: &str) -> Result<Rules, String> {
        let home = slashes(home).trim_end_matches('/').to_string();
        let mut entries = Vec::new();
        for (number, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let (action, pattern) = line
                .split_once(char::is_whitespace)
                .map(|(action, pattern)| (action, pattern.trim()))
                .ok_or_else(|| {
                    format!(
                        "line {}: expected '<off|skip|freeze> <pattern>'",
                        number + 1
                    )
                })?;
            let action = match action {
                "off" => Action::Off,
                "skip" => Action::Skip,
                "freeze" => Action::Freeze,
                other => {
                    return Err(format!(
                        "line {}: unknown action '{other}' (off|skip|freeze)",
                        number + 1
                    ));
                }
            };
            let target = if let Some(slug) = pattern.strip_prefix("wiki:") {
                Target::WikiSlug(slug.to_lowercase())
            } else {
                let mut path = slashes(pattern);
                if let Some(rest) = path.strip_prefix("~/") {
                    path = format!("{home}/{rest}");
                }
                if !(path.starts_with('/') || path.get(1..3) == Some(":/")) {
                    return Err(format!(
                        "line {}: a path pattern must be absolute or start with ~/ ('{pattern}')",
                        number + 1
                    ));
                }
                if path.ends_with('/') {
                    path.push_str("**");
                }
                Target::Path(path.to_lowercase())
            };
            entries.push((action, target));
        }
        Ok(Rules { entries })
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The action of the last rule matching `path`, if any. Matching ignores
    /// case, as the default macOS and Windows file systems do, so a rule
    /// cannot be bypassed by spelling a path differently.
    pub fn action(&self, path: &str, source: Source) -> Option<Action> {
        let path = slashes(path).to_lowercase();
        let slug = (source == Source::Wiki).then(|| {
            let name = path.rsplit('/').next().unwrap_or(&path);
            name.strip_suffix(".md").unwrap_or(name).to_string()
        });
        self.entries
            .iter()
            .rev()
            .find(|(_, target)| match target {
                Target::Path(pattern) => glob_match(pattern, &path),
                Target::WikiSlug(pattern) => slug
                    .as_deref()
                    .is_some_and(|slug| glob_match(pattern, slug)),
            })
            .map(|(action, _)| *action)
    }

    pub fn action_for(&self, file: &MemoryFile) -> Option<Action> {
        self.action(&file.path, file.source)
    }
}

/// `*` and `?` stay within a `/`-separated segment; `**` crosses segments,
/// and a whole-segment `/**/` also matches a single `/`.
fn glob_match(pattern: &str, text: &str) -> bool {
    fn go(p: &[u8], i: usize, t: &[u8], j: usize) -> bool {
        if i == p.len() {
            return j == t.len();
        }
        if p[i..].starts_with(b"**") {
            let next = i + 2;
            let whole_segment = (i == 0 || p[i - 1] == b'/') && p.get(next) == Some(&b'/');
            return (whole_segment && go(p, next + 1, t, j))
                || (j..=t.len()).any(|k| go(p, next, t, k));
        }
        match p[i] {
            b'*' => (j..=t.len())
                .take_while(|&k| k == j || t[k - 1] != b'/')
                .any(|k| go(p, i + 1, t, k)),
            b'?' => j < t.len() && t[j] != b'/' && go(p, i + 1, t, j + 1),
            ch => j < t.len() && t[j] == ch && go(p, i + 1, t, j + 1),
        }
    }
    go(pattern.as_bytes(), 0, text.as_bytes(), 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(text: &str) -> Rules {
        Rules::parse(text, "/home/me").unwrap()
    }

    #[test]
    fn the_last_matching_rule_wins() {
        let rules = rules(
            "off ~/.claude/projects/   # old memory\n\
             freeze ~/.claude/projects/-home-me-keep/memory/*.md\n",
        );
        let old = "/home/me/.claude/projects/-home-me-x/memory/note.md";
        let kept = "/home/me/.claude/projects/-home-me-keep/memory/note.md";
        assert_eq!(rules.action(old, Source::Memory), Some(Action::Off));
        assert_eq!(rules.action(kept, Source::Memory), Some(Action::Freeze));
        assert_eq!(
            rules.action("/home/me/.local/share/agent-wiki/pages/a.md", Source::Wiki),
            None
        );
    }

    #[test]
    fn wiki_patterns_match_page_slugs_only() {
        let rules = rules("skip wiki:gta6-*\n");
        let page = "/w/pages/gta6-dump-findings.md";
        assert_eq!(rules.action(page, Source::Wiki), Some(Action::Skip));
        assert_eq!(rules.action(page, Source::Memory), None);
        assert_eq!(rules.action("/w/pages/host-main.md", Source::Wiki), None);
    }

    #[test]
    fn a_single_star_stays_in_one_segment() {
        let rules = rules("skip /a/*/memory/x.md\nfreeze /b/**/x.md\n");
        assert_eq!(
            rules.action("/a/p/memory/x.md", Source::Memory),
            Some(Action::Skip)
        );
        assert_eq!(rules.action("/a/p/q/memory/x.md", Source::Memory), None);
        assert_eq!(
            rules.action("/b/x.md", Source::Memory),
            Some(Action::Freeze)
        );
        assert_eq!(
            rules.action("/b/c/d/x.md", Source::Memory),
            Some(Action::Freeze)
        );
        assert_eq!(rules.action("/bx.md", Source::Memory), None);
    }

    #[test]
    fn windows_paths_and_other_spellings_match() {
        let rules = Rules::parse("off ~/.claude/projects/\n", r"C:\Users\me").unwrap();
        let path = r"c:\users\ME\.Claude\projects\p\memory\n.md";
        assert_eq!(rules.action(path, Source::Memory), Some(Action::Off));
    }

    #[test]
    fn a_double_star_inside_a_segment_keeps_its_slashes() {
        let rules = rules("skip /repo/proj**/stuff/*.md\n");
        assert_eq!(
            rules.action("/repo/projx/stuff/a.md", Source::Memory),
            Some(Action::Skip)
        );
        assert_eq!(rules.action("/repo/projstuff/a.md", Source::Memory), None);
    }

    #[test]
    fn malformed_lines_are_errors() {
        assert!(
            Rules::parse("hide /x\n", "/h")
                .unwrap_err()
                .contains("unknown action")
        );
        assert!(Rules::parse("off\n", "/h").unwrap_err().contains("line 1"));
        assert!(
            Rules::parse("off relative/x\n", "/h")
                .unwrap_err()
                .contains("absolute")
        );
    }
}
