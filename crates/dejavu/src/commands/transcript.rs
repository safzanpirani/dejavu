use crate::args::{Args, die, dim, stdout_is_tty};
use crate::js;
use crate::render::{RenderTranscriptOptions, render_transcript};
use crate::view::{TranscriptViewOptions, view_transcript};
use crate::window::{WindowOptions, render_window, window_transcript};
use crate::{Common, Outcome};
use std::io::Write;

/// `console.log(text)`. A closed pipe (`dejavu transcript ... | head`) ends
/// the output quietly instead of panicking.
pub(crate) fn print_stdout(text: &str) {
    let mut stdout = std::io::stdout().lock();
    let written = stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.write_all(b"\n"))
        .and_then(|()| stdout.flush());
    if let Err(error) = written
        && error.kind() == std::io::ErrorKind::BrokenPipe
    {
        std::process::exit(0);
    }
}

pub fn run(mut args: Args, common: Common) -> Outcome {
    args.shift();
    let full = args.flag(&["--full"]);
    let thinking = args.flag(&["--thinking"]);
    let tools = !args.flag(&["--no-tools", "--no-toolcalls"]);
    let max_chars = args.bound("--max-chars", 1);
    let tool_chars = args.bound("--tool-chars", 1);
    let budget_chars = args.bound("--budget-chars", 1);
    let from_event = args.bound("--from-event", 0);
    let limit = args.bound("--limit", 1);
    let clipping = max_chars.is_some() || tool_chars.is_some() || budget_chars.is_some();
    if full && clipping {
        die("--full cannot be combined with character limits");
    }
    let force_color = args.flag(&["--color"]);
    let no_color = args.flag(&["--no-color"]);
    args.reject_unknown_flags();
    let Some(locator) = args.shift() else {
        die("transcript needs a transcript locator from search results");
    };
    if let Some(extra) = args.first() {
        die(&format!(
            "transcript accepts one transcript locator (unexpected argument: '{extra}')"
        ));
    }
    let view = view_transcript(&locator, TranscriptViewOptions { thinking, tools })?;
    let counts = view.counts;
    let source = view.source;
    let color = force_color
        || (!no_color
            && !common.json
            && stdout_is_tty()
            && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty()));
    let render = RenderTranscriptOptions {
        full: full || clipping,
        color,
    };
    let bounded = clipping || from_event.is_some() || limit.is_some();
    let output = if bounded {
        let default_cap = |cap: usize| (!common.json && clipping).then_some(cap);
        let windowed = window_transcript(
            view,
            &WindowOptions {
                from_event,
                limit,
                budget_chars,
                max_chars: max_chars.or(default_cap(1200)),
                tool_chars: tool_chars.or(default_cap(600)),
                focus_terms: Vec::new(),
            },
        )?;
        if common.json {
            js::pretty(&windowed)
        } else {
            format!(
                "{}\n\n{}",
                render_transcript(&windowed.view, render),
                render_window(&windowed.window)
            )
        }
    } else if common.json {
        js::pretty(&view)
    } else {
        render_transcript(&view, render)
    };
    print_stdout(&output);
    if !common.quiet && !common.json {
        eprintln!(
            "{}",
            dim(&format!(
                "{source} · {} user · {} assistant · {} tool calls · {} results · {} thinking",
                counts.user,
                counts.assistant,
                counts.tool_calls,
                counts.tool_results,
                counts.thinking
            ))
        );
    }
    Ok(0)
}
