//! `dejavu recall`: find sessions from a plain-language description. Keywords
//! drawn from the description (plus any `-k` terms) retrieve candidates through
//! `find`, one term at a time so a session needs only one of them. TypeSafe's
//! Jev then judges each candidate's opening prompt and excerpts against the
//! description, and sessions rank by that probability.

use crate::find::{self, FindHit, FindMatch, FindOptions, FindResult};
use crate::js;
use crate::paths::js_lower;
use crate::pool::map_pool;
use crate::search::{Backend, js_trim};
use crate::types::{SourceSelector, StoreDiagnostic, TranscriptSource};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

pub const DEFAULT_RECALL_LIMIT: usize = 5;
pub const DEFAULT_RECALL_CANDIDATES: usize = 30;
pub const DEFAULT_RECALL_THRESHOLD: f64 = 0.2;
pub const DEFAULT_RECALL_MODEL: &str = "jev-latest";
const DEFAULT_BASE_URL: &str = "https://api.typesafe.ai";
/// Description words kept as retrieval terms; `-k` terms come on top.
const MAX_DESCRIPTION_KEYWORDS: usize = 6;
/// Sessions per Jev request. Ten cards stay far below Jev's 64k-token request limit.
const BATCH_SIZE: usize = 10;
const MAX_EXCERPTS: usize = 6;
/// Jev bills input tokens only: $42 per billion.
const USD_PER_INPUT_TOKEN: f64 = 42.0 / 1_000_000_000.0;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const ATTEMPTS: u32 = 3;

const TASK: &str = "Recall over past coding-agent sessions: find the sessions where the work described in `search` happened. Each entry in `sessions` shows the session's source, project, date, opening request, and excerpts from messages that matched keywords of the search.";

/// Words that carry no retrieval signal in a description of a past session.
#[rustfmt::skip]
const STOPWORDS: &[&str] = &[
    "about", "after", "again", "all", "also", "and", "any", "are", "been", "before", "being",
    "but", "can", "chat", "conversation", "could", "did", "does", "doing", "done", "earlier",
    "for", "from", "had", "has", "have", "how", "into", "its", "just", "last", "made", "make",
    "not", "one", "our", "out", "over", "previous", "remember", "session", "sessions", "should",
    "some", "talked", "than", "that", "the", "their", "them", "then", "there", "these", "they",
    "thing", "things", "this", "those", "time", "was", "were", "what", "when", "where", "which",
    "while", "who", "why", "will", "with", "worked", "would", "you", "your",
];

#[derive(Debug, Clone)]
pub struct RecallOptions {
    pub source: SourceSelector,
    pub limit: usize,
    pub project: Option<String>,
    pub since: Option<String>,
    pub keywords: Vec<String>,
    pub candidates: usize,
    pub threshold: f64,
    pub model: String,
    pub max_parallel: usize,
    pub no_index: bool,
}

