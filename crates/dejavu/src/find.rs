//! The multi-term session finder (`find.ts` `findSessions`): candidates from
//! the index or direct scans, ranked so balanced multi-term relevance beats
//! one-term spam, then scored from visible messages with user messages weighted.

use crate::DEFAULT_MAX_PARALLEL;
use crate::index::{IndexReader, TieBreak, refresh_transcript_index};
use crate::js;
use crate::opencode::iso_date_from_millis;
use crate::paths::{date_from_path, js_lower, project_from_claude_path, project_from_pi_path};
use crate::pool::map_pool;
use crate::reader::extract_visible_message;
use crate::scan::locale_compare;
use crate::search::{Backend, js_trim, prepare_recall_messages};
use crate::types::{
    RecallBlock, SourceSelector, StoreDiagnostic, StoreKind, StoreSearchMatch, TranscriptSource,
    TranscriptStore,
};
use serde::Serialize;
use serde::ser::SerializeMap;
use std::collections::{HashMap, HashSet};
use std::time::Instant;

const CANDIDATE_CAP: usize = 40;
const USER_WEIGHT: usize = 5;

/// `/[/\\](subagents|tool-results|subagent-artifacts)[/\\]/`
fn is_noise_path(path: &str) -> bool {
    ["subagents", "tool-results", "subagent-artifacts"]
        .iter()
        .any(|name| {
            path.match_indices(name).any(|(at, _)| {
                let b = path.as_bytes();
                at > 0
                    && matches!(b[at - 1], b'/' | b'\\')
                    && matches!(b.get(at + name.len()), Some(b'/' | b'\\'))
            })
        })
}

/// An object whose keys keep insertion order, except that array-index keys
/// (`"0"`, `"42"`) come first in ascending order, as JavaScript objects do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsObject<V>(pub Vec<(String, V)>);

impl<V> Default for JsObject<V> {
    fn default() -> Self {
        JsObject(Vec::new())
    }
}

fn array_index(key: &str) -> Option<u32> {
    if key.is_empty() || (key.len() > 1 && key.starts_with('0')) {
        return None;
    }
    key.parse::<u32>().ok().filter(|&n| n != u32::MAX)
}

impl<V> JsObject<V> {
    pub fn get(&self, key: &str) -> Option<&V> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// `object[key] = value`.
    pub fn set(&mut self, key: &str, value: V) {
        match self.0.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = value,
            None => self.0.push((key.to_string(), value)),
        }
    }

    /// `Object.entries(object)`.
    pub fn entries(&self) -> Vec<&(String, V)> {
        let mut indexed: Vec<&(String, V)> = self
            .0
            .iter()
            .filter(|(k, _)| array_index(k).is_some())
            .collect();
        indexed.sort_by_key(|(k, _)| array_index(k));
        indexed.extend(self.0.iter().filter(|(k, _)| array_index(k).is_none()));
        indexed
    }
}

impl<V: Serialize> Serialize for JsObject<V> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let entries = self.entries();
        let mut map = serializer.serialize_map(Some(entries.len()))?;
        for (key, value) in entries {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct TermCount {
    pub user: usize,
    pub assistant: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FindMatch {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FindHit {
    pub source: TranscriptSource,
    pub path: String,
    pub project: String,
    pub date: String,
    pub score: usize,
    pub term_counts: JsObject<TermCount>,
    pub opening_prompt: String,
    pub matches: Vec<FindMatch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FindResult {
    pub terms: Vec<String>,
    pub required_terms: Vec<String>,
    pub sources: Vec<TranscriptSource>,
    pub hits: Vec<FindHit>,
    pub skipped_stores: Vec<StoreDiagnostic>,
    pub elapsed_ms: u64,
    pub store_timings: JsObject<u64>,
}

#[derive(Debug, Clone)]
pub struct FindOptions {
    pub source: SourceSelector,
    pub limit: usize,
    pub project: Option<String>,
    pub since: Option<String>,
    pub user_only: bool,
    pub max_parallel: usize,
    pub no_index: bool,
}

impl Default for FindOptions {
    fn default() -> Self {
        FindOptions {
            source: SourceSelector::All,
            limit: crate::DEFAULT_FIND_LIMIT,
            project: None,
            since: None,
            user_only: false,
            max_parallel: DEFAULT_MAX_PARALLEL,
            no_index: false,
        }
    }
}

fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_millis() as f64)
}

/// `parseSince(value)`: `YYYY-MM-DD` as is, or `<N>d`/`<N>w`/`<N>m` (30-day
/// months) before `today_ms`, as a UTC date.
pub fn parse_since(value: &str, today_ms: Option<f64>) -> Result<String, String> {
    let b = value.as_bytes();
    let digit = |i: usize| b[i].is_ascii_digit();
    if b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && [0, 1, 2, 3, 5, 6, 8, 9].iter().all(|&i| digit(i))
    {
        return Ok(value.to_string());
    }
    let invalid = || format!("--since needs YYYY-MM-DD or <N>d/<N>w/<N>m (got '{value}')");
    let (amount, unit) = value.split_at(value.len().saturating_sub(1));
    if amount.is_empty() || !amount.bytes().all(|c| c.is_ascii_digit()) {
        return Err(invalid());
    }
    let amount: f64 = amount.parse().map_err(|_| invalid())?;
    let days = match unit {
        "d" => amount,
        "w" => amount * 7.0,
        "m" => amount * 30.0,
        _ => return Err(invalid()),
    };
    iso_date_from_millis(today_ms.unwrap_or_else(now_ms) - days * 86_400_000.0)
}

fn matches_project(project: &str, needle: &str, source: TranscriptSource) -> bool {
    let haystack = js_lower(project);
    let query = js_lower(needle);
    if haystack.contains(query.as_ref()) {
        return true;
    }
    // Claude and Pi encode both directory separators and literal hyphens as '-'.
    // Their decoded paths cannot distinguish these characters.
    matches!(source, TranscriptSource::Claude | TranscriptSource::Pi)
        && haystack
            .replace('-', "/")
            .contains(&query.replace('-', "/"))
}

/// `path.basename(path)`.
fn basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(at) => &trimmed[at + 1..],
        None => trimmed,
    }
}

