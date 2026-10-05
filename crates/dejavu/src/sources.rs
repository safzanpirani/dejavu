//! Store discovery (`source-registry.ts`): where each agent keeps its
//! transcripts, from its environment variables and the home directory.

use crate::paths::{is_absolute, join, resolve};
use crate::types::{SourceSelector, StoreKind, TranscriptSource, TranscriptStore};
use std::sync::OnceLock;

/// `parseSource(value)`: `all`, `claude`, `codex`, `pi`, `opencode`, `droid`, or `agy`.
pub fn parse_source(value: &str) -> Result<SourceSelector, String> {
    if value == "all" {
        return Ok(SourceSelector::All);
    }
    TranscriptSource::from_name(value)
        .map(SourceSelector::Only)
        .ok_or_else(|| {
            format!(
                "source must be one of: all, claude, codex, pi, opencode, droid, agy (got '{value}')"
            )
        })
}

/// An environment lookup (`StoreEnv`): `None` for an unset variable.
pub type StoreEnv<'a> = &'a dyn Fn(&str) -> Option<String>;

/// The process environment as a [`StoreEnv`]. Non-UTF-8 values read as unset.
pub fn process_env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

/// `os.homedir()`: `$HOME`, else the account's home directory.
pub fn home_dir() -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => home,
        _ => std::env::home_dir()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default(),
    }
}

/// The transcript roots each agent would use (`TranscriptStoreRoots`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptStoreRoots {
    pub claude: String,
    pub codex: String,
    /// Codex moves archived rollouts here, flat, out of `codex`.
    pub codex_archive: String,
    pub pi: String,
    pub opencode: Vec<String>,
    pub droid: String,
    /// The Antigravity CLI's conversation directories.
    pub agy: String,
}

impl TranscriptStoreRoots {
    /// The JSONL root of `source`; `None` for OpenCode.
    pub fn jsonl_root(&self, source: TranscriptSource) -> Option<&str> {
        match source {
            TranscriptSource::Claude => Some(&self.claude),
            TranscriptSource::Codex => Some(&self.codex),
            TranscriptSource::Pi => Some(&self.pi),
            TranscriptSource::Opencode => None,
            TranscriptSource::Droid => Some(&self.droid),
            TranscriptSource::Agy => Some(&self.agy),
        }
    }
}

/// The roots from the process environment and home directory, computed once.
pub fn default_roots() -> &'static TranscriptStoreRoots {
    static ROOTS: OnceLock<TranscriptStoreRoots> = OnceLock::new();
    ROOTS.get_or_init(|| transcript_store_roots(&process_env, &home_dir()))
}

/// `transcriptStoreRoots(env, home)`: resolves each agent's store the way the
/// agent does. A set (non-empty) variable replaces the home-directory default:
/// Claude `$CLAUDE_CONFIG_DIR/projects`, Codex `$CODEX_HOME/sessions`, Pi
/// `$PI_CODING_AGENT_DIR/sessions` (a leading `~` expands), OpenCode `$OPENCODE_DB`
/// (relative to the data dir; `:memory:` disables it) or `$XDG_DATA_HOME/opencode/*.db`,
/// Droid `$FACTORY_HOME_OVERRIDE/.factory/sessions` (the variable replaces the
/// home directory, not the Factory directory), and agy `~/.gemini/antigravity-cli/brain`
/// (agy reads no variable for it).
pub fn transcript_store_roots(env: StoreEnv, home: &str) -> TranscriptStoreRoots {
    let set = |key: &str| env(key).filter(|value| !value.is_empty());
    let configured =
        |value: Option<String>, fallback: String| value.map_or(fallback, |value| resolve(&value));
    // Pi expands a leading tilde itself; the other agents take the path as given.
    let pi_dir = set("PI_CODING_AGENT_DIR").map(|dir| match dir.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with(['/', '\\']) => format!("{home}{rest}"),
        _ => dir,
    });
    let opencode_data = join(&[
        &configured(set("XDG_DATA_HOME"), join(&[home, ".local", "share"])),
        "opencode",
    ]);
    let opencode = match env("OPENCODE_DB").as_deref() {
        Some(":memory:") => Vec::new(),
        Some(db) if !db.is_empty() => vec![if is_absolute(db) {
            db.to_string()
        } else {
            join(&[&opencode_data, db])
        }],
        _ => ["opencode.db", "opencode-next.db", "opencode-local.db"]
            .iter()
            .map(|name| join(&[&opencode_data, name]))
            .collect(),
    };
    let codex_home = configured(set("CODEX_HOME"), join(&[home, ".codex"]));
    TranscriptStoreRoots {
        claude: join(&[
            &configured(set("CLAUDE_CONFIG_DIR"), join(&[home, ".claude"])),
            "projects",
        ]),
        codex: join(&[&codex_home, "sessions"]),
        codex_archive: join(&[&codex_home, "archived_sessions"]),
        pi: join(&[
            &configured(pi_dir, join(&[home, ".pi", "agent"])),
            "sessions",
        ]),
        opencode,
        droid: join(&[
            &configured(set("FACTORY_HOME_OVERRIDE"), home.to_string()),
            ".factory",
            "sessions",
        ]),
        agy: join(&[home, ".gemini", "antigravity-cli", "brain"]),
    }
}

