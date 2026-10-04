//! `dejavu last`: where a session left off. It picks a session (the newest in
//! a project, `find`'s best match for some terms, or a locator or session id)
//! and returns its card plus the dialogue tail that fits a character budget.

use crate::find::{
    FindOptions, find_opening_prompt, find_sessions, is_real_user_prompt, opening_preview,
    resume_command,
};
use crate::index::{ProjectFilter, RecentSession};
use crate::pack::{active_session_ids, excluded, is_uuid};
use crate::render::{RenderTranscriptOptions, render_transcript};
use crate::search::{Disk, read_transcript_project};
use crate::types::{SourceSelector, StoreDiagnostic, TranscriptSource};
use crate::view::{EventBody, TranscriptView, TranscriptViewOptions, view_transcript};
use crate::window::{WindowOptions, WindowedTranscript, render_window, window_transcript};
use serde::Serialize;

/// `find`'s ranked-candidate cap, which a term lookup searches up to.
const SEARCH_CAP: usize = 40;

/// How `last` chooses its sessions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selector {
    /// The newest sessions whose project passes the filter.
    Recent(ProjectFilter),
    /// `find`'s ranking for these terms in a project (any when `None`). With
    /// `widen`, no match there searches every project.
    Terms {
        terms: Vec<String>,
        project: Option<String>,
        widen: bool,
    },
    /// One transcript locator or session id.
    Locator(String),
}

#[derive(Debug, Clone)]
pub struct LastOptions {
    pub selector: Selector,
    pub source: SourceSelector,
    pub since: Option<String>,
    /// Card-only listing of up to `limit` sessions instead of one handoff.
    pub list: bool,
    pub limit: usize,
    /// Include tool calls and results in the tail.
    pub tools: bool,
    /// Dialogue events in the tail, newest last.
    pub turns: usize,
    pub budget_chars: usize,
    pub max_chars: usize,
    pub tool_chars: usize,
    pub exclude_sessions: Vec<String>,
}

impl Default for LastOptions {
    fn default() -> Self {
        LastOptions {
            selector: Selector::Recent(ProjectFilter::Any),
            source: SourceSelector::All,
            since: None,
            list: false,
            limit: 5,
            tools: false,
            turns: 12,
            budget_chars: 8000,
            max_chars: 1500,
            tool_chars: 400,
            exclude_sessions: Vec::new(),
        }
    }
}

/// One selected session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCard {
    pub path: String,
    pub source: TranscriptSource,
    pub project: String,
    /// The newest visible message's date.
    pub date: String,
    pub opening_prompt: String,
    /// The newest real user prompt; set with a tail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_request: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LastResult {
    /// The selected sessions, newest or best first.
    pub sessions: Vec<SessionCard>,
    /// Matching sessions before the limit, after exclusions.
    pub total: usize,
    pub excluded_count: usize,
    /// The first session's tail; absent for `--list`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tail: Option<WindowedTranscript>,
    /// The first tail event, so earlier turns read from `--from-event`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tail_start: Option<usize>,
    pub skipped_stores: Vec<StoreDiagnostic>,
}

/// A candidate before its card is built.
struct Candidate {
    path: String,
    source: TranscriptSource,
    date: String,
}

/// Where `last` gets sessions and transcripts.
pub trait LastDeps {
    /// Refreshes the index and lists sessions newest first.
    fn recent(
        &self,
        filter: &ProjectFilter,
        source: SourceSelector,
        since: Option<&str>,
    ) -> Result<(Vec<RecentSession>, Vec<StoreDiagnostic>), String>;
    /// Ranks sessions for terms the way `find` does.
    fn find(
        &self,
        terms: &[String],
        options: &FindOptions,
    ) -> Result<crate::find::FindResult, String>;
    /// The newest indexed transcript whose path contains a session id.
    fn path_for_id(&self, id: &str) -> Result<Option<String>, String>;
    fn source_of(&self, locator: &str) -> Result<TranscriptSource, String>;
    fn view(&self, locator: &str, tools: bool) -> Result<TranscriptView, String>;
    fn project(&self, path: &str, source: TranscriptSource) -> String;
    fn opening_prompt(&self, path: &str, source: TranscriptSource) -> String;
}

/// The real stores and index.
pub struct RealLast;

impl LastDeps for RealLast {
    fn recent(
        &self,
        filter: &ProjectFilter,
        source: SourceSelector,
        since: Option<&str>,
    ) -> Result<(Vec<RecentSession>, Vec<StoreDiagnostic>), String> {
        let stores = crate::sources::discover_stores(source);
        let path = crate::index::default_index_path();
        let refreshed = crate::index::refresh_transcript_index(
            &stores,
            &path,
            false,
            crate::DEFAULT_MAX_PARALLEL,
        )?;
        let sessions = crate::index::recent_sessions(filter, since, &stores, &path)?;
        Ok((sessions, refreshed.skipped))
    }

