//! Antigravity CLI (`agy`) transcripts. Each conversation keeps one JSON step
//! per line in `<root>/<conversation id>/.system_generated/logs/transcript_full.jsonl`
//! (`transcript.jsonl` beside it trims long tool output), where `<root>` is
//! `~/.gemini/antigravity-cli/brain`. The conversation's workspace lives in
//! `conversation_summaries.db` next to `brain`.

use crate::js;
use crate::paths::{compact_home, join};
use crate::types::{RecallBlock, RecallMessage};
use crate::view::js_trim;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

const LOG_DIR: [&str; 2] = [".system_generated", "logs"];
const TRANSCRIPTS: [&str; 2] = ["transcript_full.jsonl", "transcript.jsonl"];

/// One transcript per conversation directory under `root`: the full log when
/// it exists, else the trimmed one. Symlinked and hidden directories are skipped.
pub fn transcript_files(root: &str) -> Result<Vec<String>, String> {
    let entries = std::fs::read_dir(root)
        .map_err(|error| crate::reader::fs_error(&error, "scandir", root))?;
    let mut files = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let logs = join(&[root, &name, LOG_DIR[0], LOG_DIR[1]]);
        if let Some(path) = TRANSCRIPTS
            .iter()
            .map(|file| join(&[&logs, file]))
            .find(|path| std::fs::metadata(path).is_ok_and(|meta| meta.is_file()))
        {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

/// The conversation ID of a transcript path: the directory above `.system_generated`.
pub fn conversation_id(path: &str) -> Option<&str> {
    let mut parts = path.rsplit(['/', '\\']);
    let file = parts.next()?;
    if !TRANSCRIPTS.contains(&file) || parts.next()? != LOG_DIR[1] || parts.next()? != LOG_DIR[0] {
        return None;
    }
    parts.next().filter(|id| !id.is_empty())
}

/// What one step line holds that dejavu reads.
pub struct Step {
    pub kind: String,
    pub source: String,
    pub status: String,
    pub content: Option<String>,
    pub thinking: Option<String>,
    pub error: Option<String>,
    pub tool_calls: Vec<(String, Value)>,
    pub created_at: Option<String>,
}

impl Step {
    pub fn parse(line: &str) -> Option<Step> {
        Step::from_value(&serde_json::from_str(line.trim()).ok()?)
    }

    pub fn from_value(value: &Value) -> Option<Step> {
        let object = value.as_object()?;
        let text = |key: &str| object.get(key).and_then(Value::as_str).map(str::to_string);
        let tool_calls = object
            .get("tool_calls")
            .and_then(Value::as_array)
            .map(|calls| {
                calls
                    .iter()
                    .map(|call| {
                        let name = call
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                            .to_string();
                        (name, decode_args(call.get("args")))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Some(Step {
            kind: text("type").unwrap_or_default(),
            source: text("source").unwrap_or_default(),
            status: text("status").unwrap_or_default(),
            content: text("content"),
            thinking: text("thinking"),
            error: text("error"),
            tool_calls,
            created_at: text("created_at"),
        })
    }

    /// The user's own words: `<USER_REQUEST>` text of an explicit user input.
    /// Hook injections arrive as `SYSTEM_SDK` user inputs and do not count.
    pub fn user_text(&self) -> Option<String> {
        if self.kind != "USER_INPUT" || self.source != "USER_EXPLICIT" {
            return None;
        }
        let text = user_request(self.content.as_deref()?);
        (!js_trim(text).is_empty()).then(|| js_trim(text).to_string())
    }

    /// A model reply's visible text.
    pub fn assistant_text(&self) -> Option<String> {
        if self.kind != "PLANNER_RESPONSE" {
            return None;
        }
        self.content
            .as_deref()
            .map(js_trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    }

    /// Whether this step reports the outcome of a tool call: any model step
    /// other than a reply, or a tool-call error.
    pub fn is_tool_result(&self) -> bool {
        (self.source == "MODEL" && self.kind != "PLANNER_RESPONSE") || self.kind == "ERROR_MESSAGE"
    }

    /// A tool result's output without the `Created At` / `Completed At` header.
    pub fn tool_output(&self) -> String {
        let text = self
            .content
            .as_deref()
            .or(self.error.as_deref())
            .unwrap_or("");
        let mut rest = text;
        for header in ["Created At: ", "Completed At: "] {
            if let Some(after) = rest.strip_prefix(header) {
                rest = after.split_once('\n').map_or("", |(_, tail)| tail);
            }
        }
        rest.trim_start_matches('\n').to_string()
    }

    pub fn is_error(&self) -> bool {
        self.kind == "ERROR_MESSAGE" || self.status == "ERROR"
    }

    pub fn date(&self) -> Option<String> {
        self.created_at
            .as_deref()
            .map(|timestamp| js::prefix(timestamp, 10).to_string())
    }
}

/// The text inside `<USER_REQUEST>`; agy appends metadata and settings
/// changes after it. Text without the wrapper is returned whole.
pub fn user_request(content: &str) -> &str {
    let Some(start) = content.find("<USER_REQUEST>") else {
        return content;
    };
    let body = &content[start + "<USER_REQUEST>".len()..];
    body.find("</USER_REQUEST>")
        .map_or(body, |end| &body[..end])
}

/// agy stores each tool argument as a JSON-encoded string (`"\"ls\""`);
/// decode the ones that parse and keep the rest as written.
fn decode_args(args: Option<&Value>) -> Value {
    match args {
        Some(Value::Object(object)) => Value::Object(
            object
                .iter()
                .map(|(key, value)| {
                    let decoded = value
                        .as_str()
                        .and_then(|text| serde_json::from_str::<Value>(text).ok())
                        .unwrap_or_else(|| value.clone());
                    (key.clone(), decoded)
                })
                .collect::<Map<String, Value>>(),
        ),
        Some(other) => other.clone(),
        None => Value::Object(Map::new()),
    }
}

/// One step's visible user or assistant text, as search and the index read it.
pub fn visible_text(line: &str) -> Option<(&'static str, String, Option<String>)> {
    let step = Step::parse(line)?;
    if let Some(text) = step.user_text() {
        return Some(("user", text, step.date()));
    }
    step.assistant_text()
        .map(|text| ("assistant", text, step.date()))
}

/// User prompts and model replies with their tool calls, in order.
pub fn recall_messages(text: &str) -> Vec<RecallMessage> {
    let mut messages = Vec::new();
    for step in text.split('\n').filter_map(Step::parse) {
        if let Some(text) = step.user_text() {
            messages.push(RecallMessage {
                role: "user".into(),
                content: vec![RecallBlock::Text { text }],
            });
            continue;
        }
        if step.kind != "PLANNER_RESPONSE" {
            continue;
        }
        let mut content: Vec<RecallBlock> = step
            .assistant_text()
            .map(|text| RecallBlock::Text { text })
            .into_iter()
            .collect();
        content.extend(
            step.tool_calls
                .into_iter()
                .map(|(name, arguments)| RecallBlock::ToolCall { name, arguments }),
        );
        if !content.is_empty() {
            messages.push(RecallMessage {
                role: "assistant".into(),
                content,
            });
        }
    }
    messages
}

/// The home-compacted workspace of the conversation at `path`, from
/// `conversation_summaries.db` beside the `brain` directory, else from the
/// prompt history in `history.jsonl` (older conversations have no summary workspace).
pub fn project_for_path(path: &str) -> Option<String> {
    let id = conversation_id(path)?;
    let brain = path.get(..path.rfind(id)?)?.trim_end_matches(['/', '\\']);
    let base = brain.get(..brain.rfind(['/', '\\'])?)?;
    static CACHE: OnceLock<Mutex<HashMap<String, HashMap<String, String>>>> = OnceLock::new();
    let mut cache = CACHE.get_or_init(Default::default).lock().ok()?;
    let workspaces = cache
        .entry(base.to_string())
        .or_insert_with(|| read_workspaces(base));
    workspaces
        .get(id)
        .map(|workspace| compact_home(workspace).to_string())
}

fn read_workspaces(base: &str) -> HashMap<String, String> {
    let database = join(&[base, "conversation_summaries.db"]);
    let read = || -> rusqlite::Result<HashMap<String, String>> {
        let connection = rusqlite::Connection::open_with_flags(
            &database,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let mut statement = connection
            .prepare("SELECT conversation_id, workspace_uris FROM conversation_summaries")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        Ok(rows
            .filter_map(Result::ok)
            .filter_map(|(id, uris)| Some((id, first_workspace(&uris)?)))
            .collect())
    };
    let mut workspaces = read().unwrap_or_default();
    if let Ok(history) = std::fs::read_to_string(join(&[base, "history.jsonl"])) {
        for entry in history
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        {
            let field = |key| {
                entry
                    .get(key)
                    .and_then(Value::as_str)
                    .filter(|v| !v.is_empty())
            };
            if let (Some(id), Some(workspace)) = (field("conversationId"), field("workspace")) {
                workspaces
                    .entry(id.to_string())
                    .or_insert_with(|| workspace.to_string());
            }
        }
    }
    workspaces
}

/// The first `file://` URI of a JSON array, as a local path.
fn first_workspace(uris: &str) -> Option<String> {
    let uris: Vec<String> = serde_json::from_str(uris).ok()?;
    let path = uris.first()?.strip_prefix("file://")?;
    let path = percent_decode(path);
    // file:///C:/work is the Windows path C:/work.
    let bytes = path.as_bytes();
    if bytes.len() > 2 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' {
        return Some(path[1..].to_string());
    }
    Some(path)
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && let Some(byte) = text
                .get(index + 1..index + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) fn user(text: &str) -> String {
        json!({ "step_index": 0, "source": "USER_EXPLICIT", "type": "USER_INPUT", "status": "DONE", "created_at": "2026-10-05T10:57:21Z",
            "content": format!("<USER_REQUEST>\n{text}\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nThe current local time is: 2026-10-05T16:27:21+05:30.\n</ADDITIONAL_METADATA>") })
        .to_string()
    }

    #[test]
    fn user_text_is_the_request_and_hook_injections_are_hidden() {
        let step = Step::parse(&user("fix the hook")).unwrap();
        assert_eq!(step.user_text().as_deref(), Some("fix the hook"));
        assert_eq!(step.date().as_deref(), Some("2026-10-05"));
        let hook = json!({ "source": "SYSTEM_SDK", "type": "USER_INPUT", "content": "<USER_REQUEST>\n<project-memory>x</project-memory>\n</USER_REQUEST>" });
        assert!(visible_text(&hook.to_string()).is_none());
        let system = json!({ "source": "SYSTEM", "type": "SYSTEM_MESSAGE", "content": "<SYSTEM_MESSAGE>x</SYSTEM_MESSAGE>" });
        assert!(visible_text(&system.to_string()).is_none());
        assert_eq!(user_request("plain"), "plain");
    }

    #[test]
    fn replies_carry_decoded_tool_calls_and_results_drop_their_header() {
        let reply = json!({ "source": "MODEL", "type": "PLANNER_RESPONSE", "content": "Looking.", "thinking": "hmm",
            "tool_calls": [{ "name": "run_command", "args": { "CommandLine": "\"ls -la\"", "WaitMsBeforeAsync": "5000", "Raw": "not json" } }] });
        let result = json!({ "source": "MODEL", "type": "RUN_COMMAND", "status": "DONE",
            "content": "Created At: 2026-10-05T10:57:22Z\nCompleted At: 2026-10-05T10:57:23Z\n\nThe command completed successfully." });
        let text = [user("list files"), reply.to_string(), result.to_string()].join("\n");
        let messages = recall_messages(&text);
        assert_eq!(messages.len(), 2);
        assert_eq!(
            messages[1].content[1],
            RecallBlock::ToolCall {
                name: "run_command".into(),
                arguments: json!({ "CommandLine": "ls -la", "WaitMsBeforeAsync": 5000, "Raw": "not json" }),
            }
        );
        let step = Step::parse(&result.to_string()).unwrap();
        assert!(step.is_tool_result() && !step.is_error());
        assert_eq!(step.tool_output(), "The command completed successfully.");
        let error = json!({ "source": "SYSTEM", "type": "ERROR_MESSAGE", "status": "DONE", "error": "bad args" });
        let step = Step::parse(&error.to_string()).unwrap();
        assert!(step.is_tool_result() && step.is_error());
        assert_eq!(step.tool_output(), "bad args");
    }

    #[test]
    fn finds_one_transcript_per_conversation_and_its_workspace() {
        let base = std::env::temp_dir().join(format!("dejavu-agy-{}", std::process::id()));
        let base = base.to_string_lossy().into_owned();
        let _ = std::fs::remove_dir_all(&base);
        let brain = join(&[&base, "brain"]);
        for (id, files) in [
            ("aaa", &["transcript.jsonl", "transcript_full.jsonl"][..]),
            ("bbb", &["transcript.jsonl"][..]),
            ("ccc", &[][..]),
        ] {
            let logs = join(&[&brain, id, ".system_generated", "logs"]);
            std::fs::create_dir_all(&logs).unwrap();
            for file in files {
                std::fs::write(join(&[&logs, file]), user("hi")).unwrap();
            }
        }
        let files = transcript_files(&brain).unwrap();
        let full = join(&[
            &brain,
            "aaa",
            ".system_generated",
            "logs",
            "transcript_full.jsonl",
        ]);
        assert_eq!(
            files,
            [
                full.clone(),
                join(&[
                    &brain,
                    "bbb",
                    ".system_generated",
                    "logs",
                    "transcript.jsonl"
                ])
            ]
        );
        assert_eq!(conversation_id(&full), Some("aaa"));
        assert_eq!(conversation_id("/x/aaa/logs/transcript.jsonl"), None);

        let database =
            rusqlite::Connection::open(join(&[&base, "conversation_summaries.db"])).unwrap();
        database
            .execute_batch(
                "CREATE TABLE conversation_summaries (conversation_id text, workspace_uris text NOT NULL);
                 INSERT INTO conversation_summaries VALUES ('aaa', '[\"file:///work/my%20app\"]'), ('bbb', '');",
            )
            .unwrap();
        drop(database);
        std::fs::write(
            join(&[&base, "history.jsonl"]),
            "{\"display\":\"hi\",\"workspace\":\"/work/old\",\"conversationId\":\"bbb\"}\n",
        )
        .unwrap();
        assert_eq!(project_for_path(&full).as_deref(), Some("/work/my app"));
        assert_eq!(project_for_path(&files[1]).as_deref(), Some("/work/old"));
        assert_eq!(
            first_workspace("[\"file:///C:/work\"]").as_deref(),
            Some("C:/work")
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
