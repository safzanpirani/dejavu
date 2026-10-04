//! `querySession` from `core.ts`: answer a question about one transcript with
//! a model, windowing long sessions around the question's terms.
//!
//! [`prepare_recall_messages`] is public because `show` and `find` use the
//! same preparation.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Instant;

use serde::Serialize;

use crate::codex_client::{Cancel, Completion, QueryUsage, js_trim};
use crate::model_client::{self, ResolvedQueryModel};
use crate::paths::js_lower;
use crate::reader;
use crate::sources::{default_roots, source_from_locator};
use crate::types::{RecallMessage, TranscriptSource};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QueryModel {
    pub provider: String,
    pub id: String,
    #[serde(rename = "reasoningEffort", skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

/// The `query` result, in the TypeScript key order. `usage` and `costUsd`
/// are absent from JSON when unknown.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryResult {
    pub source: TranscriptSource,
    pub session_path: String,
    pub question: String,
    pub answer: String,
    pub model: QueryModel,
    pub transport: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<QueryUsage>,
    /// Estimated USD for this call, when the model entry records prices.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    pub message_count: usize,
    pub was_windowed: bool,
    pub elapsed_ms: u64,
}

/// The seams `querySession` took as `QueryDeps`. The defaults are the real
/// implementations; tests override what they need.
pub trait QueryDeps {
    fn detect_source(&self, locator: &str) -> Result<TranscriptSource, String> {
        source_from_locator(locator, default_roots())
    }

    fn path_exists(&self, path: &str) -> bool {
        Path::new(path).metadata().is_ok()
    }

    fn load_messages(
        &self,
        locator: &str,
        source: TranscriptSource,
    ) -> Result<Vec<RecallMessage>, String> {
        reader::load_recall_messages(locator, Some(source))
    }

    fn resolve_model(
        &self,
        agent_dir: &Path,
        settings: &crate::query_config::QuerySettings,
    ) -> Result<ResolvedQueryModel, String> {
        crate::query_config::resolve_model(agent_dir, settings)
    }

    fn serialize(&self, messages: &[RecallMessage]) -> String {
        reader::serialize_recall_messages(messages)
    }

    fn complete(
        &self,
        resolved: &ResolvedQueryModel,
        conversation: &str,
        question: &str,
        cancel: &Cancel,
    ) -> Result<Completion, String> {
        model_client::complete_query(resolved, conversation, question, cancel)
    }
}

/// The real implementations.
pub struct RealQuery;

impl QueryDeps for RealQuery {}

pub struct QueryOptions<'a> {
    pub agent_dir: &'a Path,
    pub settings: crate::query_config::QuerySettings,
}

/// `prepareRecallMessages`: user and assistant messages only, without
/// thinking blocks, and without messages left empty. The Rust reader never
/// emits thinking blocks, so only the role and emptiness filters remain.
pub fn prepare_recall_messages(messages: Vec<RecallMessage>) -> Vec<RecallMessage> {
    messages
        .into_iter()
        .filter(|message| message.role == "user" || message.role == "assistant")
        .filter(|message| !message.content.is_empty())
        .collect()
}

/// One serialized message for [`build_windowed_context`].
#[derive(Debug, Clone, PartialEq)]
pub struct SerializedMessage {
    pub role: String,
    pub text: String,
    /// `text.length` in UTF-16 units.
    pub char_count: usize,
}

const STOP_WORDS: &[&str] = &[
    "a", "an", "the", "is", "was", "were", "are", "be", "been", "being", "have", "has", "had",
    "do", "does", "did", "will", "would", "could", "should", "may", "might", "can", "shall", "to",
    "of", "in", "for", "on", "with", "at", "by", "from", "as", "into", "about", "like", "through",
    "after", "over", "between", "out", "against", "during", "without", "before", "under", "around",
    "among", "and", "but", "or", "nor", "not", "so", "yet", "both", "either", "neither", "each",
    "every", "all", "any", "few", "more", "most", "other", "some", "such", "no", "only", "own",
    "same", "than", "too", "very", "just", "because", "if", "when", "where", "how", "what",
    "which", "who", "whom", "this", "that", "these", "those", "it", "its", "they", "them", "their",
    "we", "us", "our", "you", "your", "he", "him", "his", "she", "her", "i", "me", "my",
];

