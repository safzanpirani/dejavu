//! A small terminal Markdown renderer for colored output. It styles headings,
//! emphasis, inline code, fenced code blocks, lists, task boxes, quotes, rules,
//! links, and tables with ANSI codes. Each style ends with its own "off" code
//! rather than a full reset, so styles nest inside one another and inside
//! search-term highlights. Callers use it only when color is on; plain output
//! never passes through here.

const BOLD: (&str, &str) = ("1", "22");
const ITALIC: (&str, &str) = ("3", "23");
const UNDERLINE: (&str, &str) = ("4", "24");
const STRIKE: (&str, &str) = ("9", "29");
const CODE: (&str, &str) = ("38;5;180", "39");
const DIM: (&str, &str) = ("90", "39");
const ACCENT: (&str, &str) = ("35", "39");
const H1: (&str, &str) = ("1;4;35", "22;24;39");
const H2: (&str, &str) = ("1;35", "22;39");
const H3: (&str, &str) = ("1", "22");

fn style((on, off): (&str, &str), text: &str) -> String {
    format!("\x1b[{on}m{text}\x1b[{off}m")
}

/// Renders a Markdown document, line by line.
pub fn render(text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut fence: Option<&str> = None;
    for line in text.split('\n') {
        let trimmed = line.trim_start();
        let indent = &line[..line.len() - trimmed.len()];
        if let Some(marker) = fence {
            if trimmed.starts_with(marker) && trimmed[marker.len()..].trim().is_empty() {
                out.push(style(DIM, "╰─"));
                fence = None;
            } else {
                out.push(format!("{} {}", style(DIM, "│"), style(CODE, line)));
            }
            continue;
        }
        if let Some(marker) = ["```", "~~~"].into_iter().find(|m| trimmed.starts_with(m)) {
            let lang = trimmed[marker.len()..]
                .trim_start_matches(['`', '~'])
                .trim();
            fence = Some(marker);
            out.push(style(
                DIM,
                &if lang.is_empty() {
                    "╭─".to_string()
                } else {
                    format!("╭─ {lang}")
                },
            ));
            continue;
        }
        out.push(format!("{indent}{}", block(trimmed)));
    }
    out.join("\n")
}

/// One non-code line with its leading whitespace removed.
fn block(line: &str) -> String {
    let hashes = line.bytes().take_while(|&b| b == b'#').count();
    if (1..=6).contains(&hashes) && line[hashes..].starts_with(' ') {
        let text = inline(line[hashes..].trim());
        return match hashes {
            1 => style(H1, &text),
            2 => style(H2, &text),
            _ => style(H3, &text),
        };
    }
    if is_rule(line) {
        return style(DIM, &"─".repeat(40));
    }
    if let Some(rest) = line.strip_prefix('>') {
        return format!(
            "{} {}",
            style(DIM, "│"),
            style(ITALIC, &inline(rest.trim_start()))
        );
    }
    for bullet in ["- ", "* ", "+ "] {
        if let Some(rest) = line.strip_prefix(bullet) {
            let (mark, rest) = if let Some(rest) = rest.strip_prefix("[ ] ") {
                ("☐", rest)
            } else if let Some(rest) = rest
                .strip_prefix("[x] ")
                .or_else(|| rest.strip_prefix("[X] "))
            {
                ("☑", rest)
            } else {
                ("•", rest)
            };
            return format!("{} {}", style(ACCENT, mark), inline(rest));
        }
    }
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    if (1..=9).contains(&digits)
        && (line[digits..].starts_with(". ") || line[digits..].starts_with(") "))
    {
        return format!(
            "{} {}",
            style(ACCENT, &line[..digits + 1]),
            inline(&line[digits + 2..])
        );
    }
    if line.starts_with('|') {
        if line.chars().all(|c| matches!(c, '|' | '-' | ':' | ' ')) {
            return style(DIM, line);
        }
        return line
            .split('|')
            .map(inline)
            .collect::<Vec<_>>()
            .join(&style(DIM, "│"));
    }
    inline(line)
}

fn is_rule(line: &str) -> bool {
    let compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    compact.len() >= 3
        && ['-', '*', '_']
            .iter()
            .any(|&c| compact.chars().all(|x| x == c))
}