fn is_id_char(c: u8) -> bool {
    c.is_ascii_digit() || (b'a'..=b'f').contains(&c) || c == b'-'
}

/// `resumeCommand(source, path)`: how to reopen a session in its agent.
pub fn resume_command(source: TranscriptSource, path: &str) -> Option<String> {
    let name = basename(path);
    match source {
        TranscriptSource::Claude => {
            let id = name.strip_suffix(".jsonl").unwrap_or(name);
            (id.len() == 36 && id.bytes().all(is_id_char)).then(|| format!("claude --resume {id}"))
        }
        TranscriptSource::Codex => {
            // /rollout-.*-([0-9a-f-]{36})\.jsonl$/
            let stem = name.strip_suffix(".jsonl")?;
            let b = stem.as_bytes();
            if b.len() < 37 {
                return None;
            }
            let dash = b.len() - 37;
            let id = &stem[dash + 1..];
            if b[dash] != b'-' || !id.bytes().all(is_id_char) {
                return None;
            }
            let prefix = &stem[..dash];
            let opened = prefix
                .match_indices("rollout-")
                .any(|(at, _)| !prefix[at..].contains(['\n', '\r', '\u{2028}', '\u{2029}']));
            opened.then(|| format!("codex resume {id}"))
        }
        TranscriptSource::Pi => Some(format!("pi --session {path}")),
        TranscriptSource::Opencode => None,
    }
}

fn is_real_user_prompt(text: &str) -> bool {
    let trimmed = js_trim(text);
    !(trimmed.is_empty()
        || trimmed.starts_with('<')
        || trimmed.starts_with("Caveat:")
        || trimmed.starts_with("[Request interrupted")
        || trimmed.starts_with("Base directory for this skill")
        || trimmed.starts_with("This session is being continued from a previous conversation"))
}

/// `findOpeningPrompt(path, source)`: the first real user prompt, best effort.
pub fn find_opening_prompt(path: &str, source: TranscriptSource, backend: &dyn Backend) -> String {
    if source == TranscriptSource::Opencode {
        let Ok(messages) = backend.load_messages(path, source) else {
            return String::new();
        };
        let messages = prepare_recall_messages(messages);
        let first = messages.iter().find(|message| {
            message.role == "user"
                && message
                    .content
                    .iter()
                    .any(|block| block.text().is_some_and(is_real_user_prompt))
        });
        let text: Vec<&str> = first
            .map(|message| {
                message
                    .content
                    .iter()
                    .filter_map(RecallBlock::text)
                    .collect()
            })
            .unwrap_or_default();
        return js_trim(&text.join(" ")).to_string();
    }
    let Ok(prefix) = backend.read_prefix(path, 256 * 1024) else {
        return String::new();
    };
    for line in prefix.split('\n') {
        if let Some(message) = extract_visible_message(line, source)
            && message.role == "user"
            && is_real_user_prompt(&message.text)
        {
            return js_trim(&message.text).to_string();
        }
    }
    String::new()
}

struct Candidate {
    source: TranscriptSource,
    path: String,
    /// Raw counts per term, in term order.
    raw_counts: Vec<(String, usize)>,
}

impl Candidate {
    fn raw_min(&self) -> usize {
        self.raw_counts.iter().map(|(_, c)| *c).min().unwrap_or(0)
    }
    fn raw_total(&self) -> usize {
        self.raw_counts.iter().map(|(_, c)| *c).sum()
    }
}

