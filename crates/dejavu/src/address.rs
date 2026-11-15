//! Session addresses: `session://<harness>/<id>[/<part>][#event-<n>]`, one
//! short name for a session that does not depend on where its transcript is
//! stored. `<harness>` is a source name (`claude`, `codex`, `omp`, ...), `<id>`
//! the session id, `<part>` a transcript stored under the session (a subagent),
//! and `#event-<n>` a stable event number, which `transcript` starts from.
//!
//! Search results print one beside each locator, and every command that takes
//! a locator takes an address. Paths and `opencode://`, `openclaw://` and
//! `hermes://` locators keep working.

use crate::pack::is_uuid;
use crate::types::{SourceSelector, TranscriptSource};

const SCHEME: &str = "session://";
const EVENT: &str = "event-";

/// A parsed session address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionAddress {
    pub source: TranscriptSource,
    /// The id as the address spells it (percent-encoded for SQLite sources).
    pub id: String,
    /// A transcript under the session, without `.jsonl`.
    pub part: Option<String>,
    pub event: Option<usize>,
}

/// Whether `value` is a session address (well-formed or not).
pub fn is_address(value: &str) -> bool {
    value.starts_with(SCHEME)
}

/// Parses a session address.
pub fn parse(address: &str) -> Result<SessionAddress, String> {
    let invalid = |why: &str| {
        format!(
            "invalid session address {address}: {why} (session://<harness>/<id>[/<part>][#event-<n>])"
        )
    };
    let rest = address
        .strip_prefix(SCHEME)
        .ok_or_else(|| invalid("it does not start with session://"))?;
    let (rest, fragment) = match rest.split_once('#') {
        Some((rest, fragment)) => (rest, Some(fragment)),
        None => (rest, None),
    };
    let (harness, tail) = rest
        .split_once('/')
        .ok_or_else(|| invalid("it names no session id"))?;
    let source = TranscriptSource::from_name(harness).ok_or_else(|| {
        let names: Vec<&str> = TranscriptSource::ALL.iter().map(|s| s.as_str()).collect();
        invalid(&format!(
            "unknown harness {harness}; one of {}",
            names.join(", ")
        ))
    })?;
    let (id, part) = match tail.split_once('/') {
        Some((id, part)) => (id, Some(part)),
        None => (tail, None),
    };
    if id.is_empty() || part.is_some_and(|part| part.is_empty() || part.ends_with('/')) {
        return Err(invalid("it names no session id"));
    }
    let event = match fragment {
        None => None,
        Some(fragment) => Some(
            fragment
                .strip_prefix(EVENT)
                .and_then(|n| n.parse::<usize>().ok())
                .ok_or_else(|| invalid("the part after # must be event-<n>"))?,
        ),
    };
    Ok(SessionAddress {
        source,
        id: id.to_string(),
        part: part.map(str::to_string),
        event,
    })
}

/// The event an address points at, if `value` is an address that names one.
pub fn event_of(value: &str) -> Option<usize> {
    is_address(value)
        .then(|| parse(value).ok())
        .flatten()
        .and_then(|address| address.event)
}

/// The address of the transcript at `path`, when its path holds a session id.
///
/// SQLite sources carry the id after `#` in their locator. agy names the
/// conversation folder. JSONL sources name the session's file or folder with a
/// UUID (`<id>.jsonl`, `rollout-<time>-<id>.jsonl`, `<time>_<id>/`); a folder
/// starting with `-` is an encoded working directory and is skipped even when
/// that directory's name ends in a UUID.
pub fn address_of(source: TranscriptSource, path: &str) -> Option<String> {
    let name = source.as_str();
    if path.contains("://") {
        let (_, id) = path.rsplit_once('#')?;
        return (!id.is_empty()).then(|| format!("{SCHEME}{name}/{id}"));
    }
    if source == TranscriptSource::Agy {
        return crate::agy::conversation_id(path).map(|id| format!("{SCHEME}{name}/{id}"));
    }
    let parts: Vec<&str> = path.split(['/', '\\']).collect();
    let (at, id) = parts.iter().enumerate().find_map(|(at, part)| {
        let stem = part.strip_suffix(".jsonl").unwrap_or(part);
        let id = stem.get(stem.len().checked_sub(36)?..)?;
        (!part.starts_with('-') && is_uuid(id)).then(|| (at, id.to_ascii_lowercase()))
    })?;
    let under = parts[at + 1..].join("/");
    let under = under.strip_suffix(".jsonl").unwrap_or(&under);
    Some(if under.is_empty() {
        format!("{SCHEME}{name}/{id}")
    } else {
        format!("{SCHEME}{name}/{id}/{under}")
    })
}

