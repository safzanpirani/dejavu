//! Direct file scanning (`search-backend.ts`) without `rg` or `grep`: a
//! case-insensitive literal matcher with ripgrep's counting semantics. A
//! file's count is the number of lines that contain the literal (`rg -i -c -F`),
//! and line lookups stop after a maximum number of matching lines (`rg -m N`).
//! Directory walks skip hidden entries and do not follow symlinks, as ripgrep's
//! and `Bun.Glob`'s defaults did.

use crate::paths::js_lower;
use crate::pool::map_pool;
use std::cmp::Ordering;
use std::fs::File;
use std::io::Read;

/// One file's matching-line count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMatchCount {
    pub path: String,
    pub count: usize,
}

/// A case-insensitive literal. ASCII literals match ASCII case-insensitively
/// over raw bytes; other literals compare JavaScript-lowercased text.
#[derive(Debug, Clone)]
pub struct Literal {
    lowered: String,
    ascii: bool,
}

const CHUNK: usize = 1 << 20;

impl Literal {
    pub fn new(query: &str) -> Literal {
        Literal {
            lowered: js_lower(query).into_owned(),
            ascii: query.is_ascii(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.lowered.is_empty()
    }

    /// Whether `text` contains the literal (`text.toLowerCase().includes(literal)`
    /// for ASCII literals, and for others up to Unicode case-mapping details).
    pub fn matches(&self, text: &[u8]) -> bool {
        if self.ascii {
            find_ascii_ci(text, self.lowered.as_bytes()).is_some()
        } else {
            js_lower(&String::from_utf8_lossy(text)).contains(self.lowered.as_str())
        }
    }

    /// Visits each line of `block` (complete lines, `\n`-terminated except
    /// possibly the last) that contains the literal, until `visit` returns false.
    fn for_each_line(&self, block: &[u8], mut visit: impl FnMut(&[u8]) -> bool) {
        if self.lowered.is_empty() {
            // An empty literal matches every line.
            for line in block.split(|&b| b == b'\n') {
                if !visit(line) {
                    return;
                }
            }
            return;
        }
        if !self.ascii {
            for line in block.split(|&b| b == b'\n') {
                if self.matches(line) && !visit(line) {
                    return;
                }
            }
            return;
        }
        let needle = self.lowered.as_bytes();
        let mut pos = 0;
        while pos < block.len() {
            let Some(found) = find_ascii_ci(&block[pos..], needle) else {
                return;
            };
            let at = pos + found;
            let start = memrchr(b'\n', &block[pos..at]).map_or(pos, |i| pos + i + 1);
            let end = memchr(b'\n', &block[at..]).map_or(block.len(), |i| at + i);
            if !visit(&block[start..end]) {
                return;
            }
            pos = end + 1;
        }
    }
}

/// Reads `path` in chunks and visits each matching line (without its `\n`)
/// until `visit` returns false. A file with a NUL byte is binary: when
/// `skip_binary` is set, scanning stops at the chunk that holds it, as
/// ripgrep's default binary detection does for files found by a directory walk.
pub fn scan_lines(
    path: &str,
    literal: &Literal,
    skip_binary: bool,
    mut visit: impl FnMut(&[u8]) -> bool,
) -> std::io::Result<()> {
    // A SQLite session locator scans its rendered transcript instead of a file.
    let mut file: Box<dyn Read> = if crate::virtual_store::is_virtual_locator(path) {
        let text = crate::virtual_store::render(path)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::NotFound, error))?;
        Box::new(std::io::Cursor::new(text.into_bytes()))
    } else {
        Box::new(File::open(path)?)
    };
    let mut buffer: Vec<u8> = Vec::with_capacity(CHUNK);
    let mut keep_going = true;
    loop {
        let start = buffer.len();
        buffer.resize(start + CHUNK, 0);
        let read = read_full(&mut *file, &mut buffer[start..])?;
        buffer.truncate(start + read);
        let eof = read == 0;
        if skip_binary && memchr(0, &buffer[start..]).is_some() {
            return Ok(());
        }
        let complete = if eof {
            buffer.len()
        } else {
            match memrchr(b'\n', &buffer[start..]) {
                Some(i) => start + i + 1,
                // A line longer than the chunk: read more before scanning.
                None => continue,
            }
        };
        if complete > 0 {
            // Without the final `\n`, so a terminated last line is not followed by an empty one.
            let block = if buffer[complete - 1] == b'\n' {
                &buffer[..complete - 1]
            } else {
                &buffer[..complete]
            };
            literal.for_each_line(block, |line| {
                keep_going = visit(line);
                keep_going
            });
        }
        if eof || !keep_going {
            return Ok(());
        }
        buffer.drain(..complete);
    }
}

