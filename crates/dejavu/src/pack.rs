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
use crate::js;
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
use std::collections::BTreeSet;

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
    pub omitted: Vec<OmittedSession>,
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
pub(crate) fn is_uuid(value: &str) -> bool {
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

/// The running agent's session ids, so a lookup skips the session asking.
/// Claude Code exports `CLAUDE_CODE_SESSION_ID`; `CLAUDE_SESSION_ID` is kept
/// for wrappers that set the older name. agy exports `ANTIGRAVITY_CONVERSATION_ID`
/// to the commands it runs. Droid exports none, so its session is inferred
/// from the process tree.
pub(crate) fn active_session_ids() -> Vec<String> {
    [
        "CODEX_THREAD_ID",
        "CLAUDE_CODE_SESSION_ID",
        "CLAUDE_SESSION_ID",
        "ANTIGRAVITY_CONVERSATION_ID",
    ]
    .iter()
    .filter_map(|key| std::env::var(key).ok())
    .filter(|value| !value.is_empty())
    .chain(crate::droid_active::active_droid_session())
    .collect()
}

/// Whether `value` (a locator, file stem, or session id) names this transcript.
pub(crate) fn excluded(path: &str, source: TranscriptSource, value: &str) -> bool {
    path == value
        || basename_without(path, ".jsonl") == value
        || (source == TranscriptSource::Opencode
            && path.ends_with(&format!("#{}", encode_uri_component(value))))
        || (source == TranscriptSource::Agy && crate::agy::conversation_id(path) == Some(value))
        || (is_uuid(value)
            && (path.ends_with(&format!("{value}.jsonl")) || path.ends_with(&format!("/{value}"))))
}

/// Omitted match neighborhoods and events in a loaded session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OmittedSession {
    pub path: String,
    pub neighborhoods: usize,
    pub events: usize,
    pub next_event: usize,
}

struct Neighborhood {
    anchor: usize,
    events: Vec<usize>,
    matches: usize,
    chars: usize,
}

struct MatchedView {
    view: TranscriptView,
    neighborhoods: Vec<Neighborhood>,
}

/// Rank dialogue neighborhoods by weighted match density. Tool and reasoning
/// events do not compete with dialogue or consume its context radius.
fn keep_matching(mut view: TranscriptView, needles: &[String], context: usize) -> MatchedView {
    for (position, event) in view.events.iter_mut().enumerate() {
        event.index.get_or_insert(position);
    }
    view.events.retain(|event| {
        matches!(
            event.body,
            crate::view::EventBody::User { .. } | crate::view::EventBody::Assistant { .. }
        )
    });
    let metrics: Vec<(usize, usize)> = view
        .events
        .iter()
        .map(|event| {
            let body = event_body(event);
            let lower = js_lower(&body);
            let weight = if event.kind() == "user" { 2 } else { 1 };
            let matches = needles
                .iter()
                .filter(|term| !term.is_empty())
                .map(|term| lower.matches(term.as_str()).count().min(8))
                .sum::<usize>();
            (matches * weight, js::len(&body).max(200))
        })
        .collect();
    let mut neighborhoods = Vec::new();
    let mut keep = BTreeSet::new();
    for (position, &(matches, _)) in metrics.iter().enumerate() {
        if matches == 0 {
            continue;
        }
        let start = position.saturating_sub(context);
        let end = position.saturating_add(context).min(view.events.len() - 1);
        let events: Vec<usize> = view.events[start..=end]
            .iter()
            .map(|event| event.index.unwrap())
            .collect();
        keep.extend(events.iter().copied());
        neighborhoods.push(Neighborhood {
            anchor: view.events[position].index.unwrap(),
            events,
            matches: metrics[start..=end].iter().map(|m| m.0).sum(),
            chars: metrics[start..=end].iter().map(|m| m.1).sum(),
        });
    }
    neighborhoods.sort_by(|a, b| {
        ((b.matches as u128) * (a.chars as u128))
            .cmp(&((a.matches as u128) * (b.chars as u128)))
            .then_with(|| a.anchor.cmp(&b.anchor))
    });
    view.events
        .retain(|event| keep.contains(&event.index.unwrap()));
    MatchedView {
        view,
        neighborhoods,
    }
}

