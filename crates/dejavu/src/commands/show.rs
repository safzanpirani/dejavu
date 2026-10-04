use crate::args::{Args, die, dim_stderr};
use crate::commands::transcript::print_stdout;
use crate::js;
use crate::render::render_show;
use crate::view::{ShowOptions, show_session};
use crate::{Common, Outcome};

pub fn run(mut args: Args, common: Common) -> Outcome {
    args.shift();
    let full = args.flag(&["--full"]);
    let around = args.value(&["--around"]);
    let tools = !args.flag(&["--no-tools", "--no-toolcalls"]);
    let max_chars = args.bound("--max-chars", 1);
    args.reject_unknown_flags();
    let Some(locator) = args.shift() else {
        die("show needs a transcript locator from search results");
    };
    if let Some(extra) = args.first() {
        die(&format!(
            "show accepts one transcript locator (unexpected argument: '{extra}')"
        ));
    }
    let options = ShowOptions {
        full,
        around,
        tools,
        max_chars,
    };
    let result = show_session(&locator, &options)?;
    print_stdout(&if common.json {
        js::pretty(&result)
    } else {
        render_show(&result)
    });
    if !common.quiet && !common.json {
        let count = result.message_count;
        eprintln!(
            "{}",
            dim_stderr(&format!(
                "{} · {count} message{}",
                result.source,
                if count == 1 { "" } else { "s" }
            ))
        );
    }
    Ok(0)
}
