use crate::args::Args;
use crate::{Common, Outcome};

pub fn run(_args: Args, _common: Common) -> Outcome {
    Err("memory is not ported yet".into())
}
