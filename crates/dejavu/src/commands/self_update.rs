use crate::args::Args;
use crate::{Common, Outcome};

pub fn run(_args: Args, _common: Common) -> Outcome {
    Err("self-update is not ported yet".into())
}

/// The daily release notice on stderr for people at a terminal.
pub fn print_update_notice(_raw: &[String]) {}