/// The locator of the transcript an address names, through the index.
pub fn resolve(address: &str) -> Result<String, String> {
    let parsed = parse(address)?;
    let stores = crate::sources::discover_stores(SourceSelector::Only(parsed.source));
    let index = crate::index::default_index_path();
    crate::index::refresh_transcript_index(&stores, &index, false, crate::DEFAULT_MAX_PARALLEL)?;
    let found = match &parsed.part {
        None => crate::index::path_for_session_id(&parsed.id, &stores, &index)?,
        Some(part) => crate::index::path_for_session_part(&parsed.id, part, &stores, &index)?,
    };
    found.ok_or_else(|| {
        format!(
            "no {} transcript found for {address}; run `dejavu find <terms> --source {}`",
            parsed.source, parsed.source
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "01a13536-3a4c-7319-8488-f95cd1d4b1dc";

    #[test]
    fn every_layout_gets_an_address_that_parses_back() {
        let cases = [
            (
                TranscriptSource::Claude,
                format!("/h/.claude/projects/-Users-x-app/{ID}.jsonl"),
                format!("session://claude/{ID}"),
            ),
            (
                TranscriptSource::Claude,
                format!("/h/.claude/projects/-Users-x-app/{ID}/subagents/agent-a1b2.jsonl"),
                format!("session://claude/{ID}/subagents/agent-a1b2"),
            ),
            (
                TranscriptSource::Codex,
                format!("/h/.codex/sessions/2026/08/29/rollout-2026-08-29T10-00-00-{ID}.jsonl"),
                format!("session://codex/{ID}"),
            ),
            (
                TranscriptSource::Omp,
                format!("/h/.omp/agent/sessions/-Dev-x/2026-10-13T12-21-48-748Z_{ID}.jsonl"),
                format!("session://omp/{ID}"),
            ),
            (
                TranscriptSource::Omp,
                format!(
                    r"C:\h\.omp\agent\sessions\-Dev-x\2026-10-13T12-21-48-748Z_{ID}\Lock Screen.jsonl"
                ),
                format!("session://omp/{ID}/Lock Screen"),
            ),
            (
                TranscriptSource::Droid,
                format!("/h/.factory/sessions/-Users-x/{ID}.jsonl"),
                format!("session://droid/{ID}"),
            ),
            (
                TranscriptSource::Opencode,
                "opencode:///h/opencode.db#ses_0863bbc96ffebFvBM1w03zML4V".to_string(),
                "session://opencode/ses_0863bbc96ffebFvBM1w03zML4V".to_string(),
            ),
            (
                TranscriptSource::Hermes,
                "hermes:///h/state.db#a%20b".to_string(),
                "session://hermes/a%20b".to_string(),
            ),
        ];
        for (source, path, address) in cases {
            assert_eq!(address_of(source, &path).as_deref(), Some(address.as_str()));
            let parsed = parse(&address).unwrap();
            assert_eq!(parsed.source, source);
            assert_eq!(parsed.event, None);
        }
    }

    #[test]
    fn a_working_directory_named_with_a_uuid_is_not_the_session() {
        let other = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        assert_eq!(
            address_of(
                TranscriptSource::Claude,
                &format!("/h/.claude/projects/-private-tmp-{other}/{ID}.jsonl"),
            ),
            Some(format!("session://claude/{ID}"))
        );
        assert_eq!(
            address_of(TranscriptSource::Pi, "/h/.pi/agent/sessions/-x/notes.jsonl"),
            None
        );
    }

    #[test]
    fn parses_parts_and_events_and_rejects_malformed_addresses() {
        assert_eq!(
            parse(&format!("session://omp/{ID}/Lock Screen#event-12")).unwrap(),
            SessionAddress {
                source: TranscriptSource::Omp,
                id: ID.into(),
                part: Some("Lock Screen".into()),
                event: Some(12),
            }
        );
        assert_eq!(event_of(&format!("session://omp/{ID}#event-0")), Some(0));
        assert_eq!(event_of(&format!("session://omp/{ID}")), None);
        assert_eq!(event_of("/a/path#event-3"), None);
        for bad in [
            "session://omp",
            "session://omp/",
            "session://cursor/abc",
            "session://omp/abc/",
            "session://omp/abc#turn-3",
            "session://omp/abc#event-x",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }
}
