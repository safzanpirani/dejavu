//! Model-free search packs (`pack.ts`): `find`'s ranked sessions, reduced to
//! the dialogue events around each match and fitted to one character budget.
//!
//! The TypeScript loaded the full transcript view of every ranked candidate
//! (up to `find`'s cap of 40) at once and kept them all in memory before taking
//! the first `limit` that had a match. A few large candidates held over a
//! gigabyte and the command did not finish. Here candidates load in rank order,
//! at most as many at once as sessions are still needed (and `max_parallel`),
//! each view is reduced to its kept events as soon as it loads, and loading
//! stops once `limit` sessions have excerpts. The selected sessions are the
//! same; only candidates past that point go unexamined.

use crate::find::{FindOptions, FindResult};
use crate::opencode::encode_uri_component;
use crate::paths::js_lower;
use crate::pool::map_pool;
use crate::render::{RenderTranscriptOptions, render_transcript};
use crate::types::{StoreDiagnostic, TranscriptSource};
use crate::view::{TranscriptView, TranscriptViewOptions, view_transcript};
use crate::window::{
    WindowOptions, WindowedTranscript, event_body, render_window, validate_bound, window_transcript,
};
use serde::Serialize;

/// `find`'s ranked-candidate cap, which pack always searches up to.
const SEARCH_CAP: usize = 40;

#[derive(Debug, Clone)]
pub struct PackOptions {
    /// Source, project, since, user, no-index, and max-parallel; `limit` is ignored.
    pub find: FindOptions,
    pub limit: usize,
    pub budget_chars: Option<usize>,
    pub max_chars: Option<usize>,
    pub context: Option<usize>,
    pub exclude_sessions: Vec<String>,
}

