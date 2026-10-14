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

/// `JSON.stringify(value, null, 2)`, with numbers printed as JavaScript prints them.
pub fn pretty<T: Serialize + ?Sized>(value: &T) -> String {
    write_json(value, JsNumbers(serde_json::ser::PrettyFormatter::new()))
}

/// `JSON.stringify(value)`: compact, with numbers printed as JavaScript prints them.
pub fn stringify<T: Serialize + ?Sized>(value: &T) -> String {
    write_json(value, JsNumbers(serde_json::ser::CompactFormatter))
}

fn write_json<T: Serialize + ?Sized, F: serde_json::ser::Formatter>(
    value: &T,
    formatter: F,
) -> String {
    let mut out = Vec::with_capacity(128);
    let mut serializer = serde_json::Serializer::with_formatter(&mut out, formatter);
    match value.serialize(&mut serializer) {
        // serde_json only writes valid UTF-8.
        Ok(()) => String::from_utf8(out).unwrap_or_default(),
        Err(_) => "null".into(),
    }
}

/// `Number.prototype.toString()` for a finite or non-finite double: `1`, not
/// `1.0`; `1e+21`; `1e-7`; `0.000001`.
pub fn number_to_string(value: f64) -> String {
    if value.is_nan() {
        return "NaN".into();
    }
    if value.is_infinite() {
        return if value > 0.0 {
            "Infinity".into()
        } else {
            "-Infinity".into()
        };
    }
    if value == 0.0 {
        return "0".into();
    }
    let sign = if value < 0.0 { "-" } else { "" };
    // `{:e}` prints the shortest round-trip digits, like JavaScript.
    let scientific = format!("{:e}", value.abs());
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let k = digits.len() as i32;
    let n = exponent.parse::<i32>().unwrap_or(0) + 1;
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let rest = if k > 1 {
            format!(".{}", &digits[1..])
        } else {
            String::new()
        };
        let e = n - 1;
        format!(
            "{}{rest}e{}{}",
            &digits[..1],
            if e >= 0 { "+" } else { "-" },
            e.abs()
        )
    };
    format!("{sign}{body}")
}

const MAX_SAFE: u64 = 1 << 53;

/// A serde_json formatter that prints numbers the way `JSON.stringify` does.
struct JsNumbers<F>(F);