/// `discoverTranscriptStores()` for the process environment and home directory.
pub fn discover_stores(selector: SourceSelector) -> Vec<TranscriptStore> {
    discover_transcript_stores(selector, &home_dir(), &process_env)
}

/// `discoverTranscriptStores(selector, home, env)`: the selected stores that
/// exist, in order Claude, Codex (live, then archived), Pi (then sibling Pi profiles under `~/.pi`
/// when `PI_CODING_AGENT_DIR` is unset), OpenCode, Droid, agy.
pub fn discover_transcript_stores(
    selector: SourceSelector,
    home: &str,
    env: StoreEnv,
) -> Vec<TranscriptStore> {
    let roots = transcript_store_roots(env, home);
    let jsonl = |source, path: String| TranscriptStore {
        source,
        kind: StoreKind::Jsonl,
        path,
    };
    let mut candidates = vec![
        jsonl(TranscriptSource::Claude, roots.claude.clone()),
        jsonl(TranscriptSource::Codex, roots.codex.clone()),
        jsonl(TranscriptSource::Codex, roots.codex_archive.clone()),
        jsonl(TranscriptSource::Pi, roots.pi.clone()),
    ];
    if env("PI_CODING_AGENT_DIR").is_none_or(|dir| dir.is_empty())
        && selector.matches(TranscriptSource::Pi)
    {
        candidates.extend(
            pi_profile_dirs(home, &roots.pi)
                .into_iter()
                .map(|path| jsonl(TranscriptSource::Pi, path)),
        );
    }
    candidates.extend(roots.opencode.into_iter().map(|path| TranscriptStore {
        source: TranscriptSource::Opencode,
        kind: StoreKind::Sqlite,
        path,
    }));
    candidates.push(jsonl(TranscriptSource::Droid, roots.droid));
    candidates.push(jsonl(TranscriptSource::Agy, roots.agy));
    candidates
        .into_iter()
        .filter(|store| selector.matches(store.source) && std::fs::metadata(&store.path).is_ok())
        .collect()
}

/// Session directories of Pi profiles kept beside the default `~/.pi/agent`,
/// sorted as JavaScript sorts strings (by UTF-16 code units). Symlinked
/// profile directories are skipped, as `Dirent.isDirectory()` skipped them.
fn pi_profile_dirs(home: &str, primary: &str) -> Vec<String> {
    let pi_home = join(&[home, ".pi"]);
    let Ok(entries) = std::fs::read_dir(&pi_home) else {
        return Vec::new();
    };
    let mut dirs: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| join(&[&pi_home, &entry.file_name().to_string_lossy(), "sessions"]))
        .filter(|path| path != primary)
        .collect();
    dirs.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
    dirs
}

/// `sourceFromLocator(locator, roots)`: `opencode://` locators, paths under a
/// configured root, then the default `.claude/projects/`, `.codex/sessions/`,
/// `.codex/archived_sessions/`, `.pi/<profile>/sessions/`, `.factory/sessions/`, and
/// `antigravity-cli/brain/` layouts.
pub fn source_from_locator(
    locator: &str,
    roots: &TranscriptStoreRoots,
) -> Result<TranscriptSource, String> {
    if locator.starts_with("opencode://") {
        return Ok(TranscriptSource::Opencode);
    }
    for source in [
        TranscriptSource::Claude,
        TranscriptSource::Codex,
        TranscriptSource::Pi,
        TranscriptSource::Droid,
        TranscriptSource::Agy,
    ] {
        let root = roots.jsonl_root(source).unwrap_or_default();
        if is_under_root(locator, root) {
            return Ok(source);
        }
    }
    if is_under_root(locator, &roots.codex_archive) {
        return Ok(TranscriptSource::Codex);
    }
    if has_segments(locator, ".claude", "projects") {
        return Ok(TranscriptSource::Claude);
    }
    if has_segments(locator, ".codex", "sessions")
        || has_segments(locator, ".codex", "archived_sessions")
    {
        return Ok(TranscriptSource::Codex);
    }
    if has_pi_profile_segment(locator) {
        return Ok(TranscriptSource::Pi);
    }
    if has_segments(locator, ".factory", "sessions") {
        return Ok(TranscriptSource::Droid);
    }
    if has_segments(locator, "antigravity-cli", "brain") {
        return Ok(TranscriptSource::Agy);
    }
    Err(format!(
        "cannot determine transcript source from locator: {locator} (use a transcript path or opencode:// locator from search results)"
    ))
}

