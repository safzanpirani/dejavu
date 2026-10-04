use serde_json::Value;
use std::collections::BTreeSet;

/// Compare documented JSON keys against actual serialized output. Reject both
/// undocumented fields and required fields that the command omits.
pub fn assert_shape(help: &str, value: &Value, variant: usize) {
    let line = help
        .lines()
        .filter(|line| line.contains("JSON object keys:") || line.contains("JSON array item keys:"))
        .nth(variant)
        .unwrap();
    let keys = line.split_once("keys: ").unwrap().1;
    let required: BTreeSet<_> = keys.split(", ").filter(|k| !k.ends_with('?')).collect();
    let allowed: BTreeSet<_> = keys.split(", ").map(|k| k.trim_end_matches('?')).collect();
    let check = |object: &Value| {
        let actual: BTreeSet<_> = object
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert!(
            required.is_subset(&actual),
            "Missing keys: {:?}; help: {line}",
            required.difference(&actual).collect::<Vec<_>>()
        );
        assert!(
            actual.is_subset(&allowed),
            "Undocumented keys: {:?}; help: {line}",
            actual.difference(&allowed).collect::<Vec<_>>()
        );
    };
    if line.contains("array item") {
        let items = value.as_array().unwrap();
        assert!(!items.is_empty(), "Array fixture must exercise item fields");
        for item in items {
            check(item);
        }
    } else {
        check(value);
    }
}