fn read_full(file: &mut dyn Read, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

/// `rg -i -c -F query path`: the number of lines that contain the literal.
pub fn count_matching_lines(path: &str, literal: &Literal) -> std::io::Result<usize> {
    let mut count = 0;
    let skip_binary = !path.ends_with(".jsonl") && !crate::virtual_store::is_virtual_locator(path);
    scan_lines(path, literal, skip_binary, |_| {
        count += 1;
        true
    })?;
    Ok(count)
}

/// Visits the first `max` non-empty lines that contain the literal, decoded
/// as UTF-8 (lossy), until `visit` returns false.
pub fn visit_matching_lines(
    query: &str,
    path: &str,
    max: usize,
    visit: &mut dyn FnMut(&str) -> bool,
) -> Result<(), String> {
    let literal = Literal::new(query);
    let mut seen = 0;
    if max == 0 {
        return Ok(());
    }
    scan_lines(path, &literal, false, |line| {
        if line.is_empty() {
            return true;
        }
        seen += 1;
        visit(&String::from_utf8_lossy(line)) && seen < max
    })
    .map_err(|error| crate::reader::fs_error(&error, "open", path))
}

/// `searchMatchingLines(query, filePath, maxMatches)`: the first `max` lines
/// that contain the literal (`rg -i -F -m N`), decoded as UTF-8 (lossy).
pub fn search_matching_lines(query: &str, path: &str, max: usize) -> Result<Vec<String>, String> {
    let literal = Literal::new(query);
    let mut lines = Vec::new();
    if max == 0 {
        return Ok(lines);
    }
    scan_lines(path, &literal, false, |line| {
        // `output.trim().split("\n").filter(Boolean)` drops empty lines.
        if !line.is_empty() {
            lines.push(String::from_utf8_lossy(line).into_owned());
        }
        lines.len() < max
    })
    .map_err(|error| crate::reader::fs_error(&error, "open", path))?;
    Ok(lines)
}

/// Every regular, non-hidden file under `root` (symlinks are not followed).
/// `filter` selects file names. Unreadable subdirectories are skipped; an
/// unreadable root is an error.
pub fn walk_files(root: &str, filter: impl Fn(&str) -> bool) -> Result<Vec<String>, String> {
    let mut files = Vec::new();
    let entries = std::fs::read_dir(root)
        .map_err(|error| crate::reader::fs_error(&error, "scandir", root))?;
    let mut stack = vec![(root.trim_end_matches('/').to_string(), entries)];
    while let Some((dir, mut entries)) = stack.pop() {
        while let Some(entry) = entries.next() {
            let Ok(entry) = entry else { continue };
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with('.') {
                continue;
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = format!("{dir}/{name}");
            if kind.is_dir() {
                if let Ok(children) = std::fs::read_dir(&path) {
                    stack.push((dir, entries));
                    stack.push((path, children));
                    break;
                }
            } else if kind.is_file() && filter(name) {
                files.push(path);
            }
        }
    }
    Ok(files)
}

/// The `**/*.jsonl` transcripts under a store root.
pub fn jsonl_files(root: &str) -> Result<Vec<String>, String> {
    walk_files(root, |name| name.ends_with(".jsonl"))
}

/// `searchFileCounts(query, dir)`: files under `dir` with at least one
/// matching line, scanned on at most `max_parallel` threads.
pub fn search_file_counts(
    query: &str,
    dir: &str,
    max_parallel: usize,
) -> Result<Vec<FileMatchCount>, String> {
    let files = walk_files(dir, |_| true)?;
    let mut counts = count_files(&[Literal::new(query)], &files, max_parallel)?;
    Ok(counts.pop().unwrap_or_default())
}

/// Counts matching lines for each literal in each file, reading every file
/// once. Returns one list per literal, in file order, without zero counts.
/// Unreadable files are skipped, as one bad session must not abort a search.
pub fn count_files(
    literals: &[Literal],
    files: &[String],
    max_parallel: usize,
) -> Result<Vec<Vec<FileMatchCount>>, String> {
    let per_file = map_pool(files, max_parallel, |path, _| {
        literals
            .iter()
            .map(|literal| count_matching_lines(path, literal).unwrap_or(0))
            .collect::<Vec<usize>>()
    })?;
    let mut out: Vec<Vec<FileMatchCount>> = vec![Vec::new(); literals.len()];
    for (path, counts) in files.iter().zip(per_file) {
        for (slot, count) in out.iter_mut().zip(counts) {
            if count > 0 {
                slot.push(FileMatchCount {
                    path: path.clone(),
                    count,
                });
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Byte search
// ---------------------------------------------------------------------------

const LO: u64 = 0x0101_0101_0101_0101;
const HI: u64 = 0x8080_8080_8080_8080;

fn has_zero(word: u64) -> u64 {
    word.wrapping_sub(LO) & !word & HI
}

/// The first index of `byte` in `hay`.
pub fn memchr(byte: u8, hay: &[u8]) -> Option<usize> {
    let pattern = LO.wrapping_mul(u64::from(byte));
    let mut i = 0;
    while i + 8 <= hay.len() {
        let word = u64::from_le_bytes(hay[i..i + 8].try_into().unwrap());
        let hit = has_zero(word ^ pattern);
        if hit != 0 {
            return Some(i + (hit.trailing_zeros() / 8) as usize);
        }
        i += 8;
    }
    hay[i..].iter().position(|&b| b == byte).map(|p| i + p)
}

/// The last index of `byte` in `hay`.
pub fn memrchr(byte: u8, hay: &[u8]) -> Option<usize> {
    let pattern = LO.wrapping_mul(u64::from(byte));
    let mut end = hay.len();
    while end >= 8 {
        let word = u64::from_be_bytes(hay[end - 8..end].try_into().unwrap());
        let hit = has_zero(word ^ pattern);
        if hit != 0 {
            return Some(end - 1 - (hit.trailing_zeros() / 8) as usize);
        }
        end -= 8;
    }
    hay[..end].iter().rposition(|&b| b == byte)
}

/// The first ASCII-case-insensitive occurrence of `needle` (already lowercase) in `hay`.
/// Candidate positions come from comparing the first and last needle bytes,
/// both folded with `| 0x20`, across 32 positions at a time; each candidate is
/// then verified exactly.
pub fn find_ascii_ci(hay: &[u8], needle: &[u8]) -> Option<usize> {
    let n = needle.len();
    if n == 0 {
        return Some(0);
    }
    if hay.len() < n {
        return None;
    }
    let first = needle[0] | 0x20;
    let last = needle[n - 1] | 0x20;
    let verify = |at: usize| hay[at..at + n].eq_ignore_ascii_case(needle);
    let positions = hay.len() - n + 1;
    let mut i = 0;
    while i + 32 <= positions {
        let heads = &hay[i..i + 32];
        let tails = &hay[i + n - 1..i + n - 1 + 32];
        let mut mask: u32 = 0;
        for j in 0..32 {
            let hit = ((heads[j] | 0x20) == first) & ((tails[j] | 0x20) == last);
            mask |= u32::from(hit) << j;
        }
        while mask != 0 {
            let at = i + mask.trailing_zeros() as usize;
            if verify(at) {
                return Some(at);
            }
            mask &= mask - 1;
        }
        i += 32;
    }
    (i..positions).find(|&at| verify(at))
}

// ---------------------------------------------------------------------------
// Ordering
// ---------------------------------------------------------------------------

/// Primary collation weight of an ASCII character in the CLDR root order:
/// whitespace, punctuation and symbols (in CLDR order), digits, then letters
/// case-insensitively. Other characters sort after ASCII by code point.
fn primary(c: char) -> (u8, u32) {
    const PUNCT: &str = "_-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$";
    match c {
        '\t' | '\n' | '\r' | ' ' => (0, c as u32),
        '0'..='9' => (2, c as u32),
        'a'..='z' => (3, c as u32),
        'A'..='Z' => (3, c.to_ascii_lowercase() as u32),
        _ => match PUNCT.find(c) {
            Some(index) => (1, index as u32),
            None => (4, c as u32),
        },
    }
}

/// `a.localeCompare(b)` approximated for paths: CLDR root primary order over
/// ASCII, then lowercase before uppercase, then code points.
pub fn locale_compare(a: &str, b: &str) -> Ordering {
    a.chars()
        .map(primary)
        .cmp(b.chars().map(primary))
        .then_with(|| {
            a.chars()
                .map(|c| c.is_ascii_uppercase())
                .cmp(b.chars().map(|c| c.is_ascii_uppercase()))
        })
        .then_with(|| a.cmp(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!(
            "dejavu-scan-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().into_owned()
    }

    #[test]
    fn finds_ascii_case_insensitively() {
        let hay = b"xx Needle and NEEDLE and needle";
        assert_eq!(find_ascii_ci(hay, b"needle"), Some(3));
        assert_eq!(find_ascii_ci(&hay[4..], b"needle"), Some(10));
        assert_eq!(find_ascii_ci(b"@x", b"@x"), Some(0));
        assert_eq!(find_ascii_ci(b"`x", b"@x"), None);
        let long: Vec<u8> = std::iter::repeat_n(b'a', 100)
            .chain(*b"Deployment")
            .collect();
        assert_eq!(find_ascii_ci(&long, b"deployment"), Some(100));
        assert_eq!(memchr(b'\n', b"abcdefghij\nk"), Some(10));
        assert_eq!(memrchr(b'\n', b"a\nbcdefghijklmnop"), Some(1));
        assert_eq!(memrchr(b'\n', b"abcdefghijklmnop\n"), Some(16));
    }

    #[test]
    fn counts_matching_lines_like_ripgrep() {
        let dir = temp_dir("counts");
        std::fs::create_dir_all(format!("{dir}/nested")).unwrap();
        std::fs::create_dir_all(format!("{dir}/.hidden")).unwrap();
        std::fs::write(
            format!("{dir}/a.jsonl"),
            "Needle needle none\nnone\nNEEDLE\n",
        )
        .unwrap();
        std::fs::write(format!("{dir}/b.jsonl"), "none").unwrap();
        std::fs::write(format!("{dir}/nested/c.txt"), "needle").unwrap();
        std::fs::write(format!("{dir}/nested/d.bin"), b"needle\0").unwrap();
        std::fs::write(format!("{dir}/.hidden/e.jsonl"), "needle").unwrap();
        let mut counts = search_file_counts("needle", &dir, 2).unwrap();
        counts.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(
            counts,
            [
                FileMatchCount {
                    path: format!("{dir}/a.jsonl"),
                    count: 2
                },
                FileMatchCount {
                    path: format!("{dir}/nested/c.txt"),
                    count: 1
                },
            ]
        );
        assert_eq!(
            search_matching_lines("NEEDLE", &format!("{dir}/a.jsonl"), 5).unwrap(),
            ["Needle needle none", "NEEDLE"]
        );
        assert_eq!(
            search_matching_lines("needle", &format!("{dir}/a.jsonl"), 1).unwrap(),
            ["Needle needle none"]
        );
        let mut jsonl = jsonl_files(&dir).unwrap();
        jsonl.sort();
        assert_eq!(jsonl, [format!("{dir}/a.jsonl"), format!("{dir}/b.jsonl")]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn scans_lines_across_chunks_and_non_ascii_literals() {
        let dir = temp_dir("chunks");
        let path = format!("{dir}/big.jsonl");
        let long_line = format!("{}Ünïcode tail", "x".repeat(CHUNK + 10));
        std::fs::write(&path, format!("first ünïcode\n{long_line}\nlast\n")).unwrap();
        let lines = search_matching_lines("ÜNÏCODE", &path, 5).unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "first ünïcode");
        assert_eq!(lines[1], long_line);
        assert_eq!(
            count_matching_lines(&path, &Literal::new("tail")).unwrap(),
            1
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn orders_paths_like_locale_compare() {
        assert_eq!(locale_compare("a", "B"), Ordering::Less);
        assert_eq!(locale_compare("a", "A"), Ordering::Less);
        assert_eq!(locale_compare("a_b", "a-b"), Ordering::Less);
        assert_eq!(locale_compare("a1", "aa"), Ordering::Less);
        assert_eq!(locale_compare("x", "x"), Ordering::Equal);
    }
}