/// One (term, store) scan.
struct Batch {
    term_index: usize,
    store: TranscriptStore,
    elapsed_ms: u64,
    rows: Vec<(String, usize)>,
    /// Matches kept for OpenCode scoring; `true` when they still need snippets from the index.
    matches: Vec<StoreSearchMatch>,
    from_index: bool,
    diagnostic: Option<StoreDiagnostic>,
}

/// `findSessions(terms, options)`.
pub fn find_sessions(
    terms: &[String],
    options: &FindOptions,
    backend: &dyn Backend,
) -> Result<FindResult, String> {
    let mut cleaned: Vec<String> = Vec::new();
    let mut normalized: HashSet<String> = HashSet::new();
    for raw in terms {
        let term = js_trim(raw);
        let lowered = js_lower(term).into_owned();
        if term.is_empty() || normalized.contains(&lowered) {
            continue;
        }
        cleaned.push(term.to_string());
        normalized.insert(lowered);
    }
    if cleaned.is_empty() {
        return Err("find needs at least one term".into());
    }
    if options.limit < 1 {
        return Err(format!(
            "limit must be an integer >= 1 (got '{}')",
            options.limit
        ));
    }
    if options.max_parallel < 1 {
        return Err(format!(
            "maxParallel must be an integer >= 1 (got '{}')",
            options.max_parallel
        ));
    }
    let started = Instant::now();
    let stores = backend.discover_stores(options.source);
    if stores.is_empty() {
        return Err(format!(
            "no transcript stores found for source: {}",
            options.source.as_str()
        ));
    }
    let max_parallel = options.max_parallel;

    // term index -> store path -> ranked matches from the transcript index (without snippets).
    let mut indexed: Vec<HashMap<String, Vec<StoreSearchMatch>>> = Vec::new();
    let mut reader: Option<IndexReader> = None;
    if let Some(index_path) = backend.index_path()
        && !options.no_index
        && cleaned.iter().all(|term| js::len(term) >= 3)
    {
        let attempt =
            || -> Result<(IndexReader, Vec<HashMap<String, Vec<StoreSearchMatch>>>), String> {
                let refreshed =
                    refresh_transcript_index(&stores, &index_path, false, max_parallel)?;
                let skipped: HashSet<&str> =
                    refreshed.skipped.iter().map(|d| d.path.as_str()).collect();
                let indexed_stores: Vec<TranscriptStore> = stores
                    .iter()
                    .filter(|store| !skipped.contains(store.path.as_str()))
                    .cloned()
                    .collect();
                let reader = IndexReader::open(&index_path)?;
                let mut per_term = Vec::new();
                for term in &cleaned {
                    let mut by_store: HashMap<String, Vec<StoreSearchMatch>> = indexed_stores
                        .iter()
                        .map(|store| (store.path.clone(), Vec::new()))
                        .collect();
                    for row in reader.grouped_matches(term, &indexed_stores, 800, TieBreak::Date)? {
                        let found = row.into_match();
                        if let Some(list) = by_store.get_mut(&found.store) {
                            list.push(found.matched);
                        }
                    }
                    per_term.push(by_store);
                }
                Ok((reader, per_term))
            };
        // The filesystem scanner remains the compatibility fallback.
        if let Ok((open, per_term)) = attempt() {
            reader = Some(open);
            indexed = per_term;
        }
    }

    // Each store is scanned once for every term; JSONL stores read each file once.
    let per_store = map_pool(&stores, max_parallel, |store, _| {
        let begun = Instant::now();
        let mut batches: Vec<Batch> = Vec::new();
        let batch = |term_index, rows, matches, from_index, diagnostic| Batch {
            term_index,
            store: store.clone(),
            elapsed_ms: 0,
            rows,
            matches,
            from_index,
            diagnostic,
        };
        let mut direct_terms: Vec<usize> = Vec::new();
        for (term_index, term) in cleaned.iter().enumerate() {
            if let Some(found) = indexed.get(term_index).and_then(|m| m.get(&store.path)) {
                let rows = found
                    .iter()
                    .filter(|m| {
                        store.kind == StoreKind::Sqlite
                            || (m.path.ends_with(".jsonl") && !is_noise_path(&m.path))
                    })
                    .map(|m| (m.path.clone(), m.count))
                    .collect();
                batches.push(batch(term_index, rows, found.clone(), true, None));
            } else if store.kind == StoreKind::Sqlite {
                match backend.search_opencode(term, &store.path, 200, 4) {
                    Ok(matches) => {
                        let rows = matches.iter().map(|m| (m.path.clone(), m.count)).collect();
                        batches.push(batch(term_index, rows, matches, false, None));
                    }
                    Err(error) => batches.push(batch(
                        term_index,
                        Vec::new(),
                        Vec::new(),
                        false,
                        Some(StoreDiagnostic {
                            source: store.source,
                            path: store.path.clone(),
                            error,
                        }),
                    )),
                }
            } else {
                direct_terms.push(term_index);
            }
        }
        if !direct_terms.is_empty() {
            let queries: Vec<&str> = direct_terms.iter().map(|&i| cleaned[i].as_str()).collect();
            let lists = backend
                .count_files(&queries, &store.path, max_parallel)
                .unwrap_or_else(|_| vec![Vec::new(); queries.len()]);
            for (&term_index, counts) in direct_terms.iter().zip(lists) {
                let rows = counts
                    .into_iter()
                    .filter(|item| item.path.ends_with(".jsonl") && !is_noise_path(&item.path))
                    .map(|item| (item.path, item.count))
                    .collect();
                batches.push(batch(term_index, rows, Vec::new(), false, None));
            }
        }
        let elapsed = begun.elapsed().as_millis() as u64;
        for batch in &mut batches {
            batch.elapsed_ms = elapsed;
        }
        batches
    })?;
    // Term-major order, as the TypeScript scanned term x store pairs.
    let mut scan_batches: Vec<Batch> = per_store.into_iter().flatten().collect();
    let store_order: HashMap<&str, usize> = stores
        .iter()
        .enumerate()
        .map(|(i, s)| (s.path.as_str(), i))
        .collect();
    scan_batches.sort_by_key(|b| (b.term_index, store_order[b.store.path.as_str()]));

    let mut store_timings: JsObject<u64> = JsObject::default();
    let mut skipped_by_path: HashMap<String, StoreDiagnostic> = HashMap::new();
    // path -> term index -> (match, needs index snippets)
    let mut open_code_matches: HashMap<String, HashMap<usize, (StoreSearchMatch, bool)>> =
        HashMap::new();
    for batch in &scan_batches {
        let source = batch.store.source.as_str();
        let previous = store_timings.get(source).copied().unwrap_or(0);
        store_timings.set(source, previous.max(batch.elapsed_ms));
        if let Some(diagnostic) = &batch.diagnostic {
            skipped_by_path.insert(batch.store.path.clone(), diagnostic.clone());
        }
        if batch.store.kind == StoreKind::Sqlite {
            for found in &batch.matches {
                open_code_matches
                    .entry(found.path.clone())
                    .or_default()
                    .insert(batch.term_index, (found.clone(), batch.from_index));
            }
        }
    }
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut by_path: HashMap<String, usize> = HashMap::new();
    for batch in &scan_batches {
        for (path, count) in &batch.rows {
            let index = *by_path.entry(path.clone()).or_insert_with(|| {
                candidates.push(Candidate {
                    source: batch.store.source,
                    path: path.clone(),
                    raw_counts: Vec::new(),
                });
                candidates.len() - 1
            });
            let term = &cleaned[batch.term_index];
            let raw = &mut candidates[index].raw_counts;
            match raw.iter_mut().find(|(t, _)| t == term) {
                Some(slot) => slot.1 = *count,
                None => raw.push((term.clone(), *count)),
            }
        }
    }

    // Path-derived project filter must run before the candidate cap, or matching
    // sessions can be capped away before the post-cap project check ever sees them.
    if let Some(project) = &options.project {
        let needle = js_lower(project).into_owned();
        candidates.retain(|candidate| match candidate.source {
            TranscriptSource::Claude => matches_project(
                &project_from_claude_path(&candidate.path),
                &needle,
                candidate.source,
            ),
            TranscriptSource::Pi => matches_project(
                &project_from_pi_path(&candidate.path),
                &needle,
                candidate.source,
            ),
            _ => true,
        });
    }
    // Path-dated candidates outside the --since window skip the deep scan entirely;
    // the rest are re-checked against message timestamps after scoring.
    let since_date = match &options.since {
        Some(since) => Some(parse_since(since, None)?),
        None => None,
    };
    if let Some(cutoff) = &since_date {
        candidates.retain(|candidate| {
            let date = date_from_path(&candidate.path);
            date == "unknown" || date.as_str() >= cutoff.as_str()
        });
    }
    // Rank balanced multi-term relevance above one-term spam.
    candidates.sort_by(|a, b| {
        b.raw_counts
            .len()
            .cmp(&a.raw_counts.len())
            .then_with(|| b.raw_min().cmp(&a.raw_min()))
            .then_with(|| b.raw_total().cmp(&a.raw_total()))
    });
    candidates.truncate(CANDIDATE_CAP);

    // OpenCode candidates score from snippets; fetch the index's now.
    if let Some(reader) = &reader {
        for candidate in candidates
            .iter()
            .filter(|c| c.source == TranscriptSource::Opencode)
        {
            let Some(per_term) = open_code_matches.get_mut(&candidate.path) else {
                continue;
            };
            for (term_index, (found, needs_snippets)) in per_term.iter_mut() {
                if *needs_snippets
                    && candidate
                        .raw_counts
                        .iter()
                        .any(|(t, _)| *t == cleaned[*term_index])
                {
                    found.snippets = reader.snippets(&cleaned[*term_index], &found.path, 4)?;
                    *needs_snippets = false;
                }
            }
        }
    }
    let term_index_of = |term: &str| cleaned.iter().position(|t| t == term).unwrap_or(0);

    let hit_candidates = map_pool(
        &candidates,
        max_parallel,
        |candidate, _| -> Option<FindHit> {
            let mut term_counts: JsObject<TermCount> = JsObject::default();
            let mut matches: Vec<FindMatch> = Vec::new();
            let mut score = 0;
            let mut latest_date = String::new();
            if candidate.source == TranscriptSource::Opencode {
                for (term, _) in &candidate.raw_counts {
                    let stored = open_code_matches
                        .get(&candidate.path)
                        .and_then(|m| m.get(&term_index_of(term)))
                        .map(|(found, _)| found);
                    let snippets = stored.map(|s| s.snippets.as_slice()).unwrap_or_default();
                    let user = snippets.iter().filter(|s| s.role == "user").count();
                    let assistant = snippets.len() - user;
                    term_counts.set(term, TermCount { user, assistant });
                    score += user * USER_WEIGHT + assistant;
                    matches.extend(snippets.iter().map(|s| FindMatch {
                        role: s.role.clone(),
                        date: None,
                        text: s.text.clone(),
                    }));
                    if let Some(stored) = stored
                        && !stored.date.is_empty()
                        && stored.date > latest_date
                    {
                        latest_date = stored.date.clone();
                    }
                }
            } else {
                for (term, _) in &candidate.raw_counts {
                    let lines = backend.find_lines(term, &candidate.path, 400);
                    let (mut user, mut assistant) = (0, 0);
                    let lowered = js_lower(term).into_owned();
                    for line in &lines {
                        let Some(message) = extract_visible_message(line, candidate.source) else {
                            continue;
                        };
                        if !js_lower(&message.text).contains(lowered.as_str()) {
                            continue;
                        }
                        if message.role == "user" {
                            user += 1;
                        } else {
                            assistant += 1;
                        }
                        if let Some(date) = &message.date
                            && !date.is_empty()
                            && *date > latest_date
                        {
                            latest_date = date.clone();
                        }
                        if message.role == "user"
                            && matches.len() < 6
                            && is_real_user_prompt(&message.text)
                        {
                            let text = js::prefix(&message.text, 240).to_string();
                            let head = js::prefix(&text, 80);
                            if !matches.iter().any(|m| js::prefix(&m.text, 80) == head) {
                                matches.push(FindMatch {
                                    role: message.role.to_string(),
                                    date: message.date.clone(),
                                    text,
                                });
                            }
                        }
                    }
                    term_counts.set(term, TermCount { user, assistant });
                    score += user * USER_WEIGHT + assistant;
                }
            }
            if score == 0 {
                return None;
            }
            let date = if latest_date.is_empty() {
                date_from_path(&candidate.path)
            } else {
                latest_date
            };
            if let Some(cutoff) = &since_date
                && date != "unknown"
                && date.as_str() < cutoff.as_str()
            {
                return None;
            }
            let project = if candidate.source == TranscriptSource::Opencode {
                "opencode".to_string()
            } else {
                backend.read_project(&candidate.path, candidate.source)
            };
            if let Some(needle) = &options.project
                && !matches_project(&project, needle, candidate.source)
            {
                return None;
            }
            let opening = find_opening_prompt(&candidate.path, candidate.source, backend);
            Some(FindHit {
                source: candidate.source,
                path: candidate.path.clone(),
                project,
                date,
                score,
                term_counts,
                opening_prompt: js::prefix(&opening, 300).to_string(),
                matches,
                resume: resume_command(candidate.source, &candidate.path),
            })
        },
    )?;
    let mut hits: Vec<FindHit> = hit_candidates.into_iter().flatten().collect();
    let matched = |hit: &FindHit, term: &str| {
        let count = hit.term_counts.get(term);
        let (user, assistant) = count.map_or((0, 0), |c| (c.user, c.assistant));
        if options.user_only {
            user > 0
        } else {
            user + assistant > 0
        }
    };
    let matched_count = |hit: &FindHit| cleaned.iter().filter(|t| matched(hit, t)).count();
    hits.sort_by(|a, b| {
        matched_count(b)
            .cmp(&matched_count(a))
            .then_with(|| b.score.cmp(&a.score))
            .then_with(|| locale_compare(&b.date, &a.date))
    });
    let required_terms: Vec<String> = match hits.first() {
        Some(first) => cleaned
            .iter()
            .filter(|t| matched(first, t))
            .cloned()
            .collect(),
        None => Vec::new(),
    };
    let strict: Vec<FindHit> = hits
        .into_iter()
        .filter(|hit| required_terms.iter().all(|t| matched(hit, t)))
        .take(options.limit)
        .collect();
    let mut sources = Vec::new();
    for batch in scan_batches.iter().filter(|b| b.diagnostic.is_none()) {
        if !sources.contains(&batch.store.source) {
            sources.push(batch.store.source);
        }
    }
    Ok(FindResult {
        terms: cleaned.clone(),
        required_terms,
        sources,
        hits: strict,
        skipped_stores: stores
            .iter()
            .filter_map(|store| skipped_by_path.get(&store.path).cloned())
            .collect(),
        elapsed_ms: started.elapsed().as_millis() as u64,
        store_timings,
    })
}

