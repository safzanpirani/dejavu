use crate::args::Args;
use crate::codex_client::{SignalGuard, complete_via_codex};
use crate::js;
use crate::profile::{self, ProfileOptions, RealProfile};
use crate::{Common, Outcome};

pub fn run(mut args: Args, common: Common) -> Outcome {
    args.shift();
    let project = args.value(&["--project"]);
    let since = args.value(&["--since"]);
    let limit = args.integer(&["--limit"], "--limit", profile::DEFAULT_PROFILE_LIMIT);
    let threshold = args.integer(
        &["--output-threshold"],
        "--output-threshold",
        profile::DEFAULT_OUTPUT_THRESHOLD,
    );
    let explain = args.flag(&["--explain"]);
    args.reject_unknown_flags();
    let options = ProfileOptions {
        project: project.as_deref(),
        since: since.as_deref(),
        limit,
        threshold,
    };
    let mut report = profile::profile_sessions(&args.items, &options, &RealProfile)?;
    if explain && !report.sessions.is_empty() {
        let guard = SignalGuard::install();
        report.explanation = Some(profile::explain_profile(
            &report,
            &guard.cancel(),
            &complete_via_codex,
        ));
    }
    if common.json {
        println!("{}", js::pretty(&report));
    } else {
        println!("{}", profile::render_profile(&report));
    }
    let failed = !report.diagnostics.is_empty()
        || report
            .explanation
            .as_ref()
            .is_some_and(|explanation| explanation.error.is_some());
    Ok(i32::from(failed))
}