/// Inline spans: code, bold, italic, strikethrough, and links. Existing ANSI
/// escapes (search-term highlights) pass through untouched.
pub fn inline(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\x1b' {
            while i < chars.len() {
                out.push(chars[i]);
                i += 1;
                if chars[i - 1] == 'm' {
                    break;
                }
            }
            continue;
        }
        if c == '`' {
            let run = chars[i..].iter().take_while(|&&x| x == '`').count();
            if let Some(end) = find_run(&chars, i + run, '`', run) {
                let inner: String = chars[i + run..end].iter().collect();
                out.push_str(&style(CODE, inner.trim()));
                i = end + run;
            } else {
                chars[i..i + run].iter().for_each(|&x| out.push(x));
                i += run;
            }
            continue;
        }
        if let Some((span, next)) = delimited(&chars, i, "**", BOLD)
            .or_else(|| delimited(&chars, i, "~~", STRIKE))
            .or_else(|| italic(&chars, i))
            .or_else(|| link(&chars, i))
        {
            out.push_str(&span);
            i = next;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// The index of the next run of exactly `len` copies of `mark` at or after `from`.
fn find_run(chars: &[char], from: usize, mark: char, len: usize) -> Option<usize> {
    let mut j = from;
    while j < chars.len() {
        if chars[j] == mark {
            let run = chars[j..].iter().take_while(|&&x| x == mark).count();
            if run == len {
                return Some(j);
            }
            j += run;
        } else {
            j += 1;
        }
    }
    None
}

fn starts_with(chars: &[char], at: usize, pattern: &str) -> bool {
    let mut rest = chars.get(at..).unwrap_or_default().iter();
    pattern.chars().all(|p| rest.next() == Some(&p))
}

/// `**bold**` or `~~strike~~`: no whitespace just inside the delimiters.
fn delimited(
    chars: &[char],
    at: usize,
    mark: &str,
    codes: (&str, &str),
) -> Option<(String, usize)> {
    let width = mark.chars().count();
    if !starts_with(chars, at, mark) {
        return None;
    }
    let start = at + width;
    if chars.get(start).is_none_or(|c| c.is_whitespace()) {
        return None;
    }
    let end = (start + 1..chars.len())
        .find(|&j| starts_with(chars, j, mark) && !chars[j - 1].is_whitespace())?;
    let inner: String = chars[start..end].iter().collect();
    Some((style(codes, &inline(&inner)), end + width))
}

/// `*italic*`, not after a word character (so `a*b*c` stays literal).
fn italic(chars: &[char], at: usize) -> Option<(String, usize)> {
    if chars[at] != '*' || chars.get(at + 1) == Some(&'*') {
        return None;
    }
    if at > 0 && chars[at - 1].is_alphanumeric() {
        return None;
    }
    if chars.get(at + 1).is_none_or(|c| c.is_whitespace()) {
        return None;
    }
    let end = (at + 2..chars.len()).find(|&j| {
        chars[j] == '*' && !chars[j - 1].is_whitespace() && chars.get(j + 1) != Some(&'*')
    })?;
    let inner: String = chars[at + 1..end].iter().collect();
    Some((style(ITALIC, &inline(&inner)), end + 1))
}

/// `[text](url)`: underlined text, then the URL dimmed unless it repeats the text.
fn link(chars: &[char], at: usize) -> Option<(String, usize)> {
    if chars[at] != '[' || (at > 0 && chars[at - 1] == '\x1b') {
        return None;
    }
    let close = (at + 1..chars.len()).find(|&j| chars[j] == ']' || chars[j] == '\n')?;
    if chars[close] != ']' || chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let end = (close + 2..chars.len()).find(|&j| chars[j] == ')' || chars[j].is_whitespace())?;
    if chars[end] != ')' {
        return None;
    }
    let text: String = chars[at + 1..close].iter().collect();
    let url: String = chars[close + 2..end].iter().collect();
    let shown = style(UNDERLINE, &inline(&text));
    let span = if text == url {
        shown
    } else {
        format!("{shown} {}", style(DIM, &format!("({url})")))
    };
    Some((span, end + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                chars.by_ref().find(|&c| c == 'm');
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn inline_spans() {
        assert_eq!(inline("a **b** c"), "a \x1b[1mb\x1b[22m c");
        assert_eq!(
            inline("run `ls -la` now"),
            "run \x1b[38;5;180mls -la\x1b[39m now"
        );
        assert_eq!(strip(&inline("*it* and ~~gone~~")), "it and gone");
        assert_eq!(inline("a*b*c and 2 * 3 * 4"), "a*b*c and 2 * 3 * 4");
        assert_eq!(inline("snake_case_name"), "snake_case_name");
        assert_eq!(
            inline("`**not bold**`"),
            "\x1b[38;5;180m**not bold**\x1b[39m"
        );
        assert_eq!(
            inline("unclosed `tick and **star"),
            "unclosed `tick and **star"
        );
        assert_eq!(
            strip(&inline("[docs](https://x.dev) and [https://y](https://y)")),
            "docs (https://x.dev) and https://y"
        );
    }

    #[test]
    fn highlights_pass_through() {
        let highlighted = "**see \x1b[1;33mgay\x1b[22;39m here**";
        assert_eq!(
            inline(highlighted),
            "\x1b[1msee \x1b[1;33mgay\x1b[22;39m here\x1b[22m"
        );
    }

    #[test]
    fn blocks() {
        let text = "# Title\n**Verified:**\n\n- one `x`\n  * nested\n- [x] done\n3. third\n> quoted\n---\n```rust\nlet a = *b*;\n```\n| a | b |\n|---|---|";
        assert_eq!(
            strip(&render(text)),
            "Title\nVerified:\n\n• one x\n  • nested\n☑ done\n3. third\n│ quoted\n────────────────────────────────────────\n╭─ rust\n│ let a = *b*;\n╰─\n│ a │ b │\n|---|---|"
        );
    }
}