impl Default for PackOptions {
    fn default() -> Self {
        PackOptions {
            find: FindOptions::default(),
            limit: 3,
            budget_chars: None,
            max_chars: None,
            context: None,
            exclude_sessions: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkippedSession {
    pub path: String,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PackResult {
    pub terms: Vec<String>,
    pub required_terms: Vec<String>,
    pub budget_chars: usize,
    pub used_chars: usize,
    pub candidate_count: usize,
    pub excluded_count: usize,
    pub sessions: Vec<WindowedTranscript>,
    pub skipped_stores: Vec<StoreDiagnostic>,
    pub skipped_sessions: Vec<SkippedSession>,
}

/// `path.basename(path, ext)`.
fn basename_without(path: &str, ext: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    let base = trimmed.rsplit('/').next().unwrap_or(trimmed);
    match base.strip_suffix(ext) {
        Some(stem) if !stem.is_empty() => stem.to_string(),
        _ => base.to_string(),
    }
}

/// `/^[a-f\d]{8}(?:-[a-f\d]{4}){3}-[a-f\d]{12}$/i`
fn is_uuid(value: &str) -> bool {
    let b = value.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, &c)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                c == b'-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}

fn excluded(path: &str, source: TranscriptSource, value: &str) -> bool {
    path == value
        || basename_without(path, ".jsonl") == value
        || (source == TranscriptSource::Opencode
            && path.ends_with(&format!("#{}", encode_uri_component(value))))
        || (is_uuid(value)
            && (path.ends_with(&format!("{value}.jsonl")) || path.ends_with(&format!("/{value}"))))
}

/// The events within `context` of an event whose body contains a needle.
fn keep_matching(mut view: TranscriptView, needles: &[String], context: usize) -> TranscriptView {
    let count = view.events.len();
    let mut keep = vec![false; count];
    for (index, event) in view.events.iter().enumerate() {
        let body = js_lower(&event_body(event)).into_owned();
        if !needles.iter().any(|needle| body.contains(needle.as_str())) {
            continue;
        }
        let end = (index + context).min(count.saturating_sub(1));
        for slot in &mut keep[index.saturating_sub(context)..=end] {
            *slot = true;
        }
    }
    let mut flags = keep.into_iter();
    view.events.retain(|_| flags.next().unwrap_or(false));
    view
}

/// `packSessions(terms, options)`. `find` ranks sessions (called with
/// `find`'s cap as the limit); `view` loads one transcript without tools.
pub fn pack_sessions(
    terms: &[String],
    options: &PackOptions,
    env_exclusions: &[String],
    find: impl FnOnce(&[String], &FindOptions) -> Result<FindResult, String>,
    view: &(dyn Fn(&str) -> Result<TranscriptView, String> + Sync),
) -> Result<PackResult, String> {
    let limit = options.limit;
    let budget_chars = options.budget_chars.unwrap_or(12_000);
    let max_chars = options.max_chars.unwrap_or(1200);
    let context = options.context.unwrap_or(2);
    validate_bound(Some(limit), "--limit", 1)?;
    validate_bound(Some(budget_chars), "--budget-chars", 1)?;
    validate_bound(Some(max_chars), "--max-chars", 1)?;
    let find_options = FindOptions {
        limit: SEARCH_CAP,
        ..options.find.clone()
    };
    let found = find(terms, &find_options)?;
    let exclusions: Vec<&str> = options
        .exclude_sessions
        .iter()
        .chain(env_exclusions)
        .map(String::as_str)
        .filter(|value| !value.is_empty())
        .collect();
    let candidates: Vec<&str> = found
        .hits
        .iter()
        .filter(|hit| {
            !exclusions
                .iter()
                .any(|value| excluded(&hit.path, hit.source, value))
        })
        .map(|hit| hit.path.as_str())
        .collect();
    let needles: Vec<String> = found
        .required_terms
        .iter()
        .map(|term| js_lower(term).into_owned())
        .collect();

    // Load in rank order, at most as many at once as sessions are still
    // needed (and --max-parallel), until `limit` sessions have excerpts.
    let mut selected: Vec<TranscriptView> = Vec::new();
    let mut skipped_sessions = Vec::new();
    let mut next = 0;
    while selected.len() < limit && next < candidates.len() {
        let width = (limit - selected.len())
            .min(options.find.max_parallel)
            .max(1);
        let chunk = &candidates[next..(next + width).min(candidates.len())];
        next += chunk.len();
        let loaded = map_pool(chunk, width, |path, _| {
            view(path).map(|loaded| keep_matching(loaded, &needles, context))
        })?;
        for (path, outcome) in chunk.iter().zip(loaded) {
            match outcome {
                Ok(kept) if !kept.events.is_empty() => selected.push(kept),
                Ok(_) => {}
                Err(error) => skipped_sessions.push(SkippedSession {
                    path: path.to_string(),
                    error,
                }),
            }
        }
    }

    let mut sessions = Vec::new();
    let mut used_chars = 0;
    let total = selected.len();
    for (index, session) in selected.into_iter().enumerate() {
        if used_chars >= budget_chars {
            break;
        }
        let share = ((budget_chars - used_chars) / (total - index)).max(1);
        // Reserve space for each selected neighbor so a long preceding turn cannot consume the match's budget.
        let per_event = max_chars.min((share / session.events.len()).max(1));
        let windowed = window_transcript(
            session,
            &WindowOptions {
                budget_chars: Some(share),
                max_chars: Some(per_event),
                focus_terms: found.required_terms.clone(),
                ..WindowOptions::default()
            },
        )?;
        used_chars += windowed.window.used_chars;
        sessions.push(windowed);
    }
    Ok(PackResult {
        terms: found.terms,
        required_terms: found.required_terms,
        budget_chars,
        used_chars,
        candidate_count: found.hits.len(),
        excluded_count: found.hits.len() - candidates.len(),
        sessions,
        skipped_stores: found.skipped_stores,
        skipped_sessions,
    })
}

/// `viewTranscript(path, { tools: false })`.
pub fn view_without_tools(path: &str) -> Result<TranscriptView, String> {
    view_transcript(
        path,
        TranscriptViewOptions {
            thinking: false,
            tools: false,
        },
    )
}

/// `renderPack(result)`.
pub fn render_pack(result: &PackResult) -> String {
    let header = format!(
        "{} sessions · {}/{} body chars · {} ranked candidates examined (search cap 40)",
        result.sessions.len(),
        result.used_chars,
        result.budget_chars,
        result.candidate_count
    );
    let relaxed = if result.required_terms.len() < result.terms.len() {
        format!("\nMatched subset: {}", result.required_terms.join(" + "))
    } else {
        String::new()
    };
    let sessions: Vec<String> = result
        .sessions
        .iter()
        .map(|session| {
            format!(
                "Transcript: {}\n{}\n{}",
                session.view.path,
                render_transcript(
                    &session.view,
                    RenderTranscriptOptions {
                        full: true,
                        color: false
                    }
                ),
                render_window(&session.window)
            )
        })
        .collect();
    format!("{header}{relaxed}\n\n{}", sessions.join("\n\n---\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::find::{FindHit, JsObject};
    use crate::view::{EventBody, TranscriptEvent, count_events};
    use std::sync::Mutex;

    fn hit(path: &str) -> FindHit {
        FindHit {
            source: TranscriptSource::Claude,
            path: path.into(),
            project: "/project".into(),
            date: "2026-09-08".into(),
            score: 1,
            term_counts: JsObject::default(),
            opening_prompt: String::new(),
            matches: Vec::new(),
            resume: None,
        }
    }

    fn found(paths: &[&str]) -> FindResult {
        FindResult {
            terms: vec!["needle".into()],
            required_terms: vec!["needle".into()],
            sources: vec![TranscriptSource::Claude],
            hits: paths.iter().map(|p| hit(p)).collect(),
            skipped_stores: Vec::new(),
            elapsed_ms: 0,
            store_timings: JsObject::default(),
        }
    }

    fn event(body: EventBody, index: usize) -> TranscriptEvent {
        let mut event = TranscriptEvent::new(body);
        event.index = Some(index);
        event
    }

    fn user(index: usize, text: &str) -> TranscriptEvent {
        event(EventBody::User { text: text.into() }, index)
    }

    fn assistant(index: usize, text: &str) -> TranscriptEvent {
        event(EventBody::Assistant { text: text.into() }, index)
    }

    fn view(path: &str, events: Vec<TranscriptEvent>) -> TranscriptView {
        TranscriptView {
            path: path.into(),
            source: TranscriptSource::Claude,
            project: "/project".into(),
            counts: count_events(&events),
            events,
        }
    }

    fn terms(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn deduplicates_neighbors_and_keeps_late_matches_visible_within_a_shared_budget() {
        let options = PackOptions {
            budget_chars: Some(300),
            max_chars: Some(100),
            context: Some(1),
            ..PackOptions::default()
        };
        let result = pack_sessions(
            &terms(&["needle"]),
            &options,
            &[],
            |_, _| Ok(found(&["/a", "/b"])),
            &|path| {
                Ok(view(
                    path,
                    vec![
                        user(0, &"x".repeat(3000)),
                        assistant(3, &format!("{} needle decision", "y".repeat(3000))),
                        user(6, "needle followup"),
                        assistant(9, "done"),
                    ],
                ))
            },
        )
        .unwrap();
        assert_eq!(result.sessions.len(), 2);
        assert!(result.used_chars <= 300);
        let mut total = 0;
        for session in &result.sessions {
            let indexes: Vec<Option<usize>> = session.view.events.iter().map(|e| e.index).collect();
            assert_eq!(indexes, [Some(0), Some(3), Some(6), Some(9)]);
            assert!(event_body(&session.view.events[1]).contains("needle"));
            let clipped = session
                .window
                .clipped
                .iter()
                .find(|c| c.index == 3)
                .unwrap();
            assert!(clipped.start_char > 0);
            total += session
                .view
                .events
                .iter()
                .map(|e| crate::js::len(&event_body(e)))
                .sum::<usize>();
        }
        assert_eq!(result.used_chars, total);
    }

    #[test]
    fn exclusions_happen_before_the_session_limit_and_unreadable_or_stale_candidates_do_not_prevent_useful_excerpts()
     {
        let options = PackOptions {
            limit: 1,
            context: Some(0),
            exclude_sessions: vec!["exclude".into()],
            ..PackOptions::default()
        };
        let result = pack_sessions(
            &terms(&["needle"]),
            &options,
            &[],
            |_, _| Ok(found(&["/exclude.jsonl", "/broken", "/stale", "/good"])),
            &|path| match path {
                "/exclude.jsonl" => panic!("excluded session was loaded"),
                "/broken" => Err("unreadable".into()),
                _ => Ok(view(
                    path,
                    vec![user(
                        9,
                        if path == "/good" {
                            "needle"
                        } else {
                            "another branch"
                        },
                    )],
                )),
            },
        )
        .unwrap();
        assert_eq!(result.excluded_count, 1);
        assert_eq!(
            result.skipped_sessions,
            [SkippedSession {
                path: "/broken".into(),
                error: "unreadable".into()
            }]
        );
        let paths: Vec<&str> = result
            .sessions
            .iter()
            .map(|s| s.view.path.as_str())
            .collect();
        assert_eq!(paths, ["/good"]);
        assert_eq!(result.sessions[0].view.events[0].index, Some(9));
    }

    #[test]
    fn empty_results_and_relaxed_terms_remain_explicit() {
        let result = pack_sessions(
            &terms(&["needle", "missing"]),
            &PackOptions::default(),
            &[],
            |_, _| {
                Ok(FindResult {
                    terms: terms(&["needle", "missing"]),
                    ..found(&[])
                })
            },
            &|_| panic!("should not load"),
        )
        .unwrap();
        assert!(result.sessions.is_empty());
        assert_eq!(result.required_terms, ["needle"]);
        assert_eq!(result.used_chars, 0);
    }

    #[test]
    fn stops_loading_once_the_limit_has_excerpts() {
        let loaded = Mutex::new(Vec::new());
        let paths: Vec<String> = (0..40).map(|i| format!("/s{i}")).collect();
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let options = PackOptions {
            limit: 1,
            find: FindOptions {
                max_parallel: 2,
                ..FindOptions::default()
            },
            ..PackOptions::default()
        };
        let result = pack_sessions(
            &terms(&["needle"]),
            &options,
            &[],
            |_, find_options| {
                assert_eq!(find_options.limit, 40);
                Ok(found(&refs))
            },
            &|path| {
                loaded.lock().unwrap().push(path.to_string());
                Ok(view(path, vec![user(0, "needle")]))
            },
        )
        .unwrap();
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].view.path, "/s0");
        assert_eq!(loaded.into_inner().unwrap(), ["/s0"]);
    }

    #[test]
    fn excludes_by_id_basename_and_opencode_session() {
        let id = "2d695265-2734-49cd-be54-3ac3d6480ce9";
        assert!(excluded(
            &format!("/x/{id}.jsonl"),
            TranscriptSource::Claude,
            id
        ));
        assert!(excluded(
            &format!("/x/rollout-2026-01-01T00-00-00-{id}.jsonl"),
            TranscriptSource::Codex,
            id
        ));
        assert!(!excluded(
            "opencode:///x.db#ses 1",
            TranscriptSource::Opencode,
            "ses 1"
        ));
        assert!(excluded(
            "opencode:///x.db#ses%201",
            TranscriptSource::Opencode,
            "ses 1"
        ));
        assert!(!excluded("/x/other.jsonl", TranscriptSource::Claude, id));
    }
}
