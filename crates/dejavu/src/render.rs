//! Text renderers (`render.ts`). Search, find, and query results are owned by
//! other modules; their renderers read the result's JSON form
//! (`serde_json::to_value(&result)`), so they depend only on the JSON contract.

use crate::js;
use crate::view::{EventBody, ShowResult, TranscriptEvent, TranscriptView, js_trim};
use serde_json::Value;

// ---------------------------------------------------------------------------
// search, find, query, show
// ---------------------------------------------------------------------------

fn field<'a>(value: &'a Value, key: &str) -> &'a Value {
    value.get(key).unwrap_or(&Value::Null)
}

/// A value as JavaScript template interpolation prints it.
fn display(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number
            .as_i64()
            .map(|n| n.to_string())
            .unwrap_or_else(|| js::number_to_string(number.as_f64().unwrap_or(f64::NAN))),
        Value::Null => "null".into(),
        Value::Bool(flag) => flag.to_string(),
        Value::Array(items) => items.iter().map(display).collect::<Vec<_>>().join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}

fn items(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or_default()
}

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

fn truthy_text(value: &Value) -> Option<String> {
    match value {
        Value::Null | Value::Bool(false) => None,
        Value::String(text) if text.is_empty() => None,
        other => Some(display(other)),
    }
}

/// `renderSearch(result)` over a `SearchResult` in JSON form
/// (`query`, `matches[]` with `date`, `source`, `project`, `count`, `path`, `snippets[]`).
pub fn render_search(result: &Value, color: bool) -> String {
    let paint = Paint { color };
    let query = display(field(result, "query"));
    let matches = items(field(result, "matches"));
    if matches.is_empty() {
        return format!(
            "No sessions found matching \"{query}\".\n\nSearch is literal, not semantic. Retry with one exact distinctive token or phrase."
        );
    }
    let terms = [query.clone()];
    let sections: Vec<String> = matches
        .iter()
        .map(|item| {
            let snippets: Vec<String> = items(field(item, "snippets"))
                .iter()
                .map(|snippet| {
                    paint.snippet(
                        &display(field(snippet, "role")),
                        "",
                        &display(field(snippet, "text")),
                        &terms,
                    )
                })
                .collect();
            let count = field(item, "count");
            let suffix = if count.as_f64() == Some(1.0) {
                ""
            } else {
                "es"
            };
            let tally = format!("{} match{suffix}", display(count));
            format!(
                "{}\n{}\n{}",
                paint.heading(
                    &display(field(item, "date")),
                    &display(field(item, "source")),
                    &display(field(item, "project")),
                    &paint.dim(&tally),
                ),
                paint.labeled("Transcript", &paint.dim(&display(field(item, "path")))),
                snippets.join("\n")
            )
        })
        .collect();
    format!(
        "{} matching \"{}\":\n\n{}",
        paint.bold(&format!(
            "Found {}",
            plural(matches.len(), "session", "sessions")
        )),
        paint.term(&query),
        sections.join(&paint.separator())
    )
}

/// `renderQuery(result)`: the answer.
pub fn render_query(result: &Value) -> String {
    display(field(result, "answer"))
}

fn join_terms(value: &Value, separator: &str) -> String {
    items(value)
        .iter()
        .map(display)
        .collect::<Vec<_>>()
        .join(separator)
}

