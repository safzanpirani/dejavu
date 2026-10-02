//! Bounded transcript pages (`transcript-window.ts`): a start event, an event
//! limit, and per-event and total character budgets. Character counts are
//! UTF-16 units of event bodies, including ellipses; labels and metadata are
//! excluded.

use crate::js;
use crate::paths::js_lower;
use crate::view::{EventBody, TranscriptEvent, TranscriptView};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WindowOptions {
    pub from_event: Option<usize>,
    pub limit: Option<usize>,
    pub max_chars: Option<usize>,
    pub tool_chars: Option<usize>,
    pub budget_chars: Option<usize>,
    /// Prefer a matching region when clipping a search excerpt.
    pub focus_terms: Vec<String>,
}

/// One event whose body was cut to fit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClippedEvent {
    pub index: usize,
    /// `text`, `input`, or `output`.
    pub field: &'static str,
    #[serde(rename = "originalChars")]
    pub original_chars: usize,
    #[serde(rename = "returnedChars")]
    pub returned_chars: usize,
    #[serde(rename = "startChar")]
    pub start_char: usize,
}

/// What a page holds and where the next page starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TranscriptWindow {
    #[serde(rename = "availableEvents")]
    pub available_events: usize,
    #[serde(rename = "returnedEvents")]
    pub returned_events: usize,
    #[serde(rename = "usedChars")]
    pub used_chars: usize,
    #[serde(rename = "nextEvent")]
    pub next_event: Option<usize>,
    pub clipped: Vec<ClippedEvent>,
}

/// A view cut to a window. Serializes as the view's fields followed by `window`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WindowedTranscript {
    #[serde(flatten)]
    pub view: TranscriptView,
    pub window: TranscriptWindow,
}

/// `eventBody(event)`: the text a budget counts. Tool inputs that are not
/// strings count as compact JSON.
pub fn event_body(event: &TranscriptEvent) -> std::borrow::Cow<'_, str> {
    match &event.body {
        EventBody::ToolCall { input, .. } => match input {
            Value::String(text) => text.as_str().into(),
            other => js::stringify(other).into(),
        },
        EventBody::ToolResult { output, .. } => output.as_str().into(),
        EventBody::User { text } | EventBody::Assistant { text } | EventBody::Thinking { text } => {
            text.as_str().into()
        }
    }
}

/// `validateBound(value, name, minimum)`: `<name> needs an integer >= <minimum>`.
pub fn validate_bound(value: Option<usize>, name: &str, minimum: usize) -> Result<(), String> {
    match value {
        Some(value) if value < minimum => Err(format!("{name} needs an integer >= {minimum}")),
        _ => Ok(()),
    }
}

