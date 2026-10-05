//! Shared transcript types (`transcript-types.ts`). Serialized field order and
//! omitted fields match what `JSON.stringify` produced for the TypeScript objects.

use serde::Serialize;
use serde_json::Value;

/// One of the agents whose transcripts dejavu reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TranscriptSource {
    Claude,
    Codex,
    Pi,
    Opencode,
    Droid,
    Agy,
}

impl TranscriptSource {
    /// Every source, in discovery order.
    pub const ALL: [TranscriptSource; 6] = [
        Self::Claude,
        Self::Codex,
        Self::Pi,
        Self::Opencode,
        Self::Droid,
        Self::Agy,
    ];

    /// The source's name as the CLI and JSON spell it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Pi => "pi",
            Self::Opencode => "opencode",
            Self::Droid => "droid",
            Self::Agy => "agy",
        }
    }

    /// Parses an exact source name (not `all`).
    pub fn from_name(name: &str) -> Option<TranscriptSource> {
        Self::ALL.into_iter().find(|source| source.as_str() == name)
    }
}

impl std::fmt::Display for TranscriptSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `--source`: every source, or one of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SourceSelector {
    #[default]
    All,
    Only(TranscriptSource),
}

impl SourceSelector {
    /// Whether the selector admits `source`.
    pub fn matches(self, source: TranscriptSource) -> bool {
        match self {
            Self::All => true,
            Self::Only(only) => only == source,
        }
    }

    /// `all` or the source name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Only(source) => source.as_str(),
        }
    }
}

impl Serialize for SourceSelector {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// How a store keeps its sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StoreKind {
    /// A directory tree of JSONL transcripts.
    Jsonl,
    /// An OpenCode SQLite database.
    Sqlite,
}

/// A discovered transcript store: a JSONL root directory or an OpenCode database.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct TranscriptStore {
    pub source: TranscriptSource,
    pub kind: StoreKind,
    pub path: String,
}

/// A store that could not be read, reported as `skipped unreadable ...`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StoreDiagnostic {
    pub source: TranscriptSource,
    pub path: String,
    pub error: String,
}

/// A block of a recalled message. Serializes as `{"type":"text","text":..}`,
/// `{"type":"toolCall","name":..,"arguments":..}`, or `{"type":"image"}`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type")]
pub enum RecallBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "toolCall")]
    ToolCall { name: String, arguments: Value },
    #[serde(rename = "image")]
    Image,
}

impl RecallBlock {
    /// The TypeScript `type` tag.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Text { .. } => "text",
            Self::ToolCall { .. } => "toolCall",
            Self::Image => "image",
        }
    }

    /// The text of a text block.
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Text { text } => Some(text),
            _ => None,
        }
    }
}

/// A visible user or assistant message (OpenCode roles pass through as stored).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecallMessage {
    pub role: String,
    pub content: Vec<RecallBlock>,
}

/// One search snippet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TranscriptSnippet {
    pub role: String,
    pub text: String,
}

/// One transcript's search result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StoreSearchMatch {
    pub source: TranscriptSource,
    pub path: String,
    pub count: usize,
    pub date: String,
    pub project: String,
    pub snippets: Vec<TranscriptSnippet>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn serializes_like_json_stringify() {
        let store = TranscriptStore {
            source: TranscriptSource::Opencode,
            kind: StoreKind::Sqlite,
            path: "/x.db".into(),
        };
        assert_eq!(
            serde_json::to_string(&store).unwrap(),
            r#"{"source":"opencode","kind":"sqlite","path":"/x.db"}"#
        );
        let message = RecallMessage {
            role: "assistant".into(),
            content: vec![
                RecallBlock::Text { text: "hi".into() },
                RecallBlock::ToolCall {
                    name: "bash".into(),
                    arguments: json!({"b": 1, "a": 2}),
                },
                RecallBlock::Image,
            ],
        };
        assert_eq!(
            serde_json::to_string(&message).unwrap(),
            r#"{"role":"assistant","content":[{"type":"text","text":"hi"},{"type":"toolCall","name":"bash","arguments":{"b":1,"a":2}},{"type":"image"}]}"#
        );
        assert_eq!(
            serde_json::to_string(&SourceSelector::All).unwrap(),
            r#""all""#
        );
        assert_eq!(
            TranscriptSource::from_name("pi"),
            Some(TranscriptSource::Pi)
        );
        assert_eq!(TranscriptSource::from_name("all"), None);
        assert_eq!(
            TranscriptSource::from_name("droid"),
            Some(TranscriptSource::Droid)
        );
        assert_eq!(
            serde_json::to_string(&TranscriptSource::Droid).unwrap(),
            r#""droid""#
        );
    }
}
