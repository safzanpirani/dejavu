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
    let locator = crate::last::resolve_locator(&locator)?;
    let result = show_session(&locator, &options)?;
    let address = crate::address::address_of(result.source, &result.path);
    print_stdout(&if common.json {
        js::pretty(&crate::address::json_with_address(
            &result,
            result.source,
            &result.path,
        ))
    } else {
        render_show(&result)
    });
    if !common.quiet && !common.json {
        let count = result.message_count;
        let address = address.map(|a| format!(" · {a}")).unwrap_or_default();
        eprintln!(
            "{}",
            dim_stderr(&format!(
                "{} · {count} message{}{address}",
                result.source,
                if count == 1 { "" } else { "s" }
            ))
        );
    }
    Ok(0)
}