/// Select before dividing the body budget. The per-event metadata allowance
/// prevents small bodies from creating an arbitrarily large JSON envelope.
fn choose_events(session: &MatchedView, budget: usize, max_chars: usize) -> BTreeSet<usize> {
    if budget == 0 {
        return BTreeSet::new();
    }
    let minimum = max_chars.min(200);
    let event_limit = (budget / (minimum + 200)).max(1);
    let anchors: BTreeSet<usize> = session.neighborhoods.iter().map(|n| n.anchor).collect();
    let mut keep = BTreeSet::new();
    for neighborhood in &session.neighborhoods {
        if keep.len() >= event_limit {
            break;
        }
        keep.insert(neighborhood.anchor);
        let mut neighbors = neighborhood.events.clone();
        neighbors.sort_by_key(|&id| (!anchors.contains(&id), id.abs_diff(neighborhood.anchor), id));
        for id in neighbors {
            if keep.len() >= event_limit {
                break;
            }
            keep.insert(id);
        }
    }
    keep
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
    let mut selected: Vec<MatchedView> = Vec::new();
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
                Ok(kept) if !kept.view.events.is_empty() => selected.push(kept),
                Ok(_) => {}
                Err(error) => skipped_sessions.push(SkippedSession {
                    path: path.to_string(),
                    error,
                }),
            }
        }
    }

    let mut sessions = Vec::new();
    let mut omitted = Vec::new();
    let mut used_chars = 0;
    let total = selected.len();
    for (index, mut session) in selected.into_iter().enumerate() {
        let remaining = budget_chars - used_chars;
        let share = if remaining == 0 {
            0
        } else {
            (remaining / (total - index)).max(1)
        };
        let keep = choose_events(&session, share, max_chars);
        let available_events = session.view.events.len();
        let next_event = session
            .view
            .events
            .iter()
            .filter_map(|event| event.index.filter(|id| !keep.contains(id)))
            .min();
        if let Some(next_event) = next_event {
            omitted.push(OmittedSession {
                path: session.view.path.clone(),
                neighborhoods: session
                    .neighborhoods
                    .iter()
                    .filter(|n| !keep.contains(&n.anchor))
                    .count(),
                events: available_events - keep.len(),
                next_event,
            });
        }
        if keep.is_empty() {
            continue;
        }
        session
            .view
            .events
            .retain(|event| keep.contains(&event.index.unwrap()));
        let per_event = max_chars.min((share / keep.len()).max(1));
        let mut windowed = window_transcript(
            session.view,
            &WindowOptions {
                budget_chars: Some(share),
                max_chars: Some(per_event),
                focus_terms: found.required_terms.clone(),
                ..WindowOptions::default()
            },
        )?;
        windowed.window.available_events = available_events;
        windowed.window.next_event = next_event;
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
        omitted,
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
    let omitted = if result.omitted.is_empty() {
        String::new()
    } else {
        format!(
            "\nOmitted: {} match neighborhoods, {} events. Continue with transcript <locator> --from-event N.",
            result
                .omitted
                .iter()
                .map(|s| s.neighborhoods)
                .sum::<usize>(),
            result.omitted.iter().map(|s| s.events).sum::<usize>()
        )
    };
    let continuations = result
        .omitted
        .iter()
        .filter(|omitted| {
            !result
                .sessions
                .iter()
                .any(|session| session.view.path == omitted.path)
        })
        .map(|omitted| {
            format!(
                "\nTranscript: {} · --from-event {}",
                omitted.path, omitted.next_event
            )
        })
        .collect::<String>();
    format!(
        "{header}{relaxed}{omitted}\n\n{}{continuations}",
        sessions.join("\n\n---\n\n")
    )
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
            truncated: false,
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
            budget_chars: Some(3000),
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
        assert!(result.used_chars <= 3000);
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
    fn small_budget_returns_useful_excerpts_and_reports_omitted_neighborhoods() {
        let result = pack_sessions(
            &terms(&["needle"]),
            &PackOptions {
                limit: 1,
                budget_chars: Some(1200),
                ..PackOptions::default()
            },
            &[],
            |_, _| Ok(found(&["/a"])),
            &|path| {
                Ok(view(
                    path,
                    (0..75)
                        .map(|i| {
                            let text = format!("{}NEEDLE{}", "x".repeat(3000), "y".repeat(500));
                            if i % 2 == 0 {
                                user(i * 3, &text)
                            } else {
                                assistant(i * 3, &text)
                            }
                        })
                        .collect(),
                ))
            },
        )
        .unwrap();
        let session = &result.sessions[0];
        assert_eq!(session.view.events.len(), 3);
        assert_eq!(session.window.available_events, 75);
        assert_eq!(session.window.used_chars, 1200);
        for event in &session.view.events {
            let body = event_body(event);
            assert!(body.contains("NEEDLE"));
            assert_eq!(js::len(&body), 400);
        }
        let returned: BTreeSet<_> = session
            .view
            .events
            .iter()
            .map(|e| e.index.unwrap())
            .collect();
        let next = (0..75)
            .map(|i| i * 3)
            .find(|i| !returned.contains(i))
            .unwrap();
        assert_eq!(session.window.next_event, Some(next));
        assert_eq!(
            result.omitted,
            [OmittedSession {
                path: "/a".into(),
                neighborhoods: 72,
                events: 72,
                next_event: next
            }]
        );
        assert!(js::stringify(&result).len() < 3600);
        assert!(render_pack(&result).contains("Omitted: 72 match neighborhoods, 72 events"));
    }

    #[test]
    fn density_and_user_dialogue_win_over_long_matches_and_tool_noise() {
        let result = pack_sessions(
            &terms(&["needle"]),
            &PackOptions {
                context: Some(0),
                budget_chars: Some(200),
                ..PackOptions::default()
            },
            &[],
            |_, _| Ok(found(&["/a"])),
            &|path| {
                Ok(view(
                    path,
                    vec![
                        user(0, &format!("needle{}", "x".repeat(4000))),
                        event(
                            EventBody::ToolResult {
                                name: None,
                                call_id: None,
                                output: "needle ".repeat(500),
                                is_error: false,
                            },
                            5,
                        ),
                        assistant(10, "needle answer"),
                        user(15, "needle question"),
                    ],
                ))
            },
        )
        .unwrap();
        assert_eq!(result.sessions[0].view.events[0].index, Some(15));
        assert_eq!(result.sessions[0].window.available_events, 3);
        assert_eq!(result.sessions[0].window.next_event, Some(0));
        assert_eq!(result.omitted[0].events, 2);
    }

    #[test]
    fn tiny_budget_reports_entirely_omitted_sessions() {
        let result = pack_sessions(
            &terms(&["needle"]),
            &PackOptions {
                budget_chars: Some(1),
                ..PackOptions::default()
            },
            &[],
            |_, _| Ok(found(&["/a", "/b"])),
            &|path| {
                Ok(view(
                    path,
                    vec![user(7, "needle"), assistant(12, "needle answer")],
                ))
            },
        )
        .unwrap();
        assert_eq!(result.used_chars, 1);
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].window.next_event, Some(12));
        assert_eq!(result.omitted.len(), 2);
        assert_eq!(
            result.omitted[1],
            OmittedSession {
                path: "/b".into(),
                neighborhoods: 2,
                events: 2,
                next_event: 7
            }
        );
        assert!(render_pack(&result).contains("Transcript: /b · --from-event 7"));
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
        let agy = format!("/x/brain/{id}/.system_generated/logs/transcript_full.jsonl");
        assert!(excluded(&agy, TranscriptSource::Agy, id));
        assert!(!excluded(&agy, TranscriptSource::Claude, id));
    }
}