/// JavaScript's `\s`.
pub fn js_space(ch: char) -> bool {
    matches!(
        ch,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

/// `extractKeywords(question)`: lowercase words longer than two units that
/// are not stop words. Characters other than `\w`, `\s`, and `-` split words.
pub fn extract_keywords(question: &str) -> Vec<String> {
    let cleaned: String = js_lower(question)
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || js_space(ch) {
                ch
            } else {
                ' '
            }
        })
        .collect();
    cleaned
        .split(js_space)
        .filter(|word| crate::js::len(word) > 2 && !STOP_WORDS.contains(word))
        .map(str::to_string)
        .collect()
}

fn format_serialized(message: &SerializedMessage) -> String {
    format!("[{}]\n{}", message.role, message.text)
}

/// `buildWindowedContext`: the first and last three messages, the messages
/// that match the most question terms, their neighbors, then the earliest
/// remaining messages, all within 80% of `token_budget * 4` characters.
/// Gaps read `[... N messages omitted ...]`.
pub fn build_windowed_context(
    messages: &[SerializedMessage],
    question: &str,
    token_budget: u64,
) -> String {
    const BOOKENDS: usize = 3;
    if messages.len() <= BOOKENDS * 2 + 2 {
        return messages
            .iter()
            .map(format_serialized)
            .collect::<Vec<_>>()
            .join("\n\n");
    }
    let keywords = extract_keywords(question);
    let mut scored: Vec<(usize, usize)> = messages
        .iter()
        .enumerate()
        .map(|(index, message)| {
            let lower = js_lower(&message.text);
            let score = keywords
                .iter()
                .filter(|keyword| lower.contains(keyword.as_str()))
                .count();
            (index, score)
        })
        .filter(|(_, score)| *score > 0)
        .collect();
    scored.sort_by_key(|item| std::cmp::Reverse(item.1));
    let matches: Vec<usize> = scored.into_iter().map(|(index, _)| index).collect();

    let limit = token_budget as f64 * 4.0 * 0.8;
    let mut included = BTreeSet::new();
    let mut used = 0.0_f64;
    let cost = |index: usize| messages[index].char_count as f64 + 20.0;
    let include = |index: usize, included: &mut BTreeSet<usize>, used: &mut f64| {
        if included.insert(index) {
            *used += cost(index);
        }
    };
    let fits = |index: usize, used: f64| used + cost(index) <= limit;
    for i in 0..BOOKENDS.min(messages.len()) {
        include(i, &mut included, &mut used);
    }
    for i in messages.len().saturating_sub(BOOKENDS)..messages.len() {
        include(i, &mut included, &mut used);
    }
    // The best match always goes in; later matches only while they fit, since
    // a common keyword can match most of a long session.
    let mut kept: Vec<usize> = Vec::new();
    for index in matches {
        if !kept.is_empty() && !included.contains(&index) && !fits(index, used) {
            continue;
        }
        include(index, &mut included, &mut used);
        kept.push(index);
    }
    let mut radius: usize = 1;
    while used < limit && radius < messages.len() {
        let previous = included.len();
        for &index in &kept {
            for candidate in index.saturating_sub(radius)..=index + radius {
                // JavaScript visited negative candidates and skipped them.
                if candidate + radius < index || candidate >= messages.len() {
                    continue;
                }
                if fits(candidate, used) {
                    include(candidate, &mut included, &mut used);
                }
            }
        }
        if included.len() == previous {
            break;
        }
        radius += 1;
    }
    let mut i = BOOKENDS;
    while i < messages.len() && used < limit {
        if fits(i, used) {
            include(i, &mut included, &mut used);
        }
        i += 1;
    }
    let mut parts = Vec::new();
    let mut previous: Option<usize> = None;
    for index in included {
        if let Some(previous) = previous
            && index > previous + 1
        {
            let gap = index - previous - 1;
            parts.push(format!(
                "[... {gap} message{} omitted ...]",
                if gap == 1 { "" } else { "s" }
            ));
        }
        parts.push(format_serialized(&messages[index]));
        previous = Some(index);
    }
    parts.join("\n\n")
}

/// What the model receives: the serialized conversation, windowed when it
/// exceeds 80% of the model's context window (four characters per token).
/// Returns the conversation and whether it was windowed.
pub fn prepare_conversation(
    messages: &[RecallMessage],
    question: &str,
    context_window: u64,
    serialize: &dyn Fn(&[RecallMessage]) -> String,
) -> (String, bool) {
    let full = serialize(messages);
    let token_budget = (context_window as f64 * 0.8).floor() as u64;
    let was_windowed = crate::js::len(&full).div_ceil(4) as u64 > token_budget;
    if !was_windowed {
        return (full, false);
    }
    let serialized: Vec<SerializedMessage> = messages
        .iter()
        .map(|message| {
            let text = serialize(std::slice::from_ref(message));
            SerializedMessage {
                role: message.role.clone(),
                char_count: crate::js::len(&text),
                text,
            }
        })
        .collect();
    (
        build_windowed_context(&serialized, question, token_budget),
        true,
    )
}

