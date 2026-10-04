use crate::args::{Args, die, stdout_is_tty};
use crate::commands::search::report_skipped_stores;
use crate::commands::transcript::print_stdout;
use crate::index::ProjectFilter;
use crate::last::{LastOptions, RealLast, Selector, last_session, render_last};
use crate::sources::parse_source;
use crate::{Common, Outcome, js};

pub fn run(mut args: Args, common: Common) -> Outcome {
    crate::args::configure_stderr(&args);
    args.shift();
    let defaults = LastOptions::default();
    let source = parse_source(
        &args
            .value(&["-s", "--source"])
            .unwrap_or_else(|| "all".into()),
    )?;
    let project = args.value(&["-p", "--project"]);
    let anywhere = args.flag(&["--anywhere"]);
    let since = args.value(&["--since"]);
    let list = args.flag(&["--list"]);
    let limit = args.integer(&["-n", "--limit"], "--limit", defaults.limit);
    let tools = args.flag(&["--tools"]);
    let turns = args.integer(&["--turns"], "--turns", defaults.turns);
    let budget_chars = args.integer(&["--budget-chars"], "--budget-chars", defaults.budget_chars);
    let max_chars = args.integer(&["--max-chars"], "--max-chars", defaults.max_chars);
    let tool_chars = args.integer(&["--tool-chars"], "--tool-chars", defaults.tool_chars);
    let exclude_sessions = args.values(&["--exclude-session"]);
    let force_color = args.flag(&["--color"]);
    let no_color = args.flag(&["--no-color"]);
    args.reject_unknown_flags();
    if project.is_some() && anywhere {
        die("use --project or --anywhere, not both");
    }
    if args.items.iter().any(|term| term.trim().is_empty()) {
        die("last needs nonempty terms");
    }
    let selector = match args.items.as_slice() {
        [] => Selector::Recent(match (project, anywhere) {
            (Some(project), _) => ProjectFilter::Contains(project),
            (None, true) => ProjectFilter::Any,
            (None, false) => ProjectFilter::Under(current_project()),
        }),
        [one] if is_locator(one) => Selector::Locator(one.clone()),
        terms => Selector::Terms {
            terms: terms.to_vec(),
            widen: project.is_none() && !anywhere,
            project: match (project, anywhere) {
                (Some(project), _) => Some(project),
                (None, true) => None,
                (None, false) => Some(compact_project(&current_project())),
            },
        },
    };
    let options = LastOptions {
        selector,
        source,
        since,
        list,
        limit,
        tools,
        turns,
        budget_chars,
        max_chars,
        tool_chars,
        exclude_sessions,
    };
    let result = last_session(&options, &RealLast)?;
    print_stdout(&if common.json {
        js::pretty(&result)
    } else {
        let color = force_color
            || (!no_color
                && stdout_is_tty()
                && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty()));
        render_last(&result, color)
    });
    report_skipped_stores(&result.skipped_stores, common.quiet);
    Ok(if result.sessions.is_empty() { 1 } else { 0 })
}

/// A transcript path, an OpenCode locator, or a bare session id.
fn is_locator(value: &str) -> bool {
    value.starts_with("opencode://")
        || crate::pack::is_uuid(value)
        || (value.contains('/') && std::path::Path::new(value).is_file())
}

/// `find`'s project filter matches home-compacted paths without `~/`.
fn compact_project(path: &str) -> String {
    let compacted = crate::paths::compact_home(path);
    compacted
        .strip_prefix("~/")
        .unwrap_or(compacted)
        .to_string()
}

/// The Git work tree holding the current directory, else the directory itself.
fn current_project() -> String {
    let toplevel = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty());
    toplevel.unwrap_or_else(|| {
        std::env::current_dir()
            .map(|dir| dir.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "/".into())
    })
}