fn is_under_root(locator: &str, root: &str) -> bool {
    if has_root_prefix(locator, root, cfg!(windows)) {
        return true;
    }
    // Windows can spell the same directory with long names or 8.3 aliases.
    // Resolve only for comparison; keep the caller's locator for reads/output.
    #[cfg(windows)]
    if let (Ok(path), Ok(root)) = (
        std::fs::canonicalize(locator.replace('/', "\\")),
        std::fs::canonicalize(root.replace('/', "\\")),
    ) {
        return path != root && path.starts_with(root);
    }
    false
}

fn has_root_prefix(locator: &str, root: &str, windows: bool) -> bool {
    locator.len() > root.len()
        && locator.bytes().zip(root.bytes()).all(|(a, b)| {
            a == b || (windows && matches!(a, b'/' | b'\\') && matches!(b, b'/' | b'\\'))
        })
        && matches!(locator.as_bytes()[root.len()], b'/' | b'\\')
}

/// `/[/\\]<dir>[/\\]<sub>[/\\]/`, so native Windows paths match too.
fn has_segments(locator: &str, dir: &str, sub: &str) -> bool {
    let b = locator.as_bytes();
    let sep = |i: usize| b.get(i).is_some_and(|&c| c == b'/' || c == b'\\');
    locator.match_indices(dir).any(|(i, _)| {
        let after = i + dir.len();
        i > 0
            && sep(i - 1)
            && sep(after)
            && locator[after + 1..].starts_with(sub)
            && sep(after + 1 + sub.len())
    })
}