/// `renderFind(result)` over a `FindResult` in JSON form (`terms`,
/// `requiredTerms`, `hits[]` with `date`, `source`, `project`, `termCounts`,
/// `openingPrompt`, `matches[]`, `path`, `resume`).
pub fn render_find(result: &Value, color: bool) -> String {
    let paint = Paint { color };
    let terms = items(field(result, "terms"));
    let hits = items(field(result, "hits"));
    if hits.is_empty() {
        return format!(
            "No sessions found for: {}.\n\nTerms are literal (AND). Try fewer or different exact terms, or drop filters.",
            join_terms(field(result, "terms"), ", ")
        );
    }
    let words: Vec<String> = terms.iter().map(display).collect();
    let required = items(field(result, "requiredTerms")).len();
    let relaxed = if required < terms.len() {
        paint.warn(&format!(
            "No session matched all {} terms; showing sessions matching {required}.",
            terms.len()
        )) + "\n\n"
    } else {
        String::new()
    };
    let sections: Vec<String> = hits
        .iter()
        .map(|hit| {
            let counts: Vec<String> = field(hit, "termCounts")
                .as_object()
                .map(|counts| {
                    counts
                        .iter()
                        .map(|(term, count)| {
                            let user = field(count, "user").as_f64().unwrap_or(0.0);
                            let assistant = field(count, "assistant").as_f64().unwrap_or(0.0);
                            format!(
                                "{}{}{}",
                                paint.term(term),
                                paint.dim("×"),
                                paint.dim(&format!(
                                    "{}(u{})",
                                    js::number_to_string(user + assistant),
                                    js::number_to_string(user)
                                ))
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            let mut lines = vec![paint.heading(
                &display(field(hit, "date")),
                &display(field(hit, "source")),
                &display(field(hit, "project")),
                &counts.join(" "),
            )];
            if let Some(prompt) = truthy_text(field(hit, "openingPrompt")) {
                let prompt = if color {
                    crate::markdown::render(&highlight(&prompt, &words, paint))
                } else {
                    prompt
                };
                lines.push(paint.labeled("Opened with", &paint.hanging(&prompt, 13)));
            }
            for item in items(field(hit, "matches")).iter().take(3) {
                let date = truthy_text(field(item, "date"))
                    .map(|date| format!(" {date}"))
                    .unwrap_or_default();
                lines.push(paint.snippet(
                    &display(field(item, "role")),
                    &date,
                    &display(field(item, "text")),
                    &words,
                ));
            }
            lines.push(paint.labeled("Transcript", &paint.dim(&display(field(hit, "path")))));
            if let Some(resume) = truthy_text(field(hit, "resume")) {
                lines.push(paint.labeled("Resume", &paint.command(&resume)));
            }
            lines.join("\n")
        })
        .collect();
    let joined = if color {
        words
            .iter()
            .map(|word| paint.term(word))
            .collect::<Vec<_>>()
            .join(&paint.dim(" + "))
    } else {
        join_terms(field(result, "terms"), " + ")
    };
    format!(
        "{relaxed}{} for {joined}:\n\n{}",
        paint.bold(&format!(
            "Found {}",
            plural(hits.len(), "session", "sessions")
        )),
        sections.join(&paint.separator())
    )
}

/// `renderShow(result)`.
pub fn render_show(result: &ShowResult) -> String {
    let blocks: Vec<String> = result
        .messages
        .iter()
        .map(|message| format!("[{}]\n{}", message.role, message.text))
        .collect();
    blocks.join("\n\n")
}

// ---------------------------------------------------------------------------
// transcript
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RenderTranscriptOptions {
    /// Do not truncate messages, tool inputs, or tool outputs.
    pub full: bool,
    /// Emit ANSI colors.
    pub color: bool,
}

const TEXT_LIMIT: usize = 1200;
const TOOL_INPUT_LIMIT: usize = 300;
const TOOL_OUTPUT_LINES: usize = 8;
const TOOL_OUTPUT_LIMIT: usize = 600;

#[derive(Clone, Copy)]
pub(crate) struct Paint {
    pub(crate) color: bool,
}

impl Paint {
    fn wrap(self, code: &str, text: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }
    fn user(self, text: &str) -> String {
        self.wrap("1;36", text)
    }
    fn assistant(self, text: &str) -> String {
        self.wrap("1;32", text)
    }
    fn tool(self, text: &str) -> String {
        self.wrap("33", text)
    }
    fn error(self, text: &str) -> String {
        self.wrap("31", text)
    }
    fn dim(self, text: &str) -> String {
        self.wrap("90", text)
    }
    /// Markdown styling in color; the text unchanged otherwise.
    pub(crate) fn markdown(self, text: &str) -> String {
        if self.color {
            crate::markdown::render(text)
        } else {
            text.to_string()
        }
    }
    fn bold(self, text: &str) -> String {
        self.wrap("1", text)
    }
    fn warn(self, text: &str) -> String {
        self.wrap("33", text)
    }
    /// A search term: bold yellow.
    fn term(self, text: &str) -> String {
        self.wrap("1;33", text)
    }
    /// A command worth copying: green.
    fn command(self, text: &str) -> String {
        self.wrap("32", text)
    }
    /// Each agent gets its own color.
    fn source(self, name: &str) -> String {
        let code = match name {
            "claude" => "1;38;5;209",
            "codex" => "1;38;5;114",
            "pi" => "1;38;5;141",
            "opencode" => "1;38;5;221",
            "droid" => "1;38;5;75",
            _ => "1",
        };
        self.wrap(code, name)
    }
    fn role(self, role: &str) -> String {
        match role {
            "user" => self.wrap("36", role),
            "assistant" => self.wrap("32", role),
            _ => self.dim(role),
        }
    }
    /// `date · source · project · tail`, the first line of a result.
    fn heading(self, date: &str, source: &str, project: &str, tail: &str) -> String {
        let dot = self.dim(" · ");
        format!(
            "{}{dot}{}{dot}{}{dot}{tail}",
            self.wrap("34", date),
            self.source(source),
            self.bold(project)
        )
    }
    fn labeled(self, label: &str, value: &str) -> String {
        format!("{} {value}", self.dim(&format!("{label}:")))
    }
    /// The plain `---` between results, or a dim rule in color.
    fn separator(self) -> String {
        if self.color {
            format!("\n\n{}\n\n", self.dim(&"─".repeat(48)))
        } else {
            "\n\n---\n\n".into()
        }
    }
    /// In color, indents continuation lines so a multi-line excerpt stays
    /// visually inside its result.
    fn hanging(self, text: &str, width: usize) -> String {
        if self.color {
            let pad = " ".repeat(width);
            text.split('\n')
                .enumerate()
                .map(|(index, line)| {
                    if index == 0 || line.is_empty() {
                        line.to_string()
                    } else {
                        format!("{pad}{line}")
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            text.to_string()
        }
    }
    /// `  [role date] text`, with terms highlighted in color.
    fn snippet(self, role: &str, date: &str, text: &str, terms: &[String]) -> String {
        if !self.color {
            return format!("  [{role}{date}] {text}");
        }
        let body = self.hanging(&crate::markdown::render(&highlight(text, terms, self)), 4);
        format!(
            "  {}{}{}{} {body}",
            self.dim("["),
            self.role(role),
            self.dim(date),
            self.dim("]")
        )
    }
}

/// Wraps every ASCII-case-insensitive occurrence of a term in the term color.
/// It ends with "off" codes instead of a reset so it nests inside Markdown styles.
pub(crate) fn highlight(text: &str, terms: &[String], paint: Paint) -> String {
    if !paint.color {
        return text.to_string();
    }
    let lower = text.to_ascii_lowercase();
    let needles: Vec<String> = terms
        .iter()
        .map(|term| term.to_ascii_lowercase())
        .filter(|term| !term.is_empty())
        .collect();
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while at < text.len() {
        let found = needles
            .iter()
            .filter_map(|needle| {
                lower[at..]
                    .find(needle.as_str())
                    .map(|i| (at + i, needle.len()))
            })
            .min_by_key(|&(start, len)| (start, std::cmp::Reverse(len)));
        let Some((start, len)) = found else { break };
        out.push_str(&text[at..start]);
        out.push_str(&format!(
            "\x1b[1;33m{}\x1b[22;39m",
            &text[start..start + len]
        ));
        at = start + len;
    }
    out.push_str(&text[at..]);
    out
}

/// `renderTranscript(view, options)`: a header line, then one labeled block per event.
pub fn render_transcript(view: &TranscriptView, options: RenderTranscriptOptions) -> String {
    let full = options.full;
    let paint = Paint {
        color: options.color,
    };
    let counts = &view.counts;
    let header = paint.dim(&format!(
        "{} · {} · {} user · {} assistant · {}",
        view.source,
        view.project,
        counts.user,
        counts.assistant,
        plural(counts.tool_calls, "tool call", "tool calls")
    ));
    let text_limit = if full { usize::MAX } else { TEXT_LIMIT };
    let blocks: Vec<String> = view
        .events
        .iter()
        .enumerate()
        .map(|(position, event)| match &event.body {
            EventBody::User { text } => format!(
                "{}\n{}",
                rule(&paint.user("USER"), event, paint),
                paint.markdown(&clip(text, text_limit))
            ),
            EventBody::Assistant { text } => format!(
                "{}\n{}",
                rule(&paint.assistant("ASSISTANT"), event, paint),
                paint.markdown(&clip(text, text_limit))
            ),
            EventBody::Thinking { text } => format!(
                "{}\n{}",
                rule(&paint.dim("THINKING"), event, paint),
                paint.dim(&clip(text, text_limit))
            ),
            EventBody::ToolCall { name, input, .. } => {
                let follows_turn = position
                    .checked_sub(1)
                    .and_then(|previous| view.events.get(previous))
                    .is_some_and(|previous| !matches!(previous.body, EventBody::User { .. }));
                let lead = if follows_turn {
                    String::new()
                } else {
                    format!("{}\n", rule(&paint.assistant("ASSISTANT"), event, paint))
                };
                format!(
                    "{lead}{}{} {}",
                    tag(event, paint),
                    paint.tool(&format!("▶ {name}")),
                    indent(&format_tool_input(input, full), "    ")
                )
            }
            EventBody::ToolResult {
                name,
                output,
                is_error,
                ..
            } => {
                let name = name.as_deref().unwrap_or("tool");
                let label = format!(
                    "{}{}",
                    tag(event, paint),
                    if *is_error {
                        paint.error(&format!("◀ {name} error"))
                    } else {
                        paint.dim(&format!("◀ {name} result"))
                    }
                );
                let body = if full {
                    output.clone()
                } else {
                    clip_lines(output, TOOL_OUTPUT_LINES, TOOL_OUTPUT_LIMIT)
                };
                if js_trim(&body).is_empty() {
                    format!("{label} {}", paint.dim("(empty)"))
                } else {
                    format!("{label}\n{}", indent_all(&paint.dim(&body), "    "))
                }
            }
        })
        .collect();
    format!("{header}\n\n{}", blocks.join("\n\n"))
}

fn rule(label: &str, event: &TranscriptEvent, paint: Paint) -> String {
    let when = match &event.timestamp {
        Some(timestamp) if !timestamp.is_empty() => {
            let shown = timestamp.replacen('T', " ", 1);
            format!(" {}", paint.dim(js::prefix(&shown, 19)))
        }
        _ => String::new(),
    };
    format!(
        "{} {}{label}{when} {}",
        paint.dim("───"),
        tag(event, paint),
        paint.dim(&"─".repeat(40))
    )
}

fn tag(event: &TranscriptEvent, paint: Paint) -> String {
    match event.index {
        None => String::new(),
        Some(index) => format!("{} ", paint.dim(&format!("#{index}"))),
    }
}

fn format_tool_input(input: &Value, full: bool) -> String {
    if let Value::String(text) = input {
        return if full {
            text.clone()
        } else {
            clip(text, TOOL_INPUT_LIMIT)
        };
    }
    let command = match input {
        Value::Object(record) => match record.get("command") {
            Some(Value::Null) | None => record.get("cmd"),
            found => found,
        }
        .and_then(Value::as_str)
        .filter(|command| !command.is_empty()),
        _ => None,
    };
    let keys = match input {
        Value::Object(record) => record.len(),
        Value::Array(items) => items.len(),
        _ => 0,
    };
    if let Some(command) = command
        && keys == 1
    {
        return if full {
            command.to_string()
        } else {
            clip(command, TOOL_INPUT_LIMIT)
        };
    }
    if full {
        js::pretty(input)
    } else {
        clip(&js::stringify(input), TOOL_INPUT_LIMIT)
    }
}

fn clip(text: &str, limit: usize) -> String {
    let trimmed = js_trim(text);
    if js::len(trimmed) <= limit {
        trimmed.to_string()
    } else {
        format!("{} [...]", js::prefix(trimmed, limit))
    }
}

fn clip_lines(text: &str, max_lines: usize, max_chars: usize) -> String {
    let lines: Vec<&str> = js_trim(text).split('\n').collect();
    let kept = lines[..lines.len().min(max_lines)].join("\n");
    let kept_chars = js::len(&kept);
    let clipped = if kept_chars > max_chars {
        js::prefix(&kept, max_chars)
    } else {
        &kept
    };
    if lines.len() > max_lines || kept_chars > max_chars {
        format!(
            "{clipped}\n[... {} lines, {} chars]",
            lines.len(),
            js::len(text)
        )
    } else {
        clipped.to_string()
    }
}

fn indent_all(text: &str, prefix: &str) -> String {
    text.split('\n')
        .map(|line| format!("{prefix}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn indent(text: &str, prefix: &str) -> String {
    text.split('\n')
        .enumerate()
        .map(|(index, line)| {
            if index == 0 {
                line.to_string()
            } else {
                format!("{prefix}{line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TranscriptSource;
    use crate::view::{ShowMessage, TranscriptCounts};
    use serde_json::json;

    fn event(body: EventBody, index: usize, timestamp: Option<&str>) -> TranscriptEvent {
        let mut event = TranscriptEvent::new(body);
        event.index = Some(index);
        event.timestamp = timestamp.map(str::to_string);
        event
    }

    fn strip_ansi(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn highlight_is_case_insensitive_and_prefers_longer_terms() {
        let paint = Paint { color: true };
        assert_eq!(
            highlight("Gay gayathri", &["gay".into(), "gayathri".into()], paint),
            "\x1b[1;33mGay\x1b[22;39m \x1b[1;33mgayathri\x1b[22;39m"
        );
    }

    #[test]
    fn transcript_layout_and_colors() {
        let view = TranscriptView {
            path: "/x".into(),
            source: TranscriptSource::Codex,
            project: "proj".into(),
            counts: TranscriptCounts {
                user: 1,
                assistant: 0,
                thinking: 0,
                tool_calls: 1,
                tool_results: 1,
            },
            events: vec![
                event(
                    EventBody::User {
                        text: " hi ".into(),
                    },
                    0,
                    Some("2026-08-01T08:00:00.000Z"),
                ),
                event(
                    EventBody::ToolCall {
                        name: "shell".into(),
                        input: json!({ "cmd": "ls" }),
                        call_id: None,
                    },
                    1,
                    None,
                ),
                event(
                    EventBody::ToolResult {
                        name: None,
                        call_id: None,
                        output: " \n".into(),
                        is_error: true,
                    },
                    2,
                    None,
                ),
            ],
        };
        let plain = render_transcript(&view, RenderTranscriptOptions::default());
        let rule = "─".repeat(40);
        assert_eq!(
            plain,
            format!(
                "codex · proj · 1 user · 0 assistant · 1 tool call\n\n─── #0 USER 2026-08-01 08:00:00 {rule}\nhi\n\n─── #1 ASSISTANT {rule}\n#1 ▶ shell ls\n\n#2 ◀ tool error (empty)"
            )
        );
        let colored = render_transcript(
            &view,
            RenderTranscriptOptions {
                full: false,
                color: true,
            },
        );
        assert!(colored.contains("\x1b[1;36mUSER\x1b[0m"));
        assert!(colored.contains("\x1b[31m◀ tool error\x1b[0m"));
    }

    #[test]
    fn tool_inputs_and_outputs_are_clipped() {
        assert_eq!(
            format_tool_input(&json!({ "command": "ls", "x": 1 }), false),
            r#"{"command":"ls","x":1}"#
        );
        assert_eq!(
            format_tool_input(&json!({ "command": "ls", "x": 1 }), true),
            "{\n  \"command\": \"ls\",\n  \"x\": 1\n}"
        );
        assert_eq!(
            format_tool_input(&json!({ "command": null, "cmd": "pwd" }), false),
            r#"{"command":null,"cmd":"pwd"}"#
        );
        assert_eq!(clip(&"a".repeat(5), 3), "aaa [...]");
        assert_eq!(
            clip_lines("a\nb\nc", 2, 100),
            "a\nb\n[... 3 lines, 5 chars]"
        );
        assert_eq!(clip_lines("abcdef", 8, 3), "abc\n[... 1 lines, 6 chars]");
    }

    #[test]
    fn search_find_show_and_query() {
        assert_eq!(
            render_search(&json!({ "query": "x", "matches": [] }), false),
            "No sessions found matching \"x\".\n\nSearch is literal, not semantic. Retry with one exact distinctive token or phrase."
        );
        let search = json!({ "query": "x", "matches": [{ "source": "pi", "path": "/p", "count": 1, "date": "2026-08-01", "project": "proj", "snippets": [{ "role": "user", "text": "a x" }] }] });
        assert_eq!(
            render_search(&search, false),
            "Found 1 session matching \"x\":\n\n2026-08-01 · pi · proj · 1 match\nTranscript: /p\n  [user] a x"
        );
        let find = json!({ "terms": ["a", "b"], "requiredTerms": ["a"], "hits": [{ "source": "claude", "path": "/c", "project": "proj", "date": "2026-08-02", "score": 5, "termCounts": { "a": { "user": 1, "assistant": 2 } }, "openingPrompt": "", "matches": [{ "role": "user", "date": "2026-08-02", "text": "a" }], "resume": "claude --resume x" }] });
        assert_eq!(
            render_find(&find, false),
            "No session matched all 2 terms; showing sessions matching 1.\n\nFound 1 session for a + b:\n\n2026-08-02 · claude · proj · a×3(u1)\n  [user 2026-08-02] a\nTranscript: /c\nResume: claude --resume x"
        );
        assert_eq!(
            render_find(
                &json!({ "terms": ["a", "b"], "requiredTerms": [], "hits": [] }),
                false
            ),
            "No sessions found for: a, b.\n\nTerms are literal (AND). Try fewer or different exact terms, or drop filters."
        );
        let colored = render_find(&find, true);
        assert!(colored.contains("\x1b[1;38;5;209mclaude\x1b[0m"));
        assert!(colored.contains("\x1b[1;33ma\x1b[0m"));
        assert_eq!(strip_ansi(&colored), render_find(&find, false));
        assert_eq!(
            strip_ansi(&render_search(&search, true)).replace("─".repeat(48).as_str(), "---"),
            render_search(&search, false)
        );
        assert_eq!(render_query(&json!({ "answer": "yes" })), "yes");
        let show = ShowResult {
            path: "/s".into(),
            source: TranscriptSource::Claude,
            message_count: 2,
            messages: vec![
                ShowMessage {
                    role: "user".into(),
                    text: "q".into(),
                },
                ShowMessage {
                    role: "assistant".into(),
                    text: "a".into(),
                },
            ],
        };
        assert_eq!(render_show(&show), "[user]\nq\n\n[assistant]\na");
    }
}
