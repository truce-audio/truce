//! Top-level command implementations. Each `cmd_*` function corresponds
//! to one entry in the dispatch match in `crate::run`.

pub(crate) mod build;
pub(crate) mod clean;
pub(crate) mod doctor;
pub(crate) mod install;
pub(crate) mod log;
pub(crate) mod new;
pub(crate) mod package;
pub(crate) mod remove;
pub(crate) mod reset_au_aax;
pub(crate) mod run;
pub(crate) mod status;
pub(crate) mod test;
pub(crate) mod validate;