/// `querySession(sessionPath, question, options)`.
pub fn query_session(
    session_path: &str,
    question: &str,
    options: &QueryOptions,
    cancel: &Cancel,
    deps: &dyn QueryDeps,
) -> Result<QueryResult, String> {
    let question = js_trim(question);
    if question.is_empty() {
        return Err("question must not be empty".into());
    }
    let source = deps.detect_source(session_path)?;
    if source != TranscriptSource::Opencode && !deps.path_exists(session_path) {
        return Err(format!("transcript not found: {session_path}"));
    }
    let started = Instant::now();
    let messages = prepare_recall_messages(deps.load_messages(session_path, source)?);
    if messages.is_empty() {
        return Err("transcript has no recallable messages".into());
    }
    let resolved = deps.resolve_model(options.agent_dir, &options.settings)?;
    let (conversation, was_windowed) =
        prepare_conversation(&messages, question, resolved.context_window, &|messages| {
            deps.serialize(messages)
        });
    let completion = deps.complete(&resolved, &conversation, question, cancel)?;
    Ok(QueryResult {
        source,
        session_path: session_path.to_string(),
        question: question.to_string(),
        answer: completion.answer,
        model: QueryModel {
            provider: resolved.provider.clone(),
            id: resolved.id.clone(),
            reasoning_effort: resolved.reasoning_effort,
        },
        transport: completion.transport,
        usage: completion.usage,
        cost_usd: model_client::estimate_cost(resolved.cost, completion.usage),
        message_count: messages.len(),
        was_windowed,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

/// The stderr summary line `cli.ts` printed after a text-mode query (undimmed).
pub fn summary_line(result: &QueryResult) -> String {
    let tokens = result
        .usage
        .map(|usage| format!(" · {} in / {} out", usage.input_tokens, usage.output_tokens))
        .unwrap_or_default();
    let cost = result
        .cost_usd
        .map(|cost| format!(" · ~${cost:.4}"))
        .unwrap_or_default();
    let reasoning = result
        .model
        .reasoning_effort
        .as_deref()
        .map(|effort| format!(" · {effort}"))
        .unwrap_or_default();
    format!(
        "{} · {}/{}{reasoning} · {} · {} message{}{}{tokens}{cost} · {}ms",
        result.source,
        result.model.provider,
        result.model.id,
        result.transport,
        result.message_count,
        if result.message_count == 1 { "" } else { "s" },
        if result.was_windowed {
            " · windowed"
        } else {
            ""
        },
        result.elapsed_ms,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex_client::{CodexDeps, complete_via_codex_with};
    // Used by the tests that run fake executables, which are shell scripts.
    #[cfg(unix)]
    use crate::model_client::{SYSTEM_PROMPT, build_prompt, complete_via_pi_with};
    use crate::types::RecallBlock;
    use std::cell::RefCell;
    use std::path::PathBuf;

    fn text_message(role: &str, text: &str) -> RecallMessage {
        RecallMessage {
            role: role.into(),
            content: vec![RecallBlock::Text { text: text.into() }],
        }
    }

    fn texts(messages: &[RecallMessage]) -> Vec<&str> {
        messages
            .iter()
            .map(|message| message.content[0].text().unwrap_or_default())
            .collect()
    }

    fn resolved(provider: &str, context_window: u64) -> ResolvedQueryModel {
        ResolvedQueryModel {
            provider: provider.into(),
            id: "tiny".into(),
            context_window,
            agent_dir: PathBuf::from("/pi"),
            reasoning_effort: None,
            harness: crate::query_config::Harness::Codex,
            direct: None,
            cost: None,
        }
    }

    #[test]
    fn drops_non_conversation_roles_and_windows_around_question_terms() {
        let prepared = prepare_recall_messages(vec![
            text_message("toolResult", "large output"),
            text_message("user", "question"),
            RecallMessage {
                role: "assistant".into(),
                content: vec![],
            },
        ]);
        assert_eq!(texts(&prepared), ["question"]);
        let messages: Vec<SerializedMessage> = (0..12)
            .map(|index| SerializedMessage {
                role: if index % 2 == 1 { "assistant" } else { "user" }.into(),
                text: if index == 6 {
                    "the distinctive marmalade decision".into()
                } else {
                    format!("filler {index}")
                },
                char_count: 500,
            })
            .collect();
        let windowed = build_windowed_context(&messages, "What was the marmalade decision?", 100);
        assert!(windowed.contains("omitted ...]"));
        assert!(windowed.contains("marmalade"));
    }

    #[test]
    fn keeps_keyword_matches_within_the_token_budget() {
        let messages: Vec<SerializedMessage> = (0..400)
            .map(|index| SerializedMessage {
                role: if index % 2 == 1 { "assistant" } else { "user" }.into(),
                text: format!("session note {index} {}", "x".repeat(1000)),
                char_count: 1020,
            })
            .collect();
        let windowed = build_windowed_context(&messages, "what was this session about", 10_000);
        assert!(windowed.len() < 10_000 * 4);
        assert!(windowed.contains("messages omitted ...]"));
    }

    #[test]
    fn keywords_drop_stop_words_and_punctuation() {
        assert_eq!(
            extract_keywords("What was the Marmalade-decision, in core.ts?"),
            ["marmalade-decision", "core"]
        );
        assert!(extract_keywords("  ").is_empty());
    }

    /// The exact text a synthetic session produces, which the TypeScript
    /// `buildWindowedContext` also produced for the same input (double role
    /// headers included: each message's text is already serialized).
    #[test]
    fn windowed_text_matches_the_typescript_layout() {
        let messages: Vec<RecallMessage> = (0..10)
            .map(|index| {
                text_message(
                    if index % 2 == 1 { "assistant" } else { "user" },
                    &if index == 5 {
                        "we picked sqlite".to_string()
                    } else {
                        format!("note {index} {}", "y".repeat(30))
                    },
                )
            })
            .collect();
        let (conversation, windowed) = prepare_conversation(
            &messages,
            "Why sqlite?",
            60,
            &reader::serialize_recall_messages,
        );
        assert!(windowed);
        let expected = [
            "[user]\n[user]\nnote 0 yyyyyyyyyyyyyyyyyyyyyyyyyyyyyy",
            "[assistant]\n[assistant]\nnote 1 yyyyyyyyyyyyyyyyyyyyyyyyyyyyyy",
            "[user]\n[user]\nnote 2 yyyyyyyyyyyyyyyyyyyyyyyyyyyyyy",
            "[... 2 messages omitted ...]",
            "[assistant]\n[assistant]\nwe picked sqlite",
            "[... 1 message omitted ...]",
            "[assistant]\n[assistant]\nnote 7 yyyyyyyyyyyyyyyyyyyyyyyyyyyyyy",
            "[user]\n[user]\nnote 8 yyyyyyyyyyyyyyyyyyyyyyyyyyyyyy",
            "[assistant]\n[assistant]\nnote 9 yyyyyyyyyyyyyyyyyyyyyyyyyyyyyy",
        ]
        .join("\n\n");
        assert_eq!(conversation, expected);
        let (full, windowed) = prepare_conversation(
            &messages,
            "Why sqlite?",
            128_000,
            &reader::serialize_recall_messages,
        );
        assert!(!windowed);
        assert_eq!(full, reader::serialize_recall_messages(&messages));
    }

    type FakeComplete = Box<dyn Fn(&ResolvedQueryModel, &str, &str) -> Result<Completion, String>>;

    struct Fake {
        resolved: ResolvedQueryModel,
        complete: FakeComplete,
        seen: RefCell<Option<String>>,
    }

    impl QueryDeps for Fake {
        fn detect_source(&self, _: &str) -> Result<TranscriptSource, String> {
            Ok(TranscriptSource::Codex)
        }
        fn path_exists(&self, _: &str) -> bool {
            true
        }
        fn load_messages(
            &self,
            _: &str,
            _: TranscriptSource,
        ) -> Result<Vec<RecallMessage>, String> {
            Ok(vec![text_message("user", "We chose SQLite.")])
        }
        fn resolve_model(
            &self,
            _: &Path,
            _: &crate::query_config::QuerySettings,
        ) -> Result<ResolvedQueryModel, String> {
            Ok(self.resolved.clone())
        }
        fn complete(
            &self,
            resolved: &ResolvedQueryModel,
            conversation: &str,
            question: &str,
            _: &Cancel,
        ) -> Result<Completion, String> {
            *self.seen.borrow_mut() = Some(conversation.to_string());
            (self.complete)(resolved, conversation, question)
        }
    }

    fn options() -> QueryOptions<'static> {
        QueryOptions {
            agent_dir: Path::new("/pi"),
            settings: crate::query_config::QuerySettings::default(),
        }
    }

    #[test]
    fn reports_the_detected_source_in_query_results() {
        let fake = Fake {
            resolved: resolved("test", 1000),
            complete: Box::new(|_, conversation, question| {
                Ok(Completion {
                    answer: format!("{question} {}", conversation.contains("SQLite")),
                    transport: "pi",
                    usage: None,
                })
            }),
            seen: RefCell::default(),
        };
        let result = query_session(
            "/tmp/a.jsonl",
            " What database? ",
            &options(),
            &Cancel::new(),
            &fake,
        )
        .unwrap();
        assert_eq!(result.source, TranscriptSource::Codex);
        assert_eq!(result.answer, "What database? true");
        assert_eq!(result.question, "What database?");
        assert!(!result.was_windowed);
        let json = crate::js::pretty(&result);
        let keys: Vec<&str> = json
            .lines()
            .filter(|line| line.starts_with("  \""))
            .map(|line| line.trim().split('"').nth(1).unwrap())
            .collect();
        assert_eq!(
            keys,
            [
                "source",
                "sessionPath",
                "question",
                "answer",
                "model",
                "transport",
                "messageCount",
                "wasWindowed",
                "elapsedMs"
            ]
        );
    }

    #[test]
    fn rejects_empty_questions_and_missing_transcripts() {
        assert_eq!(
            query_session("/tmp/a.jsonl", "  ", &options(), &Cancel::new(), &RealQuery),
            Err("question must not be empty".into())
        );
        let missing = "/nonexistent-dejavu/.codex/sessions/x.jsonl";
        assert_eq!(
            query_session(missing, "q", &options(), &Cancel::new(), &RealQuery),
            Err(format!("transcript not found: {missing}"))
        );
    }

    #[cfg(unix)]
    fn script(dir: &Path, name: &str, body: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().into_owned()
    }

    /// End to end through a fake `codex` executable: the prompt carries the
    /// system prompt, the conversation, and the question; usage and cost
    /// come back in the result and the summary line.
    #[cfg(unix)]
    #[test]
    fn answers_through_a_fake_codex_executable() {
        let dir = crate::codex_client::TempDir::new("dejavu-query-test-").unwrap();
        let prompt_file = dir.path().join("prompt.txt");
        let codex = script(
            dir.path(),
            "codex",
            &format!(
                r#"#!/bin/sh
out=
prev=
for arg in "$@"; do
  [ "$prev" = "--output-last-message" ] && out=$arg
  prev=$arg
done
cat > '{}'
printf 'SQLite, per the session.\n' > "$out"
echo '{{"type":"turn.completed","usage":{{"input_tokens":1200,"output_tokens":30}}}}'
"#,
                prompt_file.display()
            ),
        );
        let mut model = resolved("codex", 128_000);
        model.reasoning_effort = Some("medium".into());
        model.cost = Some(model_client::ModelCost {
            input: 1.0,
            output: 2.0,
        });
        let fake = Fake {
            resolved: model,
            complete: Box::new(move |resolved, conversation, question| {
                complete_via_codex_with(
                    &resolved.id,
                    &format!(
                        "{SYSTEM_PROMPT}\n\n{}",
                        build_prompt(conversation, question)
                    ),
                    &Cancel::new(),
                    &CodexDeps {
                        command: Some(vec![codex.clone()]),
                        timeout: None,
                        effort: None,
                    },
                )
            }),
            seen: RefCell::default(),
        };
        let result = query_session(
            "/x.jsonl",
            "What database?",
            &options(),
            &Cancel::new(),
            &fake,
        )
        .unwrap();
        assert_eq!(result.answer, "SQLite, per the session.");
        assert_eq!(result.transport, "codex");
        assert_eq!(
            result.usage,
            Some(QueryUsage {
                input_tokens: 1200,
                output_tokens: 30
            })
        );
        assert_eq!(result.cost_usd, Some(0.00126));
        let prompt = std::fs::read_to_string(&prompt_file).unwrap();
        assert!(prompt.starts_with(SYSTEM_PROMPT));
        assert!(prompt.contains("## Session Conversation\n\n[user]\nWe chose SQLite."));
        assert!(prompt.ends_with("## Question\n\nWhat database?"));
        let line = summary_line(&result);
        assert!(line.starts_with("codex · codex/tiny · medium · codex · 1 message · "));
        assert!(line.contains(" · 1200 in / 30 out · ~$0.0013 · "));
        assert!(line.ends_with("ms"));
        let json = crate::js::pretty(&result);
        assert!(json.contains("\"reasoningEffort\": \"medium\""));
        assert!(json.contains("\"costUsd\": 0.00126"));
    }

    /// The legacy Pi transport through a fake `pi` executable.
    #[cfg(unix)]
    #[test]
    fn answers_through_a_fake_pi_executable() {
        let dir = crate::codex_client::TempDir::new("dejavu-query-test-").unwrap();
        let pi = script(
            dir.path(),
            "pi",
            "#!/bin/sh\ncat >/dev/null\necho \"$PI_CODING_AGENT_DIR answer\"\n",
        );
        let fake = Fake {
            resolved: resolved("groq", 128_000),
            complete: Box::new(move |resolved, conversation, question| {
                complete_via_pi_with(
                    resolved,
                    &build_prompt(conversation, question),
                    &Cancel::new(),
                    &pi,
                )
            }),
            seen: RefCell::default(),
        };
        let result = query_session("/x.jsonl", "q?", &options(), &Cancel::new(), &fake).unwrap();
        assert_eq!(result.answer, "/pi answer");
        assert_eq!(result.transport, "pi");
        assert!(
            crate::js::pretty(&result)
                .contains("\"model\": {\n    \"provider\": \"groq\",\n    \"id\": \"tiny\"\n  },")
        );
        assert!(summary_line(&result).starts_with("codex · groq/tiny · pi · 1 message · "));
    }

    #[test]
    fn windows_when_the_conversation_exceeds_the_budget() {
        let fake = Fake {
            resolved: resolved("test", 1),
            complete: Box::new(|_, _, _| {
                Ok(Completion {
                    answer: "a".into(),
                    transport: "http",
                    usage: None,
                })
            }),
            seen: RefCell::default(),
        };
        let result = query_session("/x.jsonl", "q", &options(), &Cancel::new(), &fake).unwrap();
        assert!(result.was_windowed);
        assert!(summary_line(&result).contains(" · windowed · "));
        // Eight or fewer messages are joined whole, with doubled headers.
        assert_eq!(
            fake.seen.borrow().as_deref(),
            Some("[user]\n[user]\nWe chose SQLite.")
        );
    }

    #[test]
    fn cancellation_reaches_the_transport() {
        let cancel = Cancel::new();
        cancel.cancel();
        let error = complete_via_codex_with(
            "m",
            "p",
            &cancel,
            &CodexDeps {
                command: Some(vec!["/nonexistent".into()]),
                timeout: None,
                effort: None,
            },
        )
        .unwrap_err();
        assert_eq!(error, "query was cancelled");
    }

    /// Compares the prepared conversation with output the TypeScript wrote for
    /// synthetic sessions. Run with `DEJAVU_PREP_FIXTURE=<json> cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn prepared_conversations_match_typescript_fixtures() {
        let path = std::env::var("DEJAVU_PREP_FIXTURE").expect("DEJAVU_PREP_FIXTURE");
        let fixture: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let cases = fixture["cases"].as_array().unwrap();
        for (number, case) in cases.iter().enumerate() {
            let messages: Vec<RecallMessage> = case["messages"]
                .as_array()
                .unwrap()
                .iter()
                .map(|message| RecallMessage {
                    role: message["role"].as_str().unwrap().into(),
                    content: message["content"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|block| match block["type"].as_str().unwrap() {
                            "text" => RecallBlock::Text {
                                text: block["text"].as_str().unwrap().into(),
                            },
                            "toolCall" => RecallBlock::ToolCall {
                                name: block["name"].as_str().unwrap().into(),
                                arguments: block["arguments"].clone(),
                            },
                            _ => RecallBlock::Image,
                        })
                        .collect(),
                })
                .collect();
            let prepared = prepare_recall_messages(messages);
            assert_eq!(
                prepared.len() as u64,
                case["preparedCount"].as_u64().unwrap()
            );
            let (conversation, windowed) = prepare_conversation(
                &prepared,
                case["question"].as_str().unwrap(),
                case["contextWindow"].as_u64().unwrap(),
                &reader::serialize_recall_messages,
            );
            assert_eq!(
                windowed,
                case["wasWindowed"].as_bool().unwrap(),
                "case {number}"
            );
            assert_eq!(
                conversation,
                case["conversation"].as_str().unwrap(),
                "case {number}"
            );
        }
        eprintln!("{} cases match", cases.len());
    }
}
