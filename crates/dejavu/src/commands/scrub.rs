use crate::args::{Args, die, dim_stderr};
use crate::commands::transcript::print_stdout;
use crate::js;
use crate::scrub::{ScrubOptions, parse_drop_list, scrub_transcript};
use crate::{Common, Outcome};

pub fn run(mut args: Args, common: Common) -> Outcome {
    args.shift();
    let drop = parse_drop_list(&args.values(&["--drop"]))?;
    let patterns = args.values(&["--pattern"]);
    let placeholder = args.value(&["--placeholder"]);
    let dry_run = args.flag(&["--dry-run"]);
    args.reject_unknown_flags();
    let Some(locator) = args.shift() else {
        die("scrub needs a transcript locator");
    };
    if let Some(extra) = args.first() {
        die(&format!(
            "scrub accepts one transcript locator (unexpected argument: '{extra}')"
        ));
    }
    let pattern_count = patterns.len();
    let result = scrub_transcript(
        &locator,
        &ScrubOptions {
            drop,
            patterns,
            placeholder,
            dry_run,
        },
    )?;
    if common.json {
        print_stdout(&js::pretty(&result));
        return Ok(0);
    }
    let changed = result.changed_records;
    let mut lines = vec![format!(
        "{} {changed} record{} in {} transcript {}",
        if result.dry_run {
            "Would change"
        } else {
            "Changed"
        },
        if changed == 1 { "" } else { "s" },
        result.source,
        result.path
    )];
    if !result.dropped_events.is_empty() {
        let ids: Vec<String> = result
            .dropped_events
            .iter()
            .map(|index| format!("#{index}"))
            .collect();
        lines.push(format!("Redacted events: {}", ids.join(", ")));
    }
    if pattern_count > 0 {
        lines.push(format!("Pattern lines removed: {}", result.pattern_lines));
    }
    if let Some(backup) = &result.backup {
        lines.push(format!("Backup: {backup}"));
    }
    print_stdout(&lines.join("\n"));
    if !common.quiet && !result.dry_run && changed > 0 {
        eprintln!(
            "{}",
            dim_stderr(
                "a running agent that already loaded this session keeps the old content in memory until it restarts"
            )
        );
    }
    Ok(0)
}