/// `windowTranscript(view, options)`.
pub fn window_transcript(
    view: TranscriptView,
    options: &WindowOptions,
) -> Result<WindowedTranscript, String> {
    validate_bound(options.from_event, "--from-event", 0)?;
    validate_bound(options.limit, "limit", 1)?;
    validate_bound(options.max_chars, "maxChars", 1)?;
    validate_bound(options.tool_chars, "toolChars", 1)?;
    validate_bound(options.budget_chars, "budgetChars", 1)?;
    let TranscriptView {
        path,
        source,
        project,
        counts,
        events: all,
    } = view;
    let from = options.from_event.unwrap_or(0);
    let available: Vec<TranscriptEvent> = all
        .into_iter()
        .enumerate()
        .map(|(position, mut event)| {
            event.index = Some(event.index.unwrap_or(position));
            event
        })
        .filter(|event| event.index.unwrap_or(0) >= from)
        .collect();
    let available_events = available.len();
    let budget = options.budget_chars.unwrap_or(usize::MAX);
    let limit = options.limit.unwrap_or(usize::MAX);
    let focus: Vec<String> = options
        .focus_terms
        .iter()
        .map(|term| js_lower(term).into_owned())
        .collect();
    let mut events = Vec::new();
    let mut clipped = Vec::new();
    let mut used_chars = 0usize;
    let mut next_event = None;
    for mut event in available {
        let index = event.index.unwrap_or(0);
        let remaining = budget.saturating_sub(used_chars);
        if events.len() >= limit || remaining == 0 {
            next_event = Some(index);
            break;
        }
        let cap = if event.is_tool() {
            options.tool_chars
        } else {
            options.max_chars
        }
        .unwrap_or(usize::MAX)
        .min(remaining);
        let body = event_body(&event);
        let body_chars = js::len(&body);
        if body_chars <= cap {
            used_chars += body_chars;
            events.push(event);
            continue;
        }
        let mut start = 0usize;
        if !focus.is_empty() {
            let lower = js_lower(&body);
            let first = focus
                .iter()
                .filter_map(|term| lower.find(term.as_str()))
                .map(|byte| js::len(&lower[..byte]))
                .min();
            if let Some(first) = first {
                let latest = body_chars as i64 - cap as i64 + 2;
                let wanted = first as i64 - (cap / 4) as i64;
                start = latest.min(wanted).max(0) as usize;
            }
        }
        let lead = start > 0 && cap > 1;
        let keep = cap.saturating_sub(if lead { 2 } else { 1 });
        let text = format!(
            "{}{}…",
            if lead { "…" } else { "" },
            js::slice(&body, start, start.saturating_add(keep))
        );
        let text_chars = js::len(&text);
        used_chars += text_chars;
        let field = match &mut event.body {
            EventBody::ToolCall { input, .. } => {
                *input = Value::String(text);
                "input"
            }
            EventBody::ToolResult { output, .. } => {
                *output = text;
                "output"
            }
            EventBody::User { text: slot }
            | EventBody::Assistant { text: slot }
            | EventBody::Thinking { text: slot } => {
                *slot = text;
                "text"
            }
        };
        clipped.push(ClippedEvent {
            index,
            field,
            original_chars: body_chars,
            returned_chars: text_chars,
            start_char: start,
        });
        events.push(event);
    }
    let returned_events = events.len();
    Ok(WindowedTranscript {
        view: TranscriptView {
            path,
            source,
            project,
            counts,
            events,
        },
        window: TranscriptWindow {
            available_events,
            returned_events,
            used_chars,
            next_event,
            clipped,
        },
    })
}