/// `renderFind(result)`.
pub fn render_find(result: &FindResult) -> String {
    if result.hits.is_empty() {
        return format!(
            "No sessions found for: {}.\n\nTerms are literal (AND). Try fewer or different exact terms, or drop filters.",
            result.terms.join(", ")
        );
    }
    let relaxed = if result.required_terms.len() < result.terms.len() {
        format!(
            "No session matched all {} terms; showing sessions matching {}.\n\n",
            result.terms.len(),
            result.required_terms.len()
        )
    } else {
        String::new()
    };
    let sections: Vec<String> = result
        .hits
        .iter()
        .map(|hit| {
            let counts: Vec<String> = hit
                .term_counts
                .entries()
                .into_iter()
                .map(|(term, count)| {
                    format!("{term}×{}(u{})", count.user + count.assistant, count.user)
                })
                .collect();
            let mut lines = vec![format!(
                "{} · {} · {} · {}",
                hit.date,
                hit.source,
                hit.project,
                counts.join(" ")
            )];
            if !hit.opening_prompt.is_empty() {
                lines.push(format!("Opened with: {}", hit.opening_prompt));
            }
            for found in hit.matches.iter().take(3) {
                let date = match &found.date {
                    Some(date) if !date.is_empty() => format!(" {date}"),
                    _ => String::new(),
                };
                lines.push(format!("  [{}{date}] {}", found.role, found.text));
            }
            lines.push(format!("Transcript: {}", hit.path));
            if let Some(resume) = &hit.resume {
                lines.push(format!("Resume: {resume}"));
            }
            lines.join("\n")
        })
        .collect();
    format!(
        "{relaxed}Found {} session{} for {}:\n\n{}",
        result.hits.len(),
        if result.hits.len() == 1 { "" } else { "s" },
        result.terms.join(" + "),
        sections.join("\n\n---\n\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::FileMatchCount;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn parse_since_passes_absolute_dates_and_resolves_relative_windows() {
        assert_eq!(parse_since("2026-08-01", None).unwrap(), "2026-08-01");
        // 2026-08-28T12:00:00Z
        let today = 1_787_918_400_000.0;
        assert_eq!(parse_since("7d", Some(today)).unwrap(), "2026-08-21");
        assert_eq!(parse_since("2w", Some(today)).unwrap(), "2026-08-14");
        assert!(
            parse_since("yesterday", None)
                .unwrap_err()
                .contains("--since")
        );
        assert!(parse_since("d", None).is_err());
    }

    #[test]
    fn resume_commands() {
        assert_eq!(
            resume_command(
                TranscriptSource::Claude,
                "/x/-proj/2d695265-2734-49cd-be54-3ac3d6480ce9.jsonl"
            )
            .as_deref(),
            Some("claude --resume 2d695265-2734-49cd-be54-3ac3d6480ce9")
        );
        assert_eq!(
            resume_command(
                TranscriptSource::Codex,
                "/x/rollout-2026-08-21T00-49-55-01a0209d-9c33-7e43-95f2-458ff4fdad0f.jsonl"
            )
            .as_deref(),
            Some("codex resume 01a0209d-9c33-7e43-95f2-458ff4fdad0f")
        );
        assert_eq!(
            resume_command(TranscriptSource::Pi, "/x/2026-05-01T17-00-53-250Z_x.jsonl").as_deref(),
            Some("pi --session /x/2026-05-01T17-00-53-250Z_x.jsonl")
        );
        assert_eq!(
            resume_command(
                TranscriptSource::Opencode,
                "opencode:///x/opencode.db#ses_1"
            ),
            None
        );
        assert!(is_noise_path("/a/subagents/b.jsonl"));
        assert!(!is_noise_path("/a/subagents.jsonl"));
    }

    fn claude_line(role: &str, text: &str) -> String {
        serde_json::json!({ "uuid": "u", "message": { "role": role, "content": [{ "type": "text", "text": text }] }, "timestamp": "2026-08-25T08:00:00Z" }).to_string()
    }

    const STORE: &str = "/fake/.claude/projects";

    fn path_a() -> String {
        format!("{STORE}/-Users-me-Development-alpha/aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa.jsonl")
    }

    fn path_b() -> String {
        format!("{STORE}/-Users-me-Development-beta/bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb.jsonl")
    }

    type CountFn = Box<dyn Fn(&str) -> Vec<FileMatchCount> + Sync>;
    type LinesFn = Box<dyn Fn(&str, &str) -> Vec<String> + Sync>;

    struct Fake {
        stores: Vec<TranscriptStore>,
        counts: CountFn,
        lines: LinesFn,
        project: Box<dyn Fn(&str) -> String + Sync>,
        opencode_error: bool,
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
            _: &str,
            _: usize,
        ) -> Result<Vec<Vec<FileMatchCount>>, String> {
            Ok(queries.iter().map(|q| (self.counts)(q)).collect())
        }
        fn find_lines(&self, query: &str, path: &str, _: usize) -> Vec<String> {
            (self.lines)(query, path)
        }
        fn search_opencode(
            &self,
            _: &str,
            _: &str,
            _: usize,
            _: usize,
        ) -> Result<Vec<StoreSearchMatch>, String> {
            if self.opencode_error {
                Err("unable to open database file".into())
            } else {
                Ok(Vec::new())
            }
        }
        fn read_project(&self, path: &str, _: TranscriptSource) -> String {
            (self.project)(path)
        }
        fn read_prefix(&self, _: &str, _: u64) -> Result<String, String> {
            Ok(claude_line("user", "let us plan the workshop"))
        }
    }

    fn claude_store() -> TranscriptStore {
        TranscriptStore {
            source: TranscriptSource::Claude,
            kind: StoreKind::Jsonl,
            path: STORE.into(),
        }
    }

    fn deps() -> Fake {
        Fake {
            stores: vec![claude_store()],
            counts: Box::new(|term| match term {
                "workshop" => vec![
                    FileMatchCount {
                        path: path_a(),
                        count: 10,
                    },
                    FileMatchCount {
                        path: path_b(),
                        count: 500,
                    },
                ],
                "codex" => vec![FileMatchCount {
                    path: path_a(),
                    count: 20,
                }],
                _ => Vec::new(),
            }),
            lines: Box::new(|term, path| {
                if path == path_a() {
                    vec![claude_line("user", &format!("planning the {term} session"))]
                } else {
                    vec![claude_line("assistant", &format!("{term} ").repeat(3))]
                }
            }),
            project: Box::new(|_| "Development/alpha".into()),
            opencode_error: false,
        }
    }

    fn terms(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn bounds_reverse_completing_candidate_scans_and_preserves_candidate_order() {
        let paths: Vec<String> = (0..6)
            .map(|i| format!("{STORE}/-Users-me-Development-p{i}/{i}aaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa.jsonl"))
            .collect();
        let delays = [480u64, 360, 240, 120, 40, 20];
        let done = Mutex::new(Vec::new());
        let (active, peak) = (AtomicUsize::new(0), AtomicUsize::new(0));
        struct Tracked<'a> {
            inner: Fake,
            on_file: &'a (dyn Fn(&str) + Sync),
        }
        impl Backend for Tracked<'_> {
            fn discover_stores(&self, s: SourceSelector) -> Vec<TranscriptStore> {
                self.inner.discover_stores(s)
            }
            fn index_path(&self) -> Option<String> {
                None
            }
            fn count_files(
                &self,
                q: &[&str],
                r: &str,
                m: usize,
            ) -> Result<Vec<Vec<FileMatchCount>>, String> {
                self.inner.count_files(q, r, m)
            }
            fn find_lines(&self, q: &str, path: &str, m: usize) -> Vec<String> {
                (self.on_file)(path);
                self.inner.find_lines(q, path, m)
            }
            fn read_project(&self, p: &str, s: TranscriptSource) -> String {
                self.inner.read_project(p, s)
            }
            fn read_prefix(&self, p: &str, b: u64) -> Result<String, String> {
                self.inner.read_prefix(p, b)
            }
        }
        let listed = paths.clone();
        let mut inner = deps();
        inner.counts = Box::new(move |_| {
            listed
                .iter()
                .map(|path| FileMatchCount {
                    path: path.clone(),
                    count: 1,
                })
                .collect()
        });
        inner.lines = Box::new(|_, _| vec![claude_line("user", "planning the workshop session")]);
        let on_file = |path: &str| {
            let index = paths.iter().position(|p| p == path).unwrap();
            let now = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(delays[index]));
            done.lock().unwrap().push(index);
            active.fetch_sub(1, Ordering::SeqCst);
        };
        let backend = Tracked {
            inner,
            on_file: &on_file,
        };
        let options = FindOptions {
            limit: 6,
            ..FindOptions::default()
        };
        let result = find_sessions(&terms(&["workshop"]), &options, &backend).unwrap();
        assert_eq!(*done.lock().unwrap(), [3, 4, 5, 2, 1, 0]);
        assert_eq!(peak.load(Ordering::SeqCst), 4);
        let hit_paths: Vec<String> = result.hits.iter().map(|h| h.path.clone()).collect();
        assert_eq!(hit_paths, paths);
    }

    #[test]
    fn skips_an_unreadable_opencode_store_and_keeps_readable_candidates() {
        let run = |max_parallel| {
            let mut backend = deps();
            backend.stores = vec![
                TranscriptStore {
                    source: TranscriptSource::Opencode,
                    kind: StoreKind::Sqlite,
                    path: "/unreadable.db".into(),
                },
                claude_store(),
            ];
            backend.opencode_error = true;
            let options = FindOptions {
                max_parallel,
                ..FindOptions::default()
            };
            let mut result = find_sessions(&terms(&["workshop"]), &options, &backend).unwrap();
            result.elapsed_ms = 0;
            result.store_timings = JsObject::default();
            result
        };
        let result = run(4);
        assert_eq!(result.hits[0].path, path_a());
        assert_eq!(result.sources, [TranscriptSource::Claude]);
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
    fn and_across_terms_wins_over_one_term_spam() {
        let result = find_sessions(
            &terms(&["workshop", "codex"]),
            &FindOptions::default(),
            &deps(),
        )
        .unwrap();
        assert_eq!(result.hits[0].path, path_a());
        assert_eq!(result.required_terms, ["workshop", "codex"]);
        assert_eq!(result.hits[0].opening_prompt, "let us plan the workshop");
        assert_eq!(
            result.hits[0].resume.as_deref(),
            Some("claude --resume aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa")
        );
    }

    #[test]
    fn falls_back_to_fewer_terms_when_nothing_matches_all() {
        let result = find_sessions(
            &terms(&["zzznope", "workshop"]),
            &FindOptions::default(),
            &deps(),
        )
        .unwrap();
        assert_eq!(result.required_terms, ["workshop"]);
        assert!(!result.hits.is_empty());
    }

    #[test]
    fn deduplicates_terms_case_insensitively() {
        let result = find_sessions(
            &terms(&["workshop", "WORKSHOP"]),
            &FindOptions::default(),
            &deps(),
        )
        .unwrap();
        assert_eq!(result.terms, ["workshop"]);
        assert_eq!(result.required_terms, ["workshop"]);
    }

    #[test]
    fn falls_back_when_a_raw_match_is_not_visible_in_a_message() {
        let mut backend = deps();
        backend.counts = Box::new(|_| {
            vec![FileMatchCount {
                path: path_a(),
                count: 1,
            }]
        });
        backend.lines = Box::new(|term, _| {
            if term == "workshop" {
                vec![claude_line("user", "planning the workshop session")]
            } else {
                Vec::new()
            }
        });
        let result = find_sessions(
            &terms(&["workshop", "metadata"]),
            &FindOptions::default(),
            &backend,
        )
        .unwrap();
        assert_eq!(result.required_terms, ["workshop"]);
        assert_eq!(result.hits.len(), 1);
    }

    #[test]
    fn rejects_an_invalid_programmatic_limit() {
        let options = FindOptions {
            limit: 0,
            ..FindOptions::default()
        };
        assert!(
            find_sessions(&terms(&["workshop"]), &options, &deps())
                .unwrap_err()
                .contains("integer >= 1")
        );
    }

    #[test]
    fn user_drops_sessions_without_user_message_matches_for_every_term() {
        let options = FindOptions {
            user_only: true,
            ..FindOptions::default()
        };
        let result = find_sessions(&terms(&["workshop"]), &options, &deps()).unwrap();
        assert!(
            result
                .hits
                .iter()
                .all(|hit| hit.term_counts.0.iter().all(|(_, c)| c.user > 0))
        );
    }

    #[test]
    fn matches_a_hyphenated_project_name_in_a_decoded_claude_path() {
        let path = format!("{STORE}/-Users-me-Development-comfyui-modal/session.jsonl");
        let mut backend = deps();
        let listed = path.clone();
        backend.counts = Box::new(move |_| {
            vec![FileMatchCount {
                path: listed.clone(),
                count: 1,
            }]
        });
        backend.project = Box::new(|_| "Development/comfyui/modal".into());
        let options = FindOptions {
            project: Some("comfyui-modal".into()),
            ..FindOptions::default()
        };
        let result = find_sessions(&terms(&["workshop"]), &options, &backend).unwrap();
        let paths: Vec<&str> = result.hits.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(paths, [path.as_str()]);
    }

    #[test]
    fn project_filter_applies_before_the_candidate_cap() {
        let options = FindOptions {
            project: Some("beta".into()),
            ..FindOptions::default()
        };
        let result = find_sessions(&terms(&["workshop"]), &options, &deps()).unwrap();
        assert!(result.hits.iter().all(|hit| hit.path == path_b()));
    }

    #[test]
    fn serializes_numeric_keys_first_like_javascript() {
        let mut object = JsObject::default();
        object.set("deploy", 1);
        object.set("404", 2);
        object.set("12", 3);
        assert_eq!(js::stringify(&object), r#"{"12":3,"404":2,"deploy":1}"#);
    }
}