/// `/[/\\]\.pi[/\\][^/\\]+[/\\]sessions[/\\]/`
fn has_pi_profile_segment(locator: &str) -> bool {
    let b = locator.as_bytes();
    let sep = |i: usize| b.get(i).is_some_and(|&c| c == b'/' || c == b'\\');
    (0..b.len()).any(|i| {
        if !(sep(i) && b[i + 1..].starts_with(b".pi") && sep(i + 4)) {
            return false;
        }
        let start = i + 5;
        let end = (start..b.len()).find(|&j| sep(j)).unwrap_or(b.len());
        end > start && end < b.len() && b[end + 1..].starts_with(b"sessions") && sep(end + 9)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    fn opencode_defaults(dir: &str) -> Vec<String> {
        ["opencode.db", "opencode-next.db", "opencode-local.db"]
            .iter()
            .map(|name| format!("{dir}/opencode/{name}"))
            .collect()
    }

    const HOME: &str = "/home/owner";

    #[test]
    fn configured_root_prefix_accepts_mixed_windows_separators() {
        for root in [
            r"C:\Users\RUNNER~1\AppData\Local\Temp\fixture/projects",
            "C:/Users/RUNNER~1/AppData/Local/Temp/fixture/projects",
        ] {
            for path in [
                r"C:\Users\RUNNER~1\AppData\Local\Temp\fixture\projects/demo\fixture.jsonl",
                "C:/Users/RUNNER~1/AppData/Local/Temp/fixture/projects/demo/fixture.jsonl",
            ] {
                assert!(has_root_prefix(path, root, true), "{path} under {root}");
            }
            assert!(!has_root_prefix(root, root, true));
            assert!(!has_root_prefix(
                &format!("{root}-other/a.jsonl"),
                root,
                true
            ));
        }
        assert!(has_root_prefix(
            r"\\server\share\projects/demo\a.jsonl",
            r"\\server\share/projects",
            true
        ));
        // Backslashes stay literal on Unix, including in configured roots.
        assert!(!has_root_prefix(
            "/tmp/a/b/projects/x.jsonl",
            r"/tmp/a\b/projects",
            false
        ));
        assert!(has_root_prefix(
            r"/tmp/a\b/projects/x.jsonl",
            r"/tmp/a\b/projects",
            false
        ));
    }

    #[test]
    fn defaults_to_home_directory_stores_when_no_variable_is_set() {
        assert_eq!(
            transcript_store_roots(&env(&[]), HOME),
            TranscriptStoreRoots {
                claude: "/home/owner/.claude/projects".into(),
                codex: "/home/owner/.codex/sessions".into(),
                codex_archive: "/home/owner/.codex/archived_sessions".into(),
                pi: "/home/owner/.pi/agent/sessions".into(),
                opencode: opencode_defaults("/home/owner/.local/share"),
                droid: "/home/owner/.factory/sessions".into(),
                agy: "/home/owner/.gemini/antigravity-cli/brain".into(),
            }
        );
        assert_eq!(
            transcript_store_roots(
                &env(&[("CLAUDE_CONFIG_DIR", ""), ("XDG_DATA_HOME", "")]),
                HOME
            )
            .claude,
            "/home/owner/.claude/projects"
        );
    }

    #[test]
    fn each_set_variable_replaces_the_default_store() {
        let roots = transcript_store_roots(
            &env(&[
                ("CLAUDE_CONFIG_DIR", "/home/owner/.claude-rakhi"),
                ("CODEX_HOME", "/home/owner/.codex-rakhi"),
                ("PI_CODING_AGENT_DIR", "~/.pi-rakhi"),
                ("XDG_DATA_HOME", "/home/owner/.rakhi-data"),
                ("FACTORY_HOME_OVERRIDE", "/home/owner/rakhi"),
            ]),
            HOME,
        );
        assert_eq!(
            roots,
            TranscriptStoreRoots {
                claude: "/home/owner/.claude-rakhi/projects".into(),
                codex: "/home/owner/.codex-rakhi/sessions".into(),
                codex_archive: "/home/owner/.codex-rakhi/archived_sessions".into(),
                pi: "/home/owner/.pi-rakhi/sessions".into(),
                opencode: opencode_defaults("/home/owner/.rakhi-data"),
                droid: "/home/owner/rakhi/.factory/sessions".into(),
                agy: "/home/owner/.gemini/antigravity-cli/brain".into(),
            }
        );
        assert_eq!(
            transcript_store_roots(
                &env(&[("XDG_DATA_HOME", "/data"), ("OPENCODE_DB", "custom.db")]),
                HOME
            )
            .opencode,
            ["/data/opencode/custom.db"]
        );
        assert_eq!(
            transcript_store_roots(&env(&[("OPENCODE_DB", "/abs/x.db")]), HOME).opencode,
            ["/abs/x.db"]
        );
        assert!(
            transcript_store_roots(&env(&[("OPENCODE_DB", ":memory:")]), HOME)
                .opencode
                .is_empty()
        );
        // `~user` is not a home-directory reference.
        assert_eq!(
            transcript_store_roots(&env(&[("PI_CODING_AGENT_DIR", "/p/./x/")]), HOME).pi,
            "/p/x/sessions"
        );
        // It resolves against the current directory, joined with `\` on Windows.
        let pi = transcript_store_roots(&env(&[("PI_CODING_AGENT_DIR", "~x")]), HOME).pi;
        assert!(
            pi.ends_with("/~x/sessions") || (cfg!(windows) && pi.ends_with("\\~x/sessions")),
            "{pi}"
        );
    }

    fn temp_root(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!("dejavu-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_str().unwrap().to_string()
    }

    fn paths(stores: Vec<TranscriptStore>) -> Vec<String> {
        stores.into_iter().map(|store| store.path).collect()
    }

    #[test]
    fn discovery_returns_the_overridden_claude_and_opencode_stores() {
        let root = temp_root("store-env");
        for dir in [
            ".claude/projects",
            ".claude-rakhi/projects",
            ".local/share/opencode",
            ".rakhi-data/opencode",
        ] {
            std::fs::create_dir_all(format!("{root}/{dir}")).unwrap();
        }
        std::fs::write(format!("{root}/.local/share/opencode/opencode.db"), b"").unwrap();
        std::fs::write(format!("{root}/.rakhi-data/opencode/opencode.db"), b"").unwrap();
        let rakhi = env(&[
            ("CLAUDE_CONFIG_DIR", &format!("{root}/.claude-rakhi")),
            ("XDG_DATA_HOME", &format!("{root}/.rakhi-data")),
        ]);
        assert_eq!(
            paths(discover_transcript_stores(
                SourceSelector::All,
                &root,
                &rakhi
            )),
            [
                format!("{root}/.claude-rakhi/projects"),
                format!("{root}/.rakhi-data/opencode/opencode.db")
            ]
        );
        let stores = discover_transcript_stores(SourceSelector::All, &root, &env(&[]));
        assert_eq!(stores[1].kind, StoreKind::Sqlite);
        assert_eq!(
            paths(stores),
            [
                format!("{root}/.claude/projects"),
                format!("{root}/.local/share/opencode/opencode.db")
            ]
        );
        assert_eq!(
            paths(discover_transcript_stores(
                SourceSelector::Only(TranscriptSource::Opencode),
                &root,
                &env(&[])
            )),
            [format!("{root}/.local/share/opencode/opencode.db")]
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn discovery_adds_sibling_pi_profiles_only_when_pi_coding_agent_dir_is_unset() {
        let root = temp_root("pi-profiles");
        for profile in ["agent", "juna", "zeta"] {
            std::fs::create_dir_all(format!("{root}/.pi/{profile}/sessions")).unwrap();
        }
        std::fs::create_dir_all(format!("{root}/.pi/cache")).unwrap();
        std::fs::write(format!("{root}/.pi/file"), b"").unwrap();
        let pi = SourceSelector::Only(TranscriptSource::Pi);
        assert_eq!(
            paths(discover_transcript_stores(pi, &root, &env(&[]))),
            [
                format!("{root}/.pi/agent/sessions"),
                format!("{root}/.pi/juna/sessions"),
                format!("{root}/.pi/zeta/sessions")
            ]
        );
        assert_eq!(
            paths(discover_transcript_stores(
                pi,
                &root,
                &env(&[("PI_CODING_AGENT_DIR", &format!("{root}/.pi/juna"))])
            )),
            [format!("{root}/.pi/juna/sessions")]
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn discovery_lists_archived_codex_rollouts_after_live_ones() {
        let root = temp_root("codex-archive");
        for dir in [".codex/sessions", ".codex/archived_sessions"] {
            std::fs::create_dir_all(format!("{root}/{dir}")).unwrap();
        }
        let stores = discover_transcript_stores(
            SourceSelector::Only(TranscriptSource::Codex),
            &root,
            &env(&[]),
        );
        assert!(
            stores
                .iter()
                .all(|store| store.source == TranscriptSource::Codex)
        );
        assert_eq!(
            paths(stores),
            [
                format!("{root}/.codex/sessions"),
                format!("{root}/.codex/archived_sessions")
            ]
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn discovery_finds_the_droid_store_under_home_or_factory_home_override() {
        let root = temp_root("droid-store");
        for dir in [
            ".factory/sessions",
            "other/.factory/sessions",
            ".claude/projects",
        ] {
            std::fs::create_dir_all(format!("{root}/{dir}")).unwrap();
        }
        assert_eq!(
            paths(discover_transcript_stores(
                SourceSelector::All,
                &root,
                &env(&[])
            )),
            [
                format!("{root}/.claude/projects"),
                format!("{root}/.factory/sessions")
            ]
        );
        let droid = SourceSelector::Only(TranscriptSource::Droid);
        let stores = discover_transcript_stores(
            droid,
            &root,
            &env(&[("FACTORY_HOME_OVERRIDE", &format!("{root}/other"))]),
        );
        assert_eq!(stores[0].kind, StoreKind::Jsonl);
        assert_eq!(paths(stores), [format!("{root}/other/.factory/sessions")]);
        // An empty override falls back to the home directory; a missing store is not listed.
        assert_eq!(
            paths(discover_transcript_stores(
                droid,
                &root,
                &env(&[("FACTORY_HOME_OVERRIDE", "")])
            )),
            [format!("{root}/.factory/sessions")]
        );
        assert!(
            discover_transcript_stores(
                droid,
                &root,
                &env(&[("FACTORY_HOME_OVERRIDE", &format!("{root}/none"))])
            )
            .is_empty()
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn locators_resolve_to_sources() {
        let roots = transcript_store_roots(&env(&[]), HOME);
        assert_eq!(
            source_from_locator("/home/owner/.pi/juna/sessions/--work-app--/s.jsonl", &roots),
            Ok(TranscriptSource::Pi)
        );
        assert_eq!(
            source_from_locator("opencode:///x.db#s", &roots),
            Ok(TranscriptSource::Opencode)
        );
        assert_eq!(
            source_from_locator("/elsewhere/.codex/sessions/2026/a.jsonl", &roots),
            Ok(TranscriptSource::Codex)
        );
        assert_eq!(
            source_from_locator(
                "/elsewhere/.codex/archived_sessions/rollout-a.jsonl",
                &roots
            ),
            Ok(TranscriptSource::Codex)
        );
        assert_eq!(
            source_from_locator("/elsewhere/.claude/projects/-x/a.jsonl", &roots),
            Ok(TranscriptSource::Claude)
        );
        assert!(source_from_locator("/elsewhere/.pi//sessions/a.jsonl", &roots).is_err());
        assert_eq!(
            source_from_locator(
                "/home/owner/.gemini/antigravity-cli/brain/c1/.system_generated/logs/transcript.jsonl",
                &roots
            ),
            Ok(TranscriptSource::Agy)
        );
        assert_eq!(
            source_from_locator(
                "C:\\Users\\me\\.gemini\\antigravity-cli\\brain\\c1\\.system_generated\\logs\\transcript.jsonl",
                &roots
            ),
            Ok(TranscriptSource::Agy)
        );
        assert_eq!(
            source_from_locator("/home/owner/.factory/sessions/-work-app/s.jsonl", &roots),
            Ok(TranscriptSource::Droid)
        );
        assert_eq!(
            source_from_locator("/elsewhere/.factory/sessions/-x/s.jsonl", &roots),
            Ok(TranscriptSource::Droid)
        );
        assert!(source_from_locator("/home/owner/.factory/auth.json", &roots).is_err());
        for (path, source) in [
            (
                r"C:\Users\dev\.claude\projects\-x\a.jsonl",
                TranscriptSource::Claude,
            ),
            (
                r"C:\Users\dev\.codex\sessions\2026\a.jsonl",
                TranscriptSource::Codex,
            ),
            (
                r"C:\Users\dev\.factory\sessions\-x\s.jsonl",
                TranscriptSource::Droid,
            ),
            (
                r"C:\tmp\r\.claude/projects/demo\f.jsonl",
                TranscriptSource::Claude,
            ),
        ] {
            assert_eq!(source_from_locator(path, &roots), Ok(source), "{path}");
        }
        assert!(source_from_locator(r"C:\x.claude\projects\a.jsonl", &roots).is_err());
        assert_eq!(
            source_from_locator("/tmp/x.jsonl", &roots).unwrap_err(),
            "cannot determine transcript source from locator: /tmp/x.jsonl (use a transcript path or opencode:// locator from search results)"
        );
        let roots = transcript_store_roots(
            &env(&[
                ("CLAUDE_CONFIG_DIR", "/home/owner/.claude-rakhi"),
                ("CODEX_HOME", "/srv/cx"),
                ("FACTORY_HOME_OVERRIDE", "/srv/fh"),
            ]),
            HOME,
        );
        assert_eq!(
            source_from_locator("/srv/fh/.factory/sessions/-work/s.jsonl", &roots),
            Ok(TranscriptSource::Droid)
        );
        assert_eq!(
            source_from_locator("/srv/cx/archived_sessions/rollout-a.jsonl", &roots),
            Ok(TranscriptSource::Codex)
        );
        assert_eq!(
            source_from_locator(
                "/home/owner/.claude-rakhi/projects/-work-app/s.jsonl",
                &roots
            ),
            Ok(TranscriptSource::Claude)
        );
        assert_eq!(
            source_from_locator("/srv/cx/sessions/2026/09/20/rollout-x.jsonl", &roots),
            Ok(TranscriptSource::Codex)
        );
        assert_eq!(
            crate::paths::project_from_claude_path_with(
                "/home/owner/.claude-rakhi/projects/-home-owner-work-app/s.jsonl",
                HOME,
                &roots.claude
            ),
            "work/app"
        );
    }

    #[test]
    fn parses_source_selectors() {
        assert_eq!(parse_source("all"), Ok(SourceSelector::All));
        assert_eq!(
            parse_source("codex"),
            Ok(SourceSelector::Only(TranscriptSource::Codex))
        );
        assert_eq!(
            parse_source("gemini").unwrap_err(),
            "source must be one of: all, claude, codex, pi, opencode, droid, agy (got 'gemini')"
        );
        assert_eq!(
            parse_source("droid"),
            Ok(SourceSelector::Only(TranscriptSource::Droid))
        );
        assert_eq!(
            parse_source("agy"),
            Ok(SourceSelector::Only(TranscriptSource::Agy))
        );
    }
}