/// `renderWindow(window)`: the one-line page summary printed under a bounded transcript.
pub fn render_window(window: &TranscriptWindow) -> String {
    let next = match window.next_event {
        None => "end".to_string(),
        Some(next) => format!("continue with --from-event {next}"),
    };
    let clipped = if window.clipped.is_empty() {
        String::new()
    } else {
        let ids: Vec<String> = window
            .clipped
            .iter()
            .map(|event| format!("#{}", event.index))
            .collect();
        format!(
            "; clipped events {} (read each with --full --from-event N --limit 1)",
            ids.join(", ")
        )
    };
    format!(
        "[{}/{} events; {} body chars; {next}{clipped}]",
        window.returned_events, window.available_events, window.used_chars
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TranscriptSource;
    use crate::view::{EventRef, count_events};
    use serde_json::json;

    fn view(events: Vec<TranscriptEvent>) -> TranscriptView {
        TranscriptView {
            source: TranscriptSource::Claude,
            path: "/fixture".into(),
            project: "/project".into(),
            counts: count_events(&events),
            events,
        }
    }

    fn event(body: EventBody, index: usize) -> TranscriptEvent {
        let mut event = TranscriptEvent::new(body);
        event.index = Some(index);
        event
    }

    fn user(text: &str, index: usize) -> TranscriptEvent {
        event(EventBody::User { text: text.into() }, index)
    }

    fn assistant(text: &str, index: usize) -> TranscriptEvent {
        event(EventBody::Assistant { text: text.into() }, index)
    }

    #[test]
    fn pagination_follows_stable_ids_and_can_recover_the_clipped_event() {
        let mut long = assistant("a long answer", 4);
        long.reference = Some(EventRef::Line {
            line: 3,
            block: None,
        });
        let original = view(vec![user("hello", 0), long.clone(), user("next", 8)]);
        let options = |from, limit, max| WindowOptions {
            from_event: Some(from),
            limit,
            max_chars: max,
            ..WindowOptions::default()
        };
        let page = window_transcript(original.clone(), &options(1, Some(1), Some(4))).unwrap();
        assert_eq!(
            serde_json::to_value(&page.view.events).unwrap(),
            json!([{ "kind": "assistant", "text": "a l…", "ref": { "line": 3 }, "index": 4 }])
        );
        assert_eq!(page.window.next_event, Some(8));
        assert_eq!(
            page.window.clipped[0],
            ClippedEvent {
                index: 4,
                field: "text",
                original_chars: 13,
                returned_chars: 4,
                start_char: 0
            }
        );
        let exact = window_transcript(original.clone(), &options(4, Some(1), None)).unwrap();
        assert_eq!(exact.view.events[0], long);
        let tail = window_transcript(original.clone(), &options(8, None, None)).unwrap();
        assert_eq!(tail.window.next_event, None);
        let past = window_transcript(original, &options(99, None, None)).unwrap();
        assert!(past.view.events.is_empty());
    }

    #[test]
    fn tool_caps_affect_only_tool_bodies_and_mark_input_previews() {
        let input = json!({ "command": "run a long command" });
        let original = view(vec![
            user("unlimited dialogue", 0),
            event(
                EventBody::ToolCall {
                    name: "shell".into(),
                    input: input.clone(),
                    call_id: None,
                },
                1,
            ),
            event(
                EventBody::ToolResult {
                    name: None,
                    call_id: None,
                    output: "long output".into(),
                    is_error: false,
                },
                2,
            ),
        ]);
        let page = window_transcript(
            original.clone(),
            &WindowOptions {
                tool_chars: Some(6),
                ..WindowOptions::default()
            },
        )
        .unwrap();
        let events = &page.view.events;
        assert_eq!(event_body(&events[0]), "unlimited dialogue");
        assert_eq!(js::len(&event_body(&events[1])), 6);
        assert_eq!(js::len(&event_body(&events[2])), 6);
        let fields: Vec<_> = page.window.clipped.iter().map(|c| c.field).collect();
        assert_eq!(fields, ["input", "output"]);
        assert!(
            matches!(&original.events[1].body, EventBody::ToolCall { input: i, .. } if *i == input)
        );
        let unbounded = window_transcript(original.clone(), &WindowOptions::default()).unwrap();
        assert_eq!(unbounded.view.events, original.events);
    }

    #[test]
    fn tiny_budgets_include_markers_in_the_cap_and_always_advance() {
        let original = view(vec![user("large", 3), assistant("other", 7)]);
        let budget = |from| WindowOptions {
            from_event: from,
            budget_chars: Some(1),
            ..WindowOptions::default()
        };
        let first = window_transcript(original.clone(), &budget(None)).unwrap();
        assert_eq!(first.window.used_chars, 1);
        assert_eq!(first.window.next_event, Some(7));
        assert_eq!(event_body(&first.view.events[0]), "…");
        let last = window_transcript(original, &budget(Some(7))).unwrap();
        assert_eq!(last.window.next_event, None);
    }

    #[test]
    fn invalid_bounds_fail_and_zero_is_valid_only_for_offsets() {
        let original = view(Vec::new());
        let zero_budget = WindowOptions {
            budget_chars: Some(0),
            ..WindowOptions::default()
        };
        assert!(window_transcript(original.clone(), &zero_budget).is_err());
        let zero_offset = WindowOptions {
            from_event: Some(0),
            ..WindowOptions::default()
        };
        assert!(
            window_transcript(original, &zero_offset)
                .unwrap()
                .view
                .events
                .is_empty()
        );
    }

    #[test]
    fn focus_terms_center_the_excerpt_and_summary_renders() {
        let body = format!("{}needle{}", "x".repeat(50), "y".repeat(50));
        let original = view(vec![user(&body, 0)]);
        let page = window_transcript(
            original,
            &WindowOptions {
                max_chars: Some(20),
                focus_terms: vec!["NEEDLE".into()],
                ..WindowOptions::default()
            },
        )
        .unwrap();
        assert_eq!(page.window.clipped[0].start_char, 45);
        assert_eq!(event_body(&page.view.events[0]), "…xxxxxneedleyyyyyyy…");
        assert_eq!(
            render_window(&page.window),
            "[1/1 events; 20 body chars; end; clipped events #0 (read each with --full --from-event N --limit 1)]"
        );
        let json = js::stringify(&page);
        assert!(
            json.starts_with(
                r#"{"path":"/fixture","source":"claude","project":"/project","counts":"#
            )
        );
        assert!(json.contains(r#""window":{"availableEvents":1,"returnedEvents":1,"usedChars":20,"nextEvent":null,"clipped":[{"index":0,"field":"text","originalChars":106,"returnedChars":20,"startChar":45}]}"#));
    }
}