impl<F: serde_json::ser::Formatter> serde_json::ser::Formatter for JsNumbers<F> {
    fn write_f64<W: ?Sized + std::io::Write>(
        &mut self,
        writer: &mut W,
        value: f64,
    ) -> std::io::Result<()> {
        let text = if value.is_finite() {
            number_to_string(value)
        } else {
            "null".into()
        };
        writer.write_all(text.as_bytes())
    }
    fn write_f32<W: ?Sized + std::io::Write>(
        &mut self,
        writer: &mut W,
        value: f32,
    ) -> std::io::Result<()> {
        self.write_f64(writer, f64::from(value))
    }
    fn write_u64<W: ?Sized + std::io::Write>(
        &mut self,
        writer: &mut W,
        value: u64,
    ) -> std::io::Result<()> {
        if value > MAX_SAFE {
            self.write_f64(writer, value as f64)
        } else {
            self.0.write_u64(writer, value)
        }
    }
    fn write_i64<W: ?Sized + std::io::Write>(
        &mut self,
        writer: &mut W,
        value: i64,
    ) -> std::io::Result<()> {
        if value.unsigned_abs() > MAX_SAFE {
            self.write_f64(writer, value as f64)
        } else {
            self.0.write_i64(writer, value)
        }
    }
    fn begin_array<W: ?Sized + std::io::Write>(&mut self, writer: &mut W) -> std::io::Result<()> {
        self.0.begin_array(writer)
    }
    fn end_array<W: ?Sized + std::io::Write>(&mut self, writer: &mut W) -> std::io::Result<()> {
        self.0.end_array(writer)
    }
    fn begin_array_value<W: ?Sized + std::io::Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> std::io::Result<()> {
        self.0.begin_array_value(writer, first)
    }
    fn end_array_value<W: ?Sized + std::io::Write>(
        &mut self,
        writer: &mut W,
    ) -> std::io::Result<()> {
        self.0.end_array_value(writer)
    }
    fn begin_object<W: ?Sized + std::io::Write>(&mut self, writer: &mut W) -> std::io::Result<()> {
        self.0.begin_object(writer)
    }
    fn end_object<W: ?Sized + std::io::Write>(&mut self, writer: &mut W) -> std::io::Result<()> {
        self.0.end_object(writer)
    }
    fn begin_object_key<W: ?Sized + std::io::Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> std::io::Result<()> {
        self.0.begin_object_key(writer, first)
    }
    fn end_object_key<W: ?Sized + std::io::Write>(
        &mut self,
        writer: &mut W,
    ) -> std::io::Result<()> {
        self.0.end_object_key(writer)
    }
    fn begin_object_value<W: ?Sized + std::io::Write>(
        &mut self,
        writer: &mut W,
    ) -> std::io::Result<()> {
        self.0.begin_object_value(writer)
    }
    fn end_object_value<W: ?Sized + std::io::Write>(
        &mut self,
        writer: &mut W,
    ) -> std::io::Result<()> {
        self.0.end_object_value(writer)
    }
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

/// `Date.parse` for ISO 8601 timestamps, in epoch milliseconds. A date-time
/// without an offset reads as UTC (JavaScript reads it as local time; both
/// ends of a latency read the same way, so only a DST change differs).
pub fn parse_iso_ms(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    let number = |from: usize, len: usize| -> Option<i64> {
        let part = bytes.get(from..from + len)?;
        part.iter()
            .all(u8::is_ascii_digit)
            .then(|| std::str::from_utf8(part).ok()?.parse().ok())?
    };
    let (sign, mut at) = match bytes.first()? {
        b'+' => (1, 1),
        b'-' => (-1, 1),
        _ => (1, 0),
    };
    let year_len = if at == 1 { 6 } else { 4 };
    let year = sign * number(at, year_len)?;
    at += year_len;
    let field = |at: &mut usize, sep: u8, min: i64, max: i64| -> Option<Option<i64>> {
        if bytes.get(*at) != Some(&sep) {
            return Some(None);
        }
        let value = number(*at + 1, 2)?;
        if value < min || value > max {
            return None;
        }
        *at += 3;
        Some(Some(value))
    };
    let month = field(&mut at, b'-', 1, 12)?.unwrap_or(1);
    let day = field(&mut at, b'-', 1, 31)?.unwrap_or(1);
    let mut ms = 0;
    if at < bytes.len() {
        if bytes[at] != b'T' && bytes[at] != b't' && bytes[at] != b' ' {
            return None;
        }
        let hour = number(at + 1, 2)?;
        if bytes.get(at + 3) != Some(&b':') {
            return None;
        }
        let minute = number(at + 4, 2)?;
        at += 6;
        let mut second = 0;
        if bytes.get(at) == Some(&b':') {
            second = number(at + 1, 2)?;
            at += 3;
            if bytes.get(at) == Some(&b'.') {
                at += 1;
                let start = at;
                while at < bytes.len() && bytes[at].is_ascii_digit() {
                    at += 1;
                }
                if at == start {
                    return None;
                }
                let digits = &text[start..at.min(start + 3)];
                ms = format!("{digits:0<3}").parse().ok()?;
            }
        }
        if hour > 24 || minute > 59 || second > 59 || (hour == 24 && (minute | second | ms) != 0) {
            return None;
        }
        let mut offset = 0;
        match bytes.get(at) {
            None => {}
            Some(b'Z' | b'z') if at + 1 == bytes.len() => {}
            Some(&sign @ (b'+' | b'-')) => {
                let hours = number(at + 1, 2)?;
                let minutes = match bytes.get(at + 3) {
                    Some(b':') => number(at + 4, 2)?,
                    _ => number(at + 3, 2)?,
                };
                let end = if bytes.get(at + 3) == Some(&b':') {
                    at + 6
                } else {
                    at + 5
                };
                if end != bytes.len() {
                    return None;
                }
                offset = (hours * 60 + minutes) * if sign == b'+' { 1 } else { -1 };
            }
            _ => return None,
        }
        ms += ((hour * 60 + minute - offset) * 60 + second) * 1000;
    }
    Some(days_from_civil(year, month, day) * 86_400_000 + ms)
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_timestamps() {
        assert_eq!(parse_iso_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_iso_ms("2026-09-08T10:00:02.5Z"),
            Some(1_788_861_602_500)
        );
        assert_eq!(
            parse_iso_ms("2026-09-08T12:00:02+02:00"),
            parse_iso_ms("2026-09-08T10:00:02Z")
        );
        assert_eq!(parse_iso_ms("2026-09-08"), Some(1_788_825_600_000));
        assert_eq!(parse_iso_ms("not a date"), None);
        assert_eq!(parse_iso_ms("2026-13-01T00:00:00Z"), None);
    }

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

    #[test]
    fn javascript_number_and_json_formatting() {
        let cases = [
            (1.0, "1"),
            (-1.5, "-1.5"),
            (0.1, "0.1"),
            (123456789.0, "123456789"),
            (1e21, "1e+21"),
            (1.5e21, "1.5e+21"),
            (1e20, "100000000000000000000"),
            (0.000001, "0.000001"),
            (1e-7, "1e-7"),
            (1.25e-7, "1.25e-7"),
            (-0.0, "0"),
            (12345678901234567890.0, "12345678901234567000"),
        ];
        for (value, expected) in cases {
            assert_eq!(number_to_string(value), expected, "{value}");
        }
        let value: serde_json::Value =
            serde_json::from_str(r#"{"b":1.0,"a":[2.50,1e21,12345678901234567890]}"#).unwrap();
        assert_eq!(
            stringify(&value),
            r#"{"b":1,"a":[2.5,1e+21,12345678901234567000]}"#
        );
        assert_eq!(
            pretty(&value),
            "{\n  \"b\": 1,\n  \"a\": [\n    2.5,\n    1e+21,\n    12345678901234567000\n  ]\n}"
        );
        assert_eq!(pretty(&serde_json::json!({})), "{}");
    }
}
