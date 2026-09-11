//! Role wiring lives with the standalone exit gate (Task 12). Every parsed
//! action currently reports a structured not-yet-implemented diagnostic.

use std::convert::Infallible;

use crate::grammar::Command;
use crate::output::{ExitCode, StructuredError};

pub const NOT_IMPLEMENTED_EXIT: ExitCode = ExitCode::UNSUPPORTED;

pub fn dispatch(command: &Command) -> Result<Infallible, StructuredError> {
    Err(StructuredError::not_yet_implemented(&command.label()))
}