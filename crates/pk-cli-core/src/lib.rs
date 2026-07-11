//! Core surface for the piekstra CLI family (SPEC v1).
//!
//! Provides the shared error/exit-code contract, the text/JSON output
//! renderer, common global flags, and small date/money helpers. See the
//! repository's `DESIGN.md` for the full specification.

pub mod args;
pub mod dates;
pub mod error;
pub mod info;
pub mod money;
pub mod output;

pub use args::CommonArgs;
pub use error::CliError;
pub use money::Money;