    fn find(
        &self,
        terms: &[String],
        options: &FindOptions,
    ) -> Result<crate::find::FindResult, String> {
        find_sessions(terms, options, &Disk)
    }

    fn path_for_id(&self, id: &str) -> Result<Option<String>, String> {
        let stores = crate::sources::discover_stores(SourceSelector::All);
        let path = crate::index::default_index_path();
        crate::index::refresh_transcript_index(&stores, &path, false, crate::DEFAULT_MAX_PARALLEL)?;
        crate::index::path_for_session_id(id, &stores, &path)
    }

    fn source_of(&self, locator: &str) -> Result<TranscriptSource, String> {
        crate::sources::source_from_locator(locator, crate::sources::default_roots())
    }

    fn view(&self, locator: &str, tools: bool) -> Result<TranscriptView, String> {
        view_transcript(
            locator,
            TranscriptViewOptions {
                thinking: false,
                tools,
            },
        )
    }

    fn project(&self, path: &str, source: TranscriptSource) -> String {
        read_transcript_project(path, source)
    }

    fn opening_prompt(&self, path: &str, source: TranscriptSource) -> String {
        find_opening_prompt(path, source, &Disk)
    }
}

/// `dejavu last`.
pub fn last_session(options: &LastOptions, deps: &dyn LastDeps) -> Result<LastResult, String> {
    let exclusions: Vec<String> = options
        .exclude_sessions
        .iter()
        .cloned()
        .chain(active_session_ids())
        .collect();
    last_session_excluding(options, &exclusions, deps)
}

/// [`last_session`] with the exclusions already gathered.
pub fn last_session_excluding(
    options: &LastOptions,
    exclusions: &[String],
    deps: &dyn LastDeps,
) -> Result<LastResult, String> {
    let (candidates, skipped_stores) = select(options, deps)?;
    // A session named outright is read even when it is the active one.
    let exclusions = match options.selector {
        Selector::Locator(_) => &[],
        _ => exclusions,
    };
    let before = candidates.len();
    let candidates: Vec<Candidate> = candidates
        .into_iter()
        .filter(|candidate| {
            !exclusions
                .iter()
                .any(|value| excluded(&candidate.path, candidate.source, value))
        })
        .collect();
    let excluded_count = before - candidates.len();
    let total = candidates.len();
    let wanted = if options.list { options.limit } else { 1 };
    let mut sessions: Vec<SessionCard> = candidates
        .into_iter()
        .take(wanted)
        .map(|candidate| SessionCard {
            project: deps.project(&candidate.path, candidate.source),
            opening_prompt: opening_preview(
                &deps.opening_prompt(&candidate.path, candidate.source),
            ),
            last_request: None,
            resume: resume_command(candidate.source, &candidate.path),
            path: candidate.path,
            source: candidate.source,
            date: candidate.date,
        })
        .collect();
    let mut tail = None;
    let mut tail_start = None;
    if !options.list
        && let Some(card) = sessions.first_mut()
    {
        let mut view = deps.view(&card.path, options.tools)?;
        // Harness-injected user rows (task notifications, reminders, caveats) are noise here.
        view.events.retain(|event| match &event.body {
            EventBody::User { text } => is_real_user_prompt(text),
            _ => true,
        });
        card.last_request = view
            .events
            .iter()
            .rev()
            .find_map(|event| match &event.body {
                EventBody::User { text } => Some(text.trim().to_string()),
                _ => None,
            });
        card.project = view.project.clone();
        if let Some(latest) = view.events.iter().rev().find_map(|e| e.timestamp.clone()) {
            card.date = latest;
        }
        let start = tail_start_index(&view, options);
        let windowed = window_transcript(
            view,
            &WindowOptions {
                from_event: Some(start),
                max_chars: Some(options.max_chars.min(options.budget_chars)),
                tool_chars: Some(options.tool_chars.min(options.budget_chars)),
                ..WindowOptions::default()
            },
        )?;
        tail_start = Some(start);
        tail = Some(windowed);
    }
    Ok(LastResult {
        sessions,
        total,
        excluded_count,
        tail,
        tail_start,
        skipped_stores,
    })
}

