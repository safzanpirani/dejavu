use crate::args::{self, Args, die};
use crate::update::{self, DISABLE_CHECK_ENV, UpdateDeps};
use crate::{Common, Outcome, VERSION, js};

pub fn run(mut args: Args, common: Common) -> Outcome {
    args.shift();
    let check_only = args.flag(&["--check"]);
    args.reject_unknown_flags();
    if let Some(extra) = args.first() {
        die(&format!(
            "self-update accepts no positional arguments (unexpected: '{extra}')"
        ));
    }
    let result = update::self_update(check_only, &UpdateDeps::default())?;
    if common.json {
        println!("{}", js::pretty(&result));
    } else if result.updated {
        println!(
            "dejavu {} -> {} installed at {}",
            result.current,
            result.latest,
            result.path.as_deref().unwrap_or("")
        );
    } else if update::compare_versions(&result.latest, &result.current) > 0 {
        println!(
            "dejavu {} is available (installed {}); run `dejavu self-update` to install it",
            result.latest, result.current
        );
    } else {
        println!("dejavu {} is up to date", result.current);
    }
    Ok(0)
}

/// The daily release notice on stderr for people at a terminal; agents and
/// pipes never pay for the lookup.
pub fn print_update_notice(raw: &[String]) {
    if !args::stderr_is_tty()
        || raw.first().is_some_and(|first| first == "self-update")
        || raw
            .iter()
            .any(|arg| ["--json", "-q", "--quiet", "--paths"].contains(&arg.as_str()))
    {
        return;
    }
    if let Some(latest) = update::available_update(&UpdateDeps::default()) {
        eprintln!(
            "{}",
            args::dim_stderr(&format!(
                "dejavu {latest} is available (installed {VERSION}); run `dejavu self-update`, or set {DISABLE_CHECK_ENV}=1 to silence this"
            ))
        );
    }
}
