//! Literal transcript search (`core.ts` `searchSessions`): the transcript index
//! when it is usable, else direct file scans and OpenCode queries.

use crate::DEFAULT_MAX_PARALLEL;
use crate::index::{IndexReader, TieBreak, default_index_path, refresh_transcript_index};
use crate::js;
use crate::opencode::search_opencode_store;
use crate::paths::{
    compact_home, date_from_path, js_lower, project_from_transcript_path,
    project_from_transcript_text, snippet_around,
};
use crate::pool::map_pool;
use crate::reader::{extract_visible_message, load_recall_messages};
use crate::scan::{self, FileMatchCount, locale_compare};
use crate::sources::discover_stores;
use crate::types::{
    RecallMessage, SourceSelector, StoreDiagnostic, StoreKind, StoreSearchMatch, TranscriptSnippet,
    TranscriptSource, TranscriptStore,
};
use serde::Serialize;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::time::Instant;

/// Where search and find get stores, file matches, and transcript text.
/// [`Disk`] reads the real stores; tests substitute fixtures.
pub trait Backend: Sync {
    fn discover_stores(&self, selector: SourceSelector) -> Vec<TranscriptStore> {
        discover_stores(selector)
    }

    /// The transcript index to use, or `None` to scan stores directly.
    fn index_path(&self) -> Option<String> {
        Some(default_index_path())
    }

    /// `searchFileCounts`: matching-line counts for each literal, one list per
    /// literal, for every file under a JSONL store root.
    fn count_files(
        &self,
        queries: &[&str],
        root: &str,
        max_parallel: usize,
    ) -> Result<Vec<Vec<FileMatchCount>>, String> {
        let files = scan::walk_files(root, |_| true)?;
        let literals: Vec<scan::Literal> = queries.iter().map(|q| scan::Literal::new(q)).collect();
        scan::count_files(&literals, &files, max_parallel)
    }

    /// `searchMatchingLines`: the first `max` lines of a file that contain the literal.
    fn find_lines(&self, query: &str, path: &str, max: usize) -> Vec<String> {
        scan::search_matching_lines(query, path, max).unwrap_or_default()
    }

    fn search_opencode(
        &self,
        query: &str,
        database_path: &str,
        limit: usize,
        snippets: usize,
    ) -> Result<Vec<StoreSearchMatch>, String> {
        search_opencode_store(query, database_path, limit, snippets)
    }

    fn read_project(&self, path: &str, source: TranscriptSource) -> String {
        read_transcript_project(path, source)
    }

    /// The first `bytes` bytes of a file as text.
    fn read_prefix(&self, path: &str, bytes: u64) -> Result<String, String> {
        read_prefix(path, bytes)
    }

    fn load_messages(
        &self,
        path: &str,
        source: TranscriptSource,
    ) -> Result<Vec<RecallMessage>, String> {
        load_recall_messages(path, Some(source))
    }
}

/// The real stores and the default index.
pub struct Disk;

impl Backend for Disk {}

/// `Bun.file(path).slice(0, bytes).text()`.
pub fn read_prefix(path: &str, bytes: u64) -> Result<String, String> {
    use std::io::Read;
    let file =
        std::fs::File::open(path).map_err(|error| crate::reader::fs_error(&error, "open", path))?;
    let mut buffer = Vec::new();
    file.take(bytes)
        .read_to_end(&mut buffer)
        .map_err(|error| crate::reader::fs_error(&error, "read", path))?;
    let text = String::from_utf8_lossy(&buffer);
    Ok(text.strip_prefix('\u{feff}').unwrap_or(&text).to_string())
}

/// `readTranscriptProject(path, source)`: a Codex transcript's `session_meta`
/// cwd from its first 128 KiB; other sources decode the path.
pub fn read_transcript_project(path: &str, source: TranscriptSource) -> String {
    if source != TranscriptSource::Codex {
        return project_from_transcript_path(path, source);
    }
    match read_prefix(path, 128 * 1024) {
        Ok(text) => project_from_transcript_text(path, source, &text),
        Err(_) => "~".to_string(),
    }
}

