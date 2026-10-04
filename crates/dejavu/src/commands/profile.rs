use crate::args::Args;
use crate::codex_client::SignalGuard;
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
    let flags = crate::query_config::QueryFlags::parse(&mut args);
    let agent_dir = args
        .value(&["--agent-dir"])
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(crate::sources::home_dir()).join(".pi/agent"));
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
        let settings = flags.resolve()?;
        // Preserve the deterministic report if model resolution or execution fails.
        let resolved = crate::query_config::resolve_model(&agent_dir, &settings);
        let model = resolved
            .as_ref()
            .map(|m| {
                if m.provider == "codex" {
                    m.id.clone()
                } else {
                    format!("{}/{}", m.provider, m.id)
                }
            })
            .unwrap_or_else(|_| settings.model.clone());
        let effort = resolved
            .as_ref()
            .ok()
            .and_then(|m| m.reasoning_effort.as_deref());
        report.explanation = Some(profile::explain_profile(
            &report,
            &model,
            effort,
            &guard.cancel(),
            &|_, prompt, cancel| match &resolved {
                Ok(model) => crate::model_client::complete_prompt(model, prompt, cancel, false),
                Err(error) => Err(error.clone()),
            },
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
