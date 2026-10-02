//! The flag helpers `cli.ts` used. Flags may appear anywhere; each helper
//! removes what it consumes, and the remainder are positional arguments.

use std::io::IsTerminal;

pub const RED: &str = "\x1b[31m";
pub const DIM: &str = "\x1b[90m";
pub const BOLD: &str = "\x1b[1m";
pub const RESET: &str = "\x1b[0m";

pub fn dim(text: &str) -> String {
    format!("{DIM}{text}{RESET}")
}

pub fn bold(text: &str) -> String {
    format!("{BOLD}{text}{RESET}")
}

/// Prints `✗ message` in red to stderr and exits 1.
pub fn die(message: &str) -> ! {
    eprintln!("{RED}✗ {message}{RESET}");
    std::process::exit(1)
}

pub fn stdout_is_tty() -> bool {
    std::io::stdout().is_terminal()
}

pub fn stderr_is_tty() -> bool {
    std::io::stderr().is_terminal()
}

/// Command-line arguments with the operands after a bare `--` held apart:
/// those are positional even when they begin with a dash.
#[derive(Debug, Default, Clone)]
pub struct Args {
    pub items: Vec<String>,
    operands: Vec<String>,
}

impl Args {
    pub fn new(mut items: Vec<String>) -> Args {
        let operands = match items.iter().position(|arg| arg == "--") {
            Some(index) => items.split_off(index).into_iter().skip(1).collect(),
            None => Vec::new(),
        };
        Args { items, operands }
    }

    pub fn first(&self) -> Option<&str> {
        self.items.first().map(String::as_str)
    }

    pub fn shift(&mut self) -> Option<String> {
        (!self.items.is_empty()).then(|| self.items.remove(0))
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Removes every occurrence of each name; true when any was present.
    pub fn flag(&mut self, names: &[&str]) -> bool {
        let before = self.items.len();
        self.items.retain(|arg| !names.contains(&arg.as_str()));
        self.items.len() != before
    }

    /// Removes the first `--name value` or `--name=value`. A missing value,
    /// or one that begins with `-`, is a usage error.
    pub fn value(&mut self, names: &[&str]) -> Option<String> {
        for name in names {
            let prefix = format!("{name}=");
            let Some(index) = self
                .items
                .iter()
                .position(|arg| arg == name || arg.starts_with(&prefix))
            else {
                continue;
            };
            let arg = self.items.remove(index);
            if let Some(value) = arg.strip_prefix(&prefix) {
                return Some(value.to_string());
            }
            match self.items.get(index) {
                Some(value) if !value.starts_with('-') => return Some(self.items.remove(index)),
                _ => die(&format!("{name} needs a value")),
            }
        }
        None
    }

    /// Every occurrence of a repeatable value flag, in order.
    pub fn values(&mut self, names: &[&str]) -> Vec<String> {
        let mut values = Vec::new();
        while let Some(value) = self.value(names) {
            values.push(value);
        }
        values
    }

    /// A positive integer flag (`cli.ts` `integer`).
    pub fn integer(&mut self, names: &[&str], flag: &str, fallback: usize) -> usize {
        match self.value(names) {
            None => fallback,
            Some(raw) => match js_integer(&raw) {
                Some(value) if value >= 1 => value as usize,
                _ => die(&format!("{flag} needs an integer >= 1 (got '{raw}')")),
            },
        }
    }

    /// An optional bound of at least `minimum` (`cli.ts` `optionalBound`).
    pub fn bound(&mut self, flag: &str, minimum: i64) -> Option<usize> {
        let raw = self.value(&[flag])?;
        match js_integer(raw.trim()).filter(|_| !raw.trim().is_empty()) {
            Some(value) if value >= minimum => Some(value as usize),
            _ => die(&format!("{flag} needs an integer >= {minimum}")),
        }
    }

    /// Fails on the first remaining flag, then appends the `--` operands.
    pub fn reject_unknown_flags(&mut self) {
        self.reject_unknown(|arg| arg.starts_with('-') && arg != "-");
    }

    pub fn reject_unknown(&mut self, is_flag: impl Fn(&str) -> bool) {
        if let Some(unknown) = self.items.iter().find(|arg| is_flag(arg)) {
            die(&format!("unknown flag: {unknown}"));
        }
        self.items.append(&mut self.operands);
    }
}

/// `Number(value)` restricted to safe integers, as the TypeScript checked it:
/// `"5"`, `"5.0"`, `"1e3"`, and `" 7 "` are integers; `"5.5"` and `""` are not.
pub fn js_integer(raw: &str) -> Option<i64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let parsed: f64 = if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        i64::from_str_radix(hex, 16).ok()? as f64
    } else {
        trimmed.parse().ok()?
    };
    (parsed.is_finite() && parsed.fract() == 0.0 && parsed.abs() <= 9_007_199_254_740_991.0)
        .then_some(parsed as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Args {
        Args::new(list.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn values_flags_and_operands() {
        let mut a = args(&["find", "-n", "3", "--source=codex", "x", "--", "-y"]);
        assert_eq!(a.value(&["-s", "--source"]).as_deref(), Some("codex"));
        assert_eq!(a.integer(&["-n", "--limit"], "--limit", 10), 3);
        a.shift();
        a.reject_unknown_flags();
        assert_eq!(a.items, ["x", "-y"]);
    }

    #[test]
    fn js_integers() {
        assert_eq!(js_integer("5"), Some(5));
        assert_eq!(js_integer("1e3"), Some(1000));
        assert_eq!(js_integer("5.0"), Some(5));
        assert_eq!(js_integer("5.5"), None);
        assert_eq!(js_integer(""), None);
        assert_eq!(js_integer("abc"), None);
    }
}
