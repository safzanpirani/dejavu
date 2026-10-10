use std::path::PathBuf;

use crate::args::{Args, die, dim_stderr};
use crate::codex_client::SignalGuard;
use crate::js;
use crate::query::{self, QueryOptions, RealQuery};
use crate::sources::home_dir;
use crate::{Common, Outcome};

pub fn run(mut args: Args, common: Common) -> Outcome {
    args.shift();
    let agent_dir = args
        .value(&["--agent-dir"])
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(home_dir()).join(".pi").join("agent"));
    let flags = crate::query_config::QueryFlags::parse(&mut args);
    args.reject_unknown_flags();
    let Some(locator) = args.shift() else {
        die("query needs a transcript locator from search results");
    };
    let question = args.items.join(" ");
    let question = question.trim();
    if question.is_empty() {
        die("query needs a question");
    }
    let options = QueryOptions {
        agent_dir: &agent_dir,
        settings: flags.resolve()?,
    };
    let result = {
        let guard = SignalGuard::install();
        query::query_session(
            &crate::last::resolve_locator(&locator)?,
            question,
            &options,
            &guard.cancel(),
            &RealQuery,
        )?
    };
    if common.json {
        println!("{}", js::pretty(&result));
    } else {
        println!("{}", render_query(&result));
    }
    if !common.quiet && !common.json {
        eprintln!("{}", dim_stderr(&query::summary_line(&result)));
    }
    Ok(0)
}

/// `renderQuery` from `render.ts`: the answer alone.
fn render_query(result: &query::QueryResult) -> &str {
    &result.answer
}
