//! JavaScript string and number semantics the TypeScript output depends on.
//! Character limits and budgets in dejavu count UTF-16 code units, because
//! that is what `string.length` and `slice` measure.

use serde::Serialize;

/// `string.length`: UTF-16 code units.
pub fn len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

/// The byte offset where the first `units` UTF-16 units end. A surrogate
/// pair that straddles the limit is left out whole, so the result is always
/// valid UTF-8 (JavaScript would keep a lone high surrogate there).
pub fn byte_offset(text: &str, units: usize) -> usize {
    let mut seen = 0;
    for (offset, ch) in text.char_indices() {
        seen += ch.len_utf16();
        if seen > units {
            return offset;
        }
    }
    text.len()
}

/// `text.slice(start, end)` in UTF-16 units.
pub fn slice(text: &str, start: usize, end: usize) -> &str {
    let from = byte_offset(text, start);
    let to = byte_offset(text, end.max(start));
    &text[from..to]
}

/// `text.slice(0, units)`.
pub fn prefix(text: &str, units: usize) -> &str {
    &text[..byte_offset(text, units)]
}

/// `JSON.stringify(value, null, 2)`.
pub fn pretty<T: Serialize + ?Sized>(value: &T) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "null".into())
}

/// A JSON number as JavaScript prints it: integral values have no `.0`.
pub fn number(value: f64) -> serde_json::Value {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0 {
        serde_json::Value::from(value as i64)
    } else {
        serde_json::Number::from_f64(value)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_lengths_and_slices() {
        assert_eq!(len("héllo"), 5);
        assert_eq!(len("a😀b"), 4);
        assert_eq!(prefix("a😀b", 2), "a");
        assert_eq!(prefix("a😀b", 3), "a😀");
        assert_eq!(slice("abcdef", 2, 4), "cd");
        assert_eq!(number(3.0).to_string(), "3");
        assert_eq!(number(0.25).to_string(), "0.25");
    }
}