fn select(
    options: &LastOptions,
    deps: &dyn LastDeps,
) -> Result<(Vec<Candidate>, Vec<StoreDiagnostic>), String> {
    let since = options
        .since
        .as_deref()
        .map(|value| crate::find::parse_since(value, None))
        .transpose()?;
    match &options.selector {
        Selector::Recent(filter) => {
            let (sessions, skipped) = deps.recent(filter, options.source, since.as_deref())?;
            let candidates = sessions
                .into_iter()
                .map(|session| Candidate {
                    path: session.path,
                    source: session.source,
                    date: session.date,
                })
                .collect();
            Ok((candidates, skipped))
        }
        Selector::Terms {
            terms,
            project,
            widen,
        } => {
            let search = |project: Option<String>| {
                deps.find(
                    terms,
                    &FindOptions {
                        source: options.source,
                        limit: SEARCH_CAP,
                        project,
                        since: options.since.clone(),
                        ..FindOptions::default()
                    },
                )
            };
            let mut found = search(project.clone())?;
            if found.hits.is_empty() && *widen && project.is_some() {
                found = search(None)?;
            }
            let candidates = found
                .hits
                .into_iter()
                .map(|hit| Candidate {
                    path: hit.path,
                    source: hit.source,
                    date: hit.date,
                })
                .collect();
            Ok((candidates, found.skipped_stores))
        }
        Selector::Locator(value) => {
            let path = if is_uuid(value) && !std::path::Path::new(value).exists() {
                deps.path_for_id(value)?
                    .ok_or_else(|| format!("no indexed session has id {value}"))?
            } else {
                value.clone()
            };
            let source = deps.source_of(&path)?;
            Ok((
                vec![Candidate {
                    path,
                    source,
                    date: String::new(),
                }],
                Vec::new(),
            ))
        }
    }
}

/// The first event of the newest run that fits `turns` and the budget. Each
/// event costs its body clipped to its per-event cap; the newest event always fits.
fn tail_start_index(view: &TranscriptView, options: &LastOptions) -> usize {
    let mut used = 0usize;
    let mut start = None;
    for (kept, (position, event)) in view.events.iter().enumerate().rev().enumerate() {
        let cap = match event.body {
            EventBody::ToolCall { .. } | EventBody::ToolResult { .. } => options.tool_chars,
            _ => options.max_chars,
        }
        .min(options.budget_chars);
        let cost = crate::js::len(&crate::window::event_body(event)).min(cap);
        if kept > 0 && (kept >= options.turns || used + cost > options.budget_chars) {
            break;
        }
        used += cost;
        start = Some(event.index.unwrap_or(position));
    }
    start.unwrap_or(0)
}

/// The text report: cards, then the first session's tail.
pub fn render_last(result: &LastResult, color: bool) -> String {
    if result.sessions.is_empty() {
        return "No matching sessions.".to_string();
    }
    let cards: Vec<String> = result
        .sessions
        .iter()
        .map(|card| render_card(card, color))
        .collect();
    let mut out = cards.join("\n\n---\n\n");
    if result.tail.is_none() && result.total > result.sessions.len() {
        out.push_str(&format!(
            "\n\n{} of {} matching sessions.",
            result.sessions.len(),
            result.total
        ));
    }
    if let (Some(tail), Some(start)) = (&result.tail, result.tail_start) {
        let card = &result.sessions[0];
        out.push_str(&format!(
            "\n\nWhere it left off:\n\n{}\n\n{}",
            render_transcript(&tail.view, RenderTranscriptOptions { full: true, color }),
            render_window(&tail.window)
        ));
        if start > 0 {
            out.push_str(&format!(
                "\nEarlier turns: dejavu transcript '{}' --no-tools --max-chars 1500 --budget-chars 8000 --from-event {}",
                card.path,
                start.saturating_sub(20)
            ));
        }
    }
    out
}