/// `prepareRecallMessages`: user and assistant messages with content.
pub fn prepare_recall_messages(messages: Vec<RecallMessage>) -> Vec<RecallMessage> {
    messages
        .into_iter()
        .filter(|message| message.role == "user" || message.role == "assistant")
        .filter(|message| !message.content.is_empty())
        .collect()
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    pub query: String,
    pub sources: Vec<TranscriptSource>,
    pub matches: Vec<StoreSearchMatch>,
    pub skipped_stores: Vec<StoreDiagnostic>,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct SearchOptions {
    pub source: SourceSelector,
    pub limit: usize,
    pub snippets: usize,
    pub max_parallel: usize,
    pub no_index: bool,
}

impl Default for SearchOptions {
    fn default() -> Self {
        SearchOptions {
            source: SourceSelector::All,
            limit: crate::DEFAULT_SEARCH_LIMIT,
            snippets: crate::DEFAULT_SNIPPET_LIMIT,
            max_parallel: DEFAULT_MAX_PARALLEL,
            no_index: false,
        }
    }
}

/// JavaScript `String.prototype.trim()`.
pub fn js_trim(text: &str) -> &str {
    text.trim_matches(|c: char| {
        matches!(
            c,
            '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
                ..='\u{200a}'
                    | '\u{2028}'
                    | '\u{2029}'
                    | '\u{202f}'
                    | '\u{205f}'
                    | '\u{3000}'
                    | '\u{feff}'
        )
    })
}

/// One store's direct results: matches with snippets (index, OpenCode) or file counts.
struct StoreScan {
    store: TranscriptStore,
    direct: Vec<StoreSearchMatch>,
    /// Whether `direct` came from the index without snippets.
    from_index: bool,
    counts: Vec<FileMatchCount>,
    diagnostic: Option<StoreDiagnostic>,
}

/// `searchSessions(query, options)`: transcripts that contain the literal,
/// ranked by count then date, each with up to `snippets` snippets.
pub fn search_sessions(
    query: &str,
    options: SearchOptions,
    backend: &dyn Backend,
) -> Result<SearchResult, String> {
    let needle = js_trim(query);
    if needle.is_empty() {
        return Err("search token must not be empty".into());
    }
    let (limit, snippet_limit, max_parallel) =
        (options.limit, options.snippets, options.max_parallel);
    for (value, name) in [
        (limit, "limit"),
        (snippet_limit, "snippets"),
        (max_parallel, "maxParallel"),
    ] {
        if value < 1 {
            return Err(format!("{name} must be an integer >= 1"));
        }
    }
    let stores = backend.discover_stores(options.source);
    if stores.is_empty() {
        return Err(format!(
            "no transcript stores found for source: {}",
            options.source.as_str()
        ));
    }
    let started = Instant::now();
    let mut indexed: HashMap<String, Vec<StoreSearchMatch>> = HashMap::new();
    let mut reader: Option<IndexReader> = None;
    if let Some(index_path) = backend.index_path()
        && !options.no_index
        && js::len(needle) >= 3
    {
        let attempt =
            || -> Result<(IndexReader, HashMap<String, Vec<StoreSearchMatch>>), String> {
                let refreshed =
                    refresh_transcript_index(&stores, &index_path, false, max_parallel)?;
                let skipped: HashSet<&str> =
                    refreshed.skipped.iter().map(|d| d.path.as_str()).collect();
                let indexed_stores: Vec<TranscriptStore> = stores
                    .iter()
                    .filter(|store| !skipped.contains(store.path.as_str()))
                    .cloned()
                    .collect();
                let mut by_store: HashMap<String, Vec<StoreSearchMatch>> = indexed_stores
                    .iter()
                    .map(|store| (store.path.clone(), Vec::new()))
                    .collect();
                let reader = IndexReader::open(&index_path)?;
                let cap = limit
                    .saturating_mul(4)
                    .saturating_mul(indexed_stores.len().max(1));
                for row in reader.grouped_matches(needle, &indexed_stores, cap, TieBreak::Date)? {
                    let found = row.into_match();
                    if let Some(list) = by_store.get_mut(&found.store) {
                        list.push(found.matched);
                    }
                }
                Ok((reader, by_store))
            };
        // The filesystem scanner remains the compatibility fallback.
        if let Ok((open, by_store)) = attempt() {
            reader = Some(open);
            indexed = by_store;
        }
    }

    let scans = map_pool(&stores, max_parallel, |store, _| {
        let scan = |direct, from_index, counts, diagnostic| StoreScan {
            store: store.clone(),
            direct,
            from_index,
            counts,
            diagnostic,
        };
        if let Some(found) = indexed.get(&store.path) {
            return scan(found.clone(), true, Vec::new(), None);
        }
        if store.kind == StoreKind::Sqlite {
            return match backend.search_opencode(
                needle,
                &store.path,
                limit.saturating_mul(4),
                snippet_limit,
            ) {
                Ok(direct) => scan(direct, false, Vec::new(), None),
                Err(error) => scan(
                    Vec::new(),
                    false,
                    Vec::new(),
                    Some(StoreDiagnostic {
                        source: store.source,
                        path: store.path.clone(),
                        error,
                    }),
                ),
            };
        }
        let mut counts = backend
            .count_files(&[needle], &store.path, max_parallel)
            .ok()
            .and_then(|mut lists| lists.pop())
            .unwrap_or_default();
        counts.sort_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then_with(|| locale_compare(&b.path, &a.path))
        });
        counts.truncate(limit.saturating_mul(4));
        scan(Vec::new(), false, counts, None)
    })?;

    let file_tasks: Vec<(&TranscriptStore, &FileMatchCount)> = scans
        .iter()
        .flat_map(|scan| scan.counts.iter().map(move |item| (&scan.store, item)))
        .collect();
    let lowered = js_lower(needle).into_owned();
    let file_matches = map_pool(&file_tasks, max_parallel, |(store, item), _| {
        let lines = backend.find_lines(needle, &item.path, snippet_limit.saturating_mul(20));
        let visible: Vec<_> = lines
            .iter()
            .filter_map(|line| extract_visible_message(line, store.source))
            .filter(|message| js_lower(&message.text).contains(lowered.as_str()))
            .take(snippet_limit)
            .collect();
        if visible.is_empty() {
            return None;
        }
        let project = match visible
            .iter()
            .find_map(|m| m.project.as_deref().filter(|p| !p.is_empty()))
        {
            Some(project) => compact_home(project).to_string(),
            None => compact_home(&backend.read_project(&item.path, store.source)).to_string(),
        };
        Some(StoreSearchMatch {
            source: store.source,
            path: item.path.clone(),
            count: item.count,
            date: visible
                .iter()
                .find_map(|m| m.date.clone().filter(|d| !d.is_empty()))
                .unwrap_or_else(|| date_from_path(&item.path)),
            project,
            snippets: visible
                .iter()
                .map(|message| TranscriptSnippet {
                    role: message.role.to_string(),
                    text: snippet_around(&message.text, needle),
                })
                .collect(),
        })
    })?;

    let mut candidates: Vec<(StoreSearchMatch, bool)> = Vec::new();
    let mut file_matches = file_matches.into_iter();
    for scan in &scans {
        candidates.extend(scan.direct.iter().cloned().map(|m| (m, scan.from_index)));
        candidates.extend(
            file_matches
                .by_ref()
                .take(scan.counts.len())
                .flatten()
                .map(|m| (m, false)),
        );
    }
    candidates.sort_by(|(a, _), (b, _)| rank(a, b));
    candidates.truncate(limit);
    let mut matches = Vec::with_capacity(candidates.len());
    for (mut found, from_index) in candidates {
        if from_index && let Some(reader) = &reader {
            found.snippets = reader.snippets(needle, &found.path, snippet_limit)?;
        }
        matches.push(found);
    }

    let mut sources = Vec::new();
    for scan in scans.iter().filter(|scan| scan.diagnostic.is_none()) {
        if !sources.contains(&scan.store.source) {
            sources.push(scan.store.source);
        }
    }
    Ok(SearchResult {
        query: needle.to_string(),
        sources,
        matches,
        skipped_stores: scans
            .into_iter()
            .filter_map(|scan| scan.diagnostic)
            .collect(),
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

/// Count descending, then date descending.
fn rank(a: &StoreSearchMatch, b: &StoreSearchMatch) -> Ordering {
    b.count
        .cmp(&a.count)
        .then_with(|| locale_compare(&b.date, &a.date))
}

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// `renderSearch(result)`.
pub fn render_search(result: &SearchResult) -> String {
    if result.matches.is_empty() {
        return format!(
            "No sessions found matching \"{}\".\n\nSearch is literal, not semantic. Retry with one exact distinctive token or phrase.",
            result.query
        );
    }
    let sections: Vec<String> = result
        .matches
        .iter()
        .map(|found| {
            let snippets: Vec<String> = found
                .snippets
                .iter()
                .map(|snippet| format!("  [{}] {}", snippet.role, snippet.text))
                .collect();
            format!(
                "{} · {} · {} · {}\nTranscript: {}\n{}",
                found.date,
                found.source,
                found.project,
                plural(found.count, "match", "matches"),
                found.path,
                snippets.join("\n")
            )
        })
        .collect();
    format!(
        "Found {} matching \"{}\":\n\n{}",
        plural(result.matches.len(), "session", "sessions"),
        result.query,
        sections.join("\n\n---\n\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::time::Duration;

    fn jsonl(source: TranscriptSource, path: &str) -> TranscriptStore {
        TranscriptStore {
            source,
            kind: StoreKind::Jsonl,
            path: path.into(),
        }
    }

    fn sqlite(path: &str) -> TranscriptStore {
        TranscriptStore {
            source: TranscriptSource::Opencode,
            kind: StoreKind::Sqlite,
            path: path.into(),
        }
    }

    type CountFn = dyn Fn(&str, &str) -> Vec<FileMatchCount> + Sync;
    type LinesFn = dyn Fn(&str, &str) -> Vec<String> + Sync;
    type OpenCodeFn = dyn Fn(&str) -> Result<Vec<StoreSearchMatch>, String> + Sync;

    struct Fake {
        stores: Vec<TranscriptStore>,
        counts: Box<CountFn>,
        lines: Box<LinesFn>,
        opencode: Box<OpenCodeFn>,
    }

    impl Backend for Fake {
        fn discover_stores(&self, _: SourceSelector) -> Vec<TranscriptStore> {
            self.stores.clone()
        }
        fn index_path(&self) -> Option<String> {
            None
        }
        fn count_files(
            &self,
            queries: &[&str],
            root: &str,
            _: usize,
        ) -> Result<Vec<Vec<FileMatchCount>>, String> {
            Ok(queries.iter().map(|q| (self.counts)(q, root)).collect())
        }
        fn find_lines(&self, query: &str, path: &str, _: usize) -> Vec<String> {
            (self.lines)(query, path)
        }
        fn search_opencode(
            &self,
            _: &str,
            path: &str,
            _: usize,
            _: usize,
        ) -> Result<Vec<StoreSearchMatch>, String> {
            (self.opencode)(path)
        }
        fn read_project(&self, _: &str, _: TranscriptSource) -> String {
            "project".into()
        }
    }

    fn line(kind: &str, role: &str, timestamp: &str, cwd: Option<&str>, text: &str) -> String {
        let mut value = serde_json::json!({
            "type": kind, "timestamp": timestamp,
            "message": { "role": role, "content": [{ "type": "text", "text": text }] },
        });
        if let Some(cwd) = cwd {
            value["cwd"] = cwd.into();
        }
        value.to_string()
    }

    #[test]
    fn bounds_reverse_completing_store_and_file_work_while_preserving_store_order() {
        let stores: Vec<TranscriptStore> = (0..6)
            .map(|i| jsonl(TranscriptSource::Claude, &format!("/store-{i}")))
            .collect();
        let delays = [120u64, 90, 60, 30, 10, 5];
        let store_done = Mutex::new(Vec::new());
        let file_done = Mutex::new(Vec::new());
        let (active_stores, peak_stores) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let (active_files, peak_files) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let index_of = |text: &str| -> usize {
            let digits: String = text
                .split("store-")
                .nth(1)
                .unwrap()
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            digits.parse().unwrap()
        };
        let track =
            |active: &AtomicUsize, peak: &AtomicUsize, done: &Mutex<Vec<usize>>, i: usize| {
                let now = active.fetch_add(1, AtomicOrdering::SeqCst) + 1;
                peak.fetch_max(now, AtomicOrdering::SeqCst);
                std::thread::sleep(Duration::from_millis(delays[i]));
                done.lock().unwrap().push(i);
                active.fetch_sub(1, AtomicOrdering::SeqCst);
            };
        // The fake's closures must be 'static, so they borrow through raw statics.
        let fake = Fake {
            stores: stores.clone(),
            counts: Box::new(|_, _| Vec::new()),
            lines: Box::new(|_, _| Vec::new()),
            opencode: Box::new(|_| Ok(Vec::new())),
        };
        struct Tracked<'a> {
            fake: Fake,
            on_store: &'a (dyn Fn(usize) + Sync),
            on_file: &'a (dyn Fn(usize) + Sync),
            index_of: &'a (dyn Fn(&str) -> usize + Sync),
        }
        impl Backend for Tracked<'_> {
            fn discover_stores(&self, s: SourceSelector) -> Vec<TranscriptStore> {
                self.fake.discover_stores(s)
            }
            fn index_path(&self) -> Option<String> {
                None
            }
            fn count_files(
                &self,
                _: &[&str],
                root: &str,
                _: usize,
            ) -> Result<Vec<Vec<FileMatchCount>>, String> {
                (self.on_store)((self.index_of)(root));
                Ok(vec![vec![FileMatchCount {
                    path: format!("{root}/session.jsonl"),
                    count: 1,
                }]])
            }
            fn find_lines(&self, _: &str, path: &str, _: usize) -> Vec<String> {
                (self.on_file)((self.index_of)(path));
                vec![line("user", "user", "2026-08-01T00:00:00Z", None, "Needle")]
            }
            fn read_project(&self, _: &str, _: TranscriptSource) -> String {
                "project".into()
            }
        }
        let on_store = |i| track(&active_stores, &peak_stores, &store_done, i);
        let on_file = |i| track(&active_files, &peak_files, &file_done, i);
        let backend = Tracked {
            fake,
            on_store: &on_store,
            on_file: &on_file,
            index_of: &index_of,
        };
        let options = SearchOptions {
            limit: 6,
            ..SearchOptions::default()
        };
        let result = search_sessions("Needle", options, &backend).unwrap();
        assert_eq!(*store_done.lock().unwrap(), [3, 4, 5, 2, 1, 0]);
        assert_eq!(*file_done.lock().unwrap(), [3, 4, 5, 2, 1, 0]);
        assert_eq!(peak_stores.load(AtomicOrdering::SeqCst), 4);
        assert_eq!(peak_files.load(AtomicOrdering::SeqCst), 4);
        let paths: Vec<String> = result.matches.iter().map(|m| m.path.clone()).collect();
        let expected: Vec<String> = stores
            .iter()
            .map(|s| format!("{}/session.jsonl", s.path))
            .collect();
        assert_eq!(paths, expected);
    }

    #[test]
    fn merges_and_ranks_file_and_sqlite_stores() {
        let open_code = StoreSearchMatch {
            source: TranscriptSource::Opencode,
            path: "opencode:///opencode.db#ses".into(),
            count: 7,
            date: "2026-08-03".into(),
            project: "/work/open".into(),
            snippets: vec![TranscriptSnippet {
                role: "user".into(),
                text: "Needle".into(),
            }],
        };
        let fake = Fake {
            stores: vec![
                jsonl(TranscriptSource::Claude, "/claude"),
                jsonl(TranscriptSource::Pi, "/pi"),
                sqlite("/opencode.db"),
            ],
            counts: Box::new(|_, root| {
                let (path, count) = if root == "/claude" {
                    ("/claude/project/a.jsonl", 2)
                } else {
                    ("/pi/project/b.jsonl", 5)
                };
                vec![FileMatchCount {
                    path: path.into(),
                    count,
                }]
            }),
            lines: Box::new(|_, path| {
                vec![if path.contains("claude") {
                    line(
                        "assistant",
                        "assistant",
                        "2026-08-01T00:00:00Z",
                        Some("/work/claude"),
                        "Needle in Claude",
                    )
                } else {
                    line(
                        "message",
                        "user",
                        "2026-08-02T00:00:00Z",
                        None,
                        "Needle in Pi",
                    )
                }]
            }),
            opencode: Box::new(move |_| Ok(vec![open_code.clone()])),
        };
        let options = SearchOptions {
            limit: 2,
            ..SearchOptions::default()
        };
        let result = search_sessions("Needle", options, &fake).unwrap();
        assert_eq!(
            result.sources,
            [
                TranscriptSource::Claude,
                TranscriptSource::Pi,
                TranscriptSource::Opencode
            ]
        );
        let ranked: Vec<(TranscriptSource, usize)> =
            result.matches.iter().map(|m| (m.source, m.count)).collect();
        assert_eq!(
            ranked,
            [(TranscriptSource::Opencode, 7), (TranscriptSource::Pi, 5)]
        );
    }

    #[test]
    fn skips_an_unreadable_opencode_store_and_searches_the_readable_stores() {
        let readable = StoreSearchMatch {
            source: TranscriptSource::Opencode,
            path: "opencode:///readable.db#ses".into(),
            count: 2,
            date: "2026-08-04".into(),
            project: "/work/readable".into(),
            snippets: vec![TranscriptSnippet {
                role: "user".into(),
                text: "Needle".into(),
            }],
        };
        let expected = readable.clone();
        let fake = Fake {
            stores: vec![sqlite("/unreadable.db"), sqlite("/readable.db")],
            counts: Box::new(|_, _| Vec::new()),
            lines: Box::new(|_, _| Vec::new()),
            opencode: Box::new(move |path| {
                if path == "/unreadable.db" {
                    Err("unable to open database file".into())
                } else {
                    Ok(vec![readable.clone()])
                }
            }),
        };
        let run = |max_parallel| {
            let options = SearchOptions {
                source: SourceSelector::Only(TranscriptSource::Opencode),
                max_parallel,
                ..SearchOptions::default()
            };
            let mut result = search_sessions("Needle", options, &fake).unwrap();
            result.elapsed_ms = 0;
            result
        };
        let result = run(4);
        assert_eq!(result.matches, [expected]);
        assert_eq!(result.sources, [TranscriptSource::Opencode]);
        assert_eq!(
            result.skipped_stores,
            [StoreDiagnostic {
                source: TranscriptSource::Opencode,
                path: "/unreadable.db".into(),
                error: "unable to open database file".into(),
            }]
        );
        assert_eq!(run(1), result);
    }

    #[test]
    fn rejects_an_empty_literal_before_discovering_stores() {
        struct Panics;
        impl Backend for Panics {
            fn discover_stores(&self, _: SourceSelector) -> Vec<TranscriptStore> {
                panic!("should not run")
            }
        }
        let error = search_sessions("  ", SearchOptions::default(), &Panics).unwrap_err();
        assert!(error.contains("must not be empty"));
    }

    #[test]
    fn searches_the_index_and_fetches_snippets_for_returned_matches() {
        let root = std::env::temp_dir().join(format!("dejavu-search-index-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store_path = root.join("claude");
        std::fs::create_dir_all(store_path.join("-work-a")).unwrap();
        let store_path = store_path.to_string_lossy().into_owned();
        for (name, text) in [("one", "needle needle"), ("two", "needle once")] {
            std::fs::write(
                format!("{store_path}/-work-a/{name}.jsonl"),
                line(
                    "user",
                    "user",
                    "2026-09-01T00:00:00Z",
                    Some("/work/a"),
                    text,
                ),
            )
            .unwrap();
        }
        struct Indexed {
            store: String,
            index: String,
        }
        impl Backend for Indexed {
            fn discover_stores(&self, _: SourceSelector) -> Vec<TranscriptStore> {
                vec![jsonl(TranscriptSource::Claude, &self.store)]
            }
            fn index_path(&self) -> Option<String> {
                Some(self.index.clone())
            }
        }
        let backend = Indexed {
            store: store_path.clone(),
            index: root.join("index.sqlite").to_string_lossy().into_owned(),
        };
        let result = search_sessions("Needle", SearchOptions::default(), &backend).unwrap();
        let summary: Vec<(String, usize, usize)> = result
            .matches
            .iter()
            .map(|m| (m.path.clone(), m.count, m.snippets.len()))
            .collect();
        assert_eq!(
            summary,
            [
                (format!("{store_path}/-work-a/one.jsonl"), 2, 1),
                (format!("{store_path}/-work-a/two.jsonl"), 1, 1),
            ]
        );
        assert_eq!(result.matches[0].project, "/work/a");
        let direct = search_sessions(
            "Needle",
            SearchOptions {
                no_index: true,
                ..SearchOptions::default()
            },
            &backend,
        )
        .unwrap();
        // Direct scans count matching lines, and equal counts and dates keep path-descending order.
        let direct: Vec<(String, usize)> = direct
            .matches
            .iter()
            .map(|m| (m.path.clone(), m.count))
            .collect();
        assert_eq!(
            direct,
            [
                (format!("{store_path}/-work-a/two.jsonl"), 1),
                (format!("{store_path}/-work-a/one.jsonl"), 1),
            ]
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