impl Default for RecallOptions {
    fn default() -> Self {
        RecallOptions {
            source: SourceSelector::All,
            limit: DEFAULT_RECALL_LIMIT,
            project: None,
            since: None,
            keywords: Vec::new(),
            candidates: DEFAULT_RECALL_CANDIDATES,
            threshold: DEFAULT_RECALL_THRESHOLD,
            model: DEFAULT_RECALL_MODEL.into(),
            max_parallel: crate::DEFAULT_MAX_PARALLEL,
            no_index: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecallHit {
    /// Jev's probability that this session is the one described, rounded to 3 places.
    pub relevance: f64,
    pub source: TranscriptSource,
    pub path: String,
    pub project: String,
    pub date: String,
    /// Retrieval terms this session matched.
    pub keywords: Vec<String>,
    pub opening_prompt: String,
    pub matches: Vec<FindMatch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecallResult {
    pub query: String,
    pub keywords: Vec<String>,
    /// The versioned Jev model that answered; the requested id when nothing was judged.
    pub model: String,
    pub threshold: f64,
    /// Sessions sent to Jev.
    pub candidates: usize,
    pub hits: Vec<RecallHit>,
    pub input_tokens: u64,
    pub cost_usd: f64,
    pub skipped_stores: Vec<StoreDiagnostic>,
    pub elapsed_ms: u64,
}

/// Retrieval terms: `extra` first, then up to six description words, longest
/// first. Words shorter than three characters, numbers, and stopwords drop out.
pub fn keywords(description: &str, extra: &[String]) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for term in extra {
        let term = js_trim(term);
        let lowered = js_lower(term).into_owned();
        if !term.is_empty() && seen.insert(lowered) {
            out.push(term.to_string());
        }
    }
    let mut words: Vec<String> = Vec::new();
    for raw in description.split(|c: char| !(c.is_alphanumeric() || "-_./".contains(c))) {
        let word = js_lower(raw.trim_matches(|c: char| "-_./".contains(c))).into_owned();
        if word.chars().count() < 3
            || word.chars().all(|c| c.is_ascii_digit())
            || STOPWORDS.contains(&word.as_str())
            || seen.contains(&word)
        {
            continue;
        }
        seen.insert(word.clone());
        words.push(word);
    }
    // Stable: equal lengths keep description order.
    words.sort_by_key(|word| std::cmp::Reverse(word.chars().count()));
    words.truncate(MAX_DESCRIPTION_KEYWORDS);
    out.extend(words);
    out
}

/// A session found by at least one retrieval term.
#[derive(Debug, Clone)]
struct Candidate {
    hit: FindHit,
    keywords: Vec<String>,
    score: usize,
}

/// Union of per-term `find` hits, ranked by how many terms a session matched,
/// then by `find`'s weighted score, then by recency.
fn merge_candidates(per_term: Vec<(String, FindResult)>, cap: usize) -> Vec<Candidate> {
    let mut by_path: HashMap<String, Candidate> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for (term, result) in per_term {
        for hit in result.hits {
            match by_path.get_mut(&hit.path) {
                Some(existing) => {
                    existing.keywords.push(term.clone());
                    existing.score += hit.score;
                    for found in hit.matches {
                        if existing.hit.matches.len() < MAX_EXCERPTS
                            && !existing.hit.matches.iter().any(|m| m.text == found.text)
                        {
                            existing.hit.matches.push(found);
                        }
                    }
                }
                None => {
                    order.push(hit.path.clone());
                    by_path.insert(
                        hit.path.clone(),
                        Candidate {
                            score: hit.score,
                            keywords: vec![term.clone()],
                            hit,
                        },
                    );
                }
            }
        }
    }
    let mut candidates: Vec<Candidate> = order
        .into_iter()
        .filter_map(|path| by_path.remove(&path))
        .collect();
    candidates.sort_by(|a, b| {
        b.keywords
            .len()
            .cmp(&a.keywords.len())
            .then(b.score.cmp(&a.score))
            .then(b.hit.date.cmp(&a.hit.date))
    });
    candidates.truncate(cap);
    candidates
}

/// Key of the `i`th session in a judgment batch.
fn session_key(i: usize) -> String {
    format!("s{i:02}")
}

/// The text Jev reads for one session.
fn session_card(hit: &FindHit) -> String {
    let mut card = format!("{} · {} · {}", hit.source.as_str(), hit.project, hit.date);
    if !hit.opening_prompt.is_empty() {
        card.push_str(&format!("\nOpened with: {}", hit.opening_prompt));
    }
    if !hit.matches.is_empty() {
        card.push_str("\nExcerpts:");
        for found in &hit.matches {
            card.push_str(&format!("\n[{}] {}", found.role, found.text));
        }
    }
    card
}

/// One Jev request: one noul per session over a shared state.
fn judgment_request(query: &str, model: &str, batch: &[Candidate]) -> Value {
    let mut sessions = Map::new();
    let mut questions = Map::new();
    for (i, candidate) in batch.iter().enumerate() {
        let key = session_key(i);
        sessions.insert(key.clone(), Value::String(session_card(&candidate.hit)));
        questions.insert(
            key.clone(),
            json!({
                "type": "noul",
                "instructions": format!(
                    "Is `sessions.{key}` a past session where the work in `search` happened: \"{query}\"? Apply `criteria`."
                ),
            }),
        );
    }
    // Keys in lexicographic order, as omp's find sends them.
    json!({
        "model": model,
        "questions": questions,
        "state": {
            "criteria": {
                "no": "The session only shares keywords with the search, mentions the topic in passing, or worked on something else.",
                "yes": "The session's opening request or excerpts show it did, discussed, or decided the work the search describes. Excerpts are partial: favor recall.",
            },
            "search": query,
            "sessions": sessions,
            "task": TASK,
        },
    })
}

/// Relevance per batch position, the answering model, and input tokens.
fn read_judgment(response: &Value, count: usize) -> Result<(Vec<f64>, String, u64), String> {
    let answers = response
        .get("answers")
        .and_then(Value::as_object)
        .ok_or("Jev response has no answers")?;
    let scores = (0..count)
        .map(|i| {
            answers
                .get(&session_key(i))
                .and_then(|answer| answer.get("noul"))
                .and_then(Value::as_f64)
                .ok_or_else(|| format!("Jev response has no answer for {}", session_key(i)))
        })
        .collect::<Result<Vec<f64>, String>>()?;
    let model = response
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let tokens = response
        .get("usage")
        .and_then(|usage| usage.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Ok((scores, model, tokens))
}

/// The recall pipeline with retrieval and judgment injected: `retrieve` runs
/// `find` for one term; `judge` sends one Jev request body and returns its response.
pub fn recall_with(
    description: &str,
    options: &RecallOptions,
    retrieve: &(dyn Fn(&str) -> Result<FindResult, String> + Sync),
    judge: &(dyn Fn(&Value) -> Result<Value, String> + Sync),
) -> Result<RecallResult, String> {
    let query = js_trim(description).to_string();
    if query.is_empty() {
        return Err("recall needs a description".into());
    }
    if options.limit < 1 {
        return Err(format!(
            "limit must be an integer >= 1 (got '{}')",
            options.limit
        ));
    }
    if options.candidates < 1 {
        return Err(format!(
            "candidates must be an integer >= 1 (got '{}')",
            options.candidates
        ));
    }
    if !(0.0..=1.0).contains(&options.threshold) {
        return Err(format!(
            "threshold must be between 0 and 1 (got '{}')",
            options.threshold
        ));
    }
    let started = Instant::now();
    let terms = keywords(&query, &options.keywords);
    if terms.is_empty() {
        return Err(
            "recall found no searchable words in the description; add terms with -k".into(),
        );
    }

    let mut per_term = Vec::new();
    let mut skipped: Vec<StoreDiagnostic> = Vec::new();
    for term in &terms {
        let mut result = retrieve(term)?;
        for diagnostic in result.skipped_stores.drain(..) {
            if !skipped.iter().any(|seen| seen.path == diagnostic.path) {
                skipped.push(diagnostic);
            }
        }
        per_term.push((term.clone(), result));
    }
    let candidates = merge_candidates(per_term, options.candidates);

    let batches: Vec<&[Candidate]> = candidates.chunks(BATCH_SIZE).collect();
    let judged = map_pool(&batches, options.max_parallel, |batch, _| {
        let request = judgment_request(&query, &options.model, batch);
        judge(&request).and_then(|response| read_judgment(&response, batch.len()))
    })?;

    let mut model = String::new();
    let mut input_tokens = 0;
    let mut hits: Vec<RecallHit> = Vec::new();
    for (batch, outcome) in batches.iter().zip(judged) {
        let (scores, answered_by, tokens) = outcome?;
        if model.is_empty() {
            model = answered_by;
        }
        input_tokens += tokens;
        for (candidate, score) in batch.iter().zip(scores) {
            if score < options.threshold {
                continue;
            }
            let hit = &candidate.hit;
            hits.push(RecallHit {
                relevance: (score * 1000.0).round() / 1000.0,
                source: hit.source,
                path: hit.path.clone(),
                project: hit.project.clone(),
                date: hit.date.clone(),
                keywords: candidate.keywords.clone(),
                opening_prompt: hit.opening_prompt.clone(),
                matches: hit.matches.clone(),
                resume: hit.resume.clone(),
            });
        }
    }
    hits.sort_by(|a, b| {
        b.relevance
            .total_cmp(&a.relevance)
            .then(b.date.cmp(&a.date))
    });
    hits.truncate(options.limit);
    if model.is_empty() {
        model = options.model.clone();
    }
    Ok(RecallResult {
        query,
        keywords: terms,
        model,
        threshold: options.threshold,
        candidates: candidates.len(),
        hits,
        input_tokens,
        cost_usd: (input_tokens as f64 * USD_PER_INPUT_TOKEN * 1_000_000.0).round() / 1_000_000.0,
        skipped_stores: skipped,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

/// `TYPESAFE_API_KEY`, else the key omp stored with `omp login typesafe`.
pub fn typesafe_key() -> Result<String, String> {
    if let Some(key) = std::env::var("TYPESAFE_API_KEY")
        .ok()
        .filter(|key| !key.trim().is_empty())
    {
        return Ok(key.trim().to_string());
    }
    let from_omp = std::process::Command::new("omp")
        .args(["token", "typesafe"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|key| !key.is_empty());
    from_omp.ok_or_else(|| {
        "recall needs a TypeSafe API key: set TYPESAFE_API_KEY or run `omp login typesafe`".into()
    })
}

/// POST one Jev request, retrying rate limits, overloads, and server errors.
pub fn post_judgment(base_url: &str, key: &str, body: &Value) -> Result<Value, String> {
    let url = format!("{}/v1/systemone", base_url.trim_end_matches('/'));
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        .http_status_as_error(false)
        .build()
        .into();
    let payload = body.to_string();
    let mut last_error = String::new();
    for attempt in 0..ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(500 << (attempt - 1)));
        }
        let sent = agent
            .post(&url)
            .header("Authorization", &format!("Bearer {key}"))
            .header("Content-Type", "application/json")
            .send(&payload);
        let mut response = match sent {
            Ok(response) => response,
            Err(error) => {
                last_error = format!("TypeSafe request failed: {error}");
                continue;
            }
        };
        let status = response.status().as_u16();
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(|e| format!("TypeSafe response unreadable: {e}"))?;
        if status == 200 {
            return serde_json::from_str(&text)
                .map_err(|e| format!("TypeSafe response is not JSON: {e}"));
        }
        last_error = format!(
            "TypeSafe returned HTTP {status}: {}",
            js::slice(&text, 0, 300)
        );
        if !(status == 429 || status == 529 || status >= 500) {
            break;
        }
    }
    Err(last_error)
}

/// Production recall: `find` over local stores, Jev over HTTPS.
pub fn recall(
    description: &str,
    options: &RecallOptions,
    backend: &dyn Backend,
) -> Result<RecallResult, String> {
    let base_url = std::env::var("TYPESAFE_BASE_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_BASE_URL.into());
    // Resolved on the first judgment only, so zero candidates need no key.
    let key = std::sync::LazyLock::new(typesafe_key);
    let find_options = FindOptions {
        source: options.source,
        limit: options.candidates,
        project: options.project.clone(),
        since: options.since.clone(),
        user_only: false,
        max_parallel: options.max_parallel,
        no_index: options.no_index,
    };
    let retrieve = |term: &str| find::find_sessions(&[term.to_string()], &find_options, backend);
    let judge = |body: &Value| {
        let key = (*key).clone()?;
        post_judgment(&base_url, &key, body)
    };
    recall_with(description, options, &retrieve, &judge)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::find::JsObject;
    use std::sync::Mutex;

    fn hit(path: &str, date: &str, score: usize, excerpt: &str) -> FindHit {
        FindHit {
            source: TranscriptSource::Claude,
            path: path.into(),
            project: "work".into(),
            date: date.into(),
            score,
            term_counts: JsObject::default(),
            opening_prompt: format!("opening of {path}"),
            matches: vec![FindMatch {
                role: "user".into(),
                date: None,
                text: excerpt.into(),
            }],
            resume: None,
        }
    }

    fn found(hits: Vec<FindHit>) -> FindResult {
        FindResult {
            terms: Vec::new(),
            required_terms: Vec::new(),
            sources: Vec::new(),
            hits,
            skipped_stores: Vec::new(),
            elapsed_ms: 0,
            store_timings: JsObject::default(),
            truncated: false,
        }
    }

    #[test]
    fn keywords_keep_explicit_terms_then_the_longest_description_words() {
        assert_eq!(
            keywords(
                "Where did we set up the Factory broker and its WAF quarantine fix in 2026?",
                &["fk-".into(), "Broker".into()]
            ),
            [
                "fk-",
                "Broker",
                "quarantine",
                "factory",
                "set",
                "waf",
                "fix"
            ]
        );
        assert_eq!(
            keywords("the gpt-6.1-sol context window", &[]),
            ["gpt-6.1-sol", "context", "window"]
        );
        assert!(keywords("what did we do", &[]).is_empty());
    }

    #[test]
    fn candidates_rank_by_terms_matched_then_score_and_merge_excerpts() {
        let per_term = vec![
            (
                "broker".to_string(),
                found(vec![
                    hit("a", "2026-01-01", 50, "broker one"),
                    hit("b", "2026-02-01", 5, "broker two"),
                ]),
            ),
            (
                "factory".to_string(),
                found(vec![
                    hit("b", "2026-02-01", 5, "factory two"),
                    hit("c", "2026-03-01", 90, "factory three"),
                ]),
            ),
        ];
        let merged = merge_candidates(per_term, 10);
        let order: Vec<&str> = merged.iter().map(|c| c.hit.path.as_str()).collect();
        assert_eq!(order, ["b", "c", "a"]);
        assert_eq!(merged[0].keywords, ["broker", "factory"]);
        let excerpts: Vec<&str> = merged[0]
            .hit
            .matches
            .iter()
            .map(|m| m.text.as_str())
            .collect();
        assert_eq!(excerpts, ["broker two", "factory two"]);
        assert_eq!(
            merge_candidates(vec![("x".into(), found(vec![hit("a", "d", 1, "x")]))], 0).len(),
            0
        );
    }

    #[test]
    fn recall_judges_in_batches_thresholds_and_ranks_by_relevance() {
        // 23 candidates for one term: three batches of 10, 10, and 3.
        let hits: Vec<FindHit> = (0..23)
            .map(|i| {
                hit(
                    &format!("p{i}"),
                    &format!("2026-01-{:02}", i + 1),
                    23 - i,
                    "broker",
                )
            })
            .collect();
        let retrieve = |_: &str| Ok(found(hits.clone()));
        let requests = Mutex::new(Vec::new());
        let judge = |body: &Value| {
            let sessions = body["state"]["sessions"].as_object().unwrap();
            requests.lock().unwrap().push(sessions.len());
            let mut answers = Map::new();
            for (key, card) in sessions {
                // p7 is the described session; p12 is borderline; the rest are noise.
                let noul = if card.as_str().unwrap().contains("opening of p7\n") {
                    0.97
                } else if card.as_str().unwrap().contains("opening of p12\n") {
                    0.31
                } else {
                    0.04
                };
                answers.insert(key.clone(), json!({"type": "noul", "noul": noul}));
            }
            Ok(json!({"model": "jev-1.13.0", "answers": answers, "usage": {"input_tokens": 1000}}))
        };
        let options = RecallOptions {
            candidates: 40,
            ..RecallOptions::default()
        };
        let result = recall_with("broker", &options, &retrieve, &judge).unwrap();
        let mut sizes = requests.into_inner().unwrap();
        sizes.sort_unstable();
        assert_eq!(sizes, [3, 10, 10]);
        let ranked: Vec<(&str, f64)> = result
            .hits
            .iter()
            .map(|h| (h.path.as_str(), h.relevance))
            .collect();
        assert_eq!(ranked, [("p7", 0.97), ("p12", 0.31)]);
        assert_eq!(result.candidates, 23);
        assert_eq!(result.model, "jev-1.13.0");
        assert_eq!(result.input_tokens, 3000);
        assert_eq!(result.cost_usd, 0.000126);
    }

    #[test]
    fn recall_skips_the_judge_without_candidates_and_surfaces_judge_errors() {
        let empty = |_: &str| Ok(found(Vec::new()));
        let never =
            |_: &Value| -> Result<Value, String> { panic!("judge called without candidates") };
        let result =
            recall_with("broker setup", &RecallOptions::default(), &empty, &never).unwrap();
        assert_eq!((result.candidates, result.hits.len()), (0, 0));
        assert_eq!(result.model, DEFAULT_RECALL_MODEL);

        let one = |_: &str| Ok(found(vec![hit("a", "d", 1, "broker")]));
        let failing =
            |_: &Value| -> Result<Value, String> { Err("TypeSafe returned HTTP 401".into()) };
        assert_eq!(
            recall_with("broker", &RecallOptions::default(), &one, &failing).unwrap_err(),
            "TypeSafe returned HTTP 401"
        );
        let partial = |_: &Value| Ok(json!({"answers": {}}));
        assert!(
            recall_with("broker", &RecallOptions::default(), &one, &partial)
                .unwrap_err()
                .contains("no answer for s00")
        );
    }
}