fn render_card(card: &SessionCard, color: bool) -> String {
    let paint = crate::render::Paint { color };
    let mut lines = vec![format!(
        "{} · {} · {}",
        card.date, card.source, card.project
    )];
    if !card.opening_prompt.is_empty() {
        lines.push(format!(
            "Opened with: {}",
            paint.markdown(crate::js::prefix(&card.opening_prompt, 300))
        ));
    }
    if let Some(request) = &card.last_request {
        lines.push(format!(
            "Last request: {}",
            paint.markdown(crate::js::prefix(request, 500))
        ));
    }
    lines.push(format!("Transcript: {}", card.path));
    if let Some(resume) = &card.resume {
        lines.push(format!("Resume: {resume}"));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::find::FindResult;
    use crate::view::{TranscriptEvent, count_events};

    fn event(body: EventBody, index: usize, timestamp: &str) -> TranscriptEvent {
        let mut event = TranscriptEvent::new(body);
        event.index = Some(index);
        event.timestamp = Some(timestamp.into());
        event
    }

    fn user(text: &str, index: usize) -> TranscriptEvent {
        event(
            EventBody::User { text: text.into() },
            index,
            "2026-10-01T10:00:00Z",
        )
    }

    fn assistant(text: &str, index: usize) -> TranscriptEvent {
        event(
            EventBody::Assistant { text: text.into() },
            index,
            "2026-10-01T11:00:00Z",
        )
    }

    struct Fake {
        recent: Vec<RecentSession>,
        events: Vec<TranscriptEvent>,
    }

    impl LastDeps for Fake {
        fn recent(
            &self,
            filter: &ProjectFilter,
            _: SourceSelector,
            _: Option<&str>,
        ) -> Result<(Vec<RecentSession>, Vec<StoreDiagnostic>), String> {
            assert_eq!(filter, &ProjectFilter::Under("/work/app".into()));
            Ok((self.recent.clone(), Vec::new()))
        }
        fn find(&self, terms: &[String], options: &FindOptions) -> Result<FindResult, String> {
            // Only the unfiltered search finds anything.
            let hits = match options.project {
                Some(_) => Vec::new(),
                None => vec![crate::find::FindHit {
                    source: TranscriptSource::Claude,
                    path: "/c/elsewhere.jsonl".into(),
                    project: "other".into(),
                    date: "2026-09-01".into(),
                    score: 1,
                    term_counts: crate::find::JsObject(Vec::new()),
                    opening_prompt: String::new(),
                    matches: Vec::new(),
                    resume: None,
                }],
            };
            Ok(FindResult {
                terms: terms.to_vec(),
                required_terms: terms.to_vec(),
                sources: Vec::new(),
                hits,
                truncated: false,
                skipped_stores: Vec::new(),
                elapsed_ms: 0,
                store_timings: crate::find::JsObject(Vec::new()),
            })
        }
        fn path_for_id(&self, id: &str) -> Result<Option<String>, String> {
            Ok(Some(format!("/c/{id}.jsonl")))
        }
        fn source_of(&self, _: &str) -> Result<TranscriptSource, String> {
            Ok(TranscriptSource::Claude)
        }
        fn view(&self, path: &str, _: bool) -> Result<TranscriptView, String> {
            Ok(TranscriptView {
                path: path.into(),
                source: TranscriptSource::Claude,
                project: "work/app".into(),
                counts: count_events(&self.events),
                events: self.events.clone(),
            })
        }
        fn project(&self, _: &str, _: TranscriptSource) -> String {
            "work/app".into()
        }
        fn opening_prompt(&self, path: &str, _: TranscriptSource) -> String {
            if path == "/c/long.jsonl" {
                return "😀 request ".repeat(100);
            }
            "fix the build".into()
        }
    }

    fn recent(path: &str, date: &str) -> RecentSession {
        RecentSession {
            path: path.into(),
            source: TranscriptSource::Claude,
            date: date.into(),
        }
    }

    const ACTIVE: &str = "11111111-2222-3333-4444-555555555555";

    #[test]
    fn skips_the_active_session_and_tails_the_newest_other_one() {
        let fake = Fake {
            recent: vec![
                recent(&format!("/c/{ACTIVE}.jsonl"), "2026-10-02"),
                recent(
                    "/c/aaaaaaaa-2222-3333-4444-555555555555.jsonl",
                    "2026-10-01",
                ),
                recent(
                    "/c/bbbbbbbb-2222-3333-4444-555555555555.jsonl",
                    "2026-09-30",
                ),
            ],
            events: (0..30)
                .map(|i| {
                    if i % 2 == 0 {
                        user(&format!("ask {i}"), i)
                    } else {
                        assistant(&format!("answer {i}"), i)
                    }
                })
                .collect(),
        };
        let options = LastOptions {
            selector: Selector::Recent(ProjectFilter::Under("/work/app".into())),
            turns: 4,
            ..LastOptions::default()
        };
        let result = last_session_excluding(&options, &[ACTIVE.into()], &fake).unwrap();
        assert_eq!(result.excluded_count, 1);
        assert_eq!(result.total, 2);
        assert_eq!(result.sessions.len(), 1);
        let card = &result.sessions[0];
        assert!(card.path.contains("aaaaaaaa"));
        assert_eq!(card.date, "2026-10-01T11:00:00Z");
        assert_eq!(
            card.resume.as_deref(),
            Some("claude --resume aaaaaaaa-2222-3333-4444-555555555555")
        );
        let tail = result.tail.unwrap();
        let indexes: Vec<usize> = tail.view.events.iter().filter_map(|e| e.index).collect();
        assert_eq!(indexes, vec![26, 27, 28, 29]);
        assert_eq!(result.tail_start, Some(26));
        assert_eq!(tail.window.next_event, None);
    }

    #[test]
    fn injected_user_rows_leave_the_tail_and_the_last_request_is_the_real_prompt() {
        let fake = Fake {
            recent: vec![recent("/c/a.jsonl", "2026-10-01")],
            events: vec![
                user("ship the release", 0),
                assistant("shipping", 1),
                user("<task-notification>done</task-notification>", 2),
                assistant("released", 3),
            ],
        };
        let options = LastOptions {
            selector: Selector::Recent(ProjectFilter::Under("/work/app".into())),
            ..LastOptions::default()
        };
        let result = last_session_excluding(&options, &[], &fake).unwrap();
        assert_eq!(
            result.sessions[0].last_request.as_deref(),
            Some("ship the release")
        );
        let indexes: Vec<usize> = result
            .tail
            .unwrap()
            .view
            .events
            .iter()
            .filter_map(|e| e.index)
            .collect();
        assert_eq!(indexes, vec![0, 1, 3]);
    }

    #[test]
    fn the_budget_bounds_the_tail_but_keeps_the_newest_event() {
        let long = "x".repeat(5000);
        let fake = Fake {
            recent: vec![recent("/c/a.jsonl", "2026-10-01")],
            events: vec![user("short", 0), assistant(&long, 1), user(&long, 2)],
        };
        let options = LastOptions {
            selector: Selector::Recent(ProjectFilter::Under("/work/app".into())),
            budget_chars: 1000,
            max_chars: 1500,
            ..LastOptions::default()
        };
        let result = last_session_excluding(&options, &[], &fake).unwrap();
        let tail = result.tail.unwrap();
        assert_eq!(tail.view.events.len(), 1);
        assert_eq!(tail.view.events[0].index, Some(2));
        assert!(tail.window.used_chars <= 1000);
    }

    #[test]
    fn serialized_cards_bound_long_opening_prompts() {
        let fake = Fake {
            recent: vec![recent("/c/long.jsonl", "2026-10-01")],
            events: Vec::new(),
        };
        let options = LastOptions {
            selector: Selector::Recent(ProjectFilter::Under("/work/app".into())),
            list: true,
            ..LastOptions::default()
        };
        let result = last_session_excluding(&options, &[], &fake).unwrap();
        let value = serde_json::to_value(result).unwrap();
        let preview = value["sessions"][0]["openingPrompt"].as_str().unwrap();
        assert!(crate::js::len(preview) <= 300);
        assert!(preview.ends_with('…'));
    }

    #[test]
    fn list_returns_cards_without_a_tail() {
        let fake = Fake {
            recent: (0..8)
                .map(|i| recent(&format!("/c/{i}.jsonl"), "2026-10-01"))
                .collect(),
            events: Vec::new(),
        };
        let options = LastOptions {
            selector: Selector::Recent(ProjectFilter::Under("/work/app".into())),
            list: true,
            limit: 3,
            ..LastOptions::default()
        };
        let result = last_session_excluding(&options, &[], &fake).unwrap();
        assert_eq!(result.sessions.len(), 3);
        assert_eq!(result.total, 8);
        assert!(result.tail.is_none());
        assert!(render_last(&result, false).ends_with("3 of 8 matching sessions."));
    }

    #[test]
    fn terms_widen_to_every_project_only_when_allowed() {
        let fake = Fake {
            recent: Vec::new(),
            events: vec![user("hi", 0)],
        };
        let terms = |widen| LastOptions {
            selector: Selector::Terms {
                terms: vec!["auth".into()],
                project: Some("work/app".into()),
                widen,
            },
            ..LastOptions::default()
        };
        let widened = last_session_excluding(&terms(true), &[], &fake).unwrap();
        assert_eq!(widened.sessions[0].path, "/c/elsewhere.jsonl");
        let pinned = last_session_excluding(&terms(false), &[], &fake).unwrap();
        assert!(pinned.sessions.is_empty());
    }

    #[test]
    fn a_session_id_resolves_through_the_index() {
        let fake = Fake {
            recent: Vec::new(),
            events: vec![user("hi", 0)],
        };
        let id = "cccccccc-2222-3333-4444-555555555555";
        let options = LastOptions {
            selector: Selector::Locator(id.into()),
            ..LastOptions::default()
        };
        let result = last_session_excluding(&options, &[], &fake).unwrap();
        assert_eq!(result.sessions[0].path, format!("/c/{id}.jsonl"));
    }
}
