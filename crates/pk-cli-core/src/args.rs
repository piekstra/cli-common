//! The global flags every family CLI carries (SPEC v1 §1.1), as a clap
//! `#[command(flatten)]`-able struct. Identity flags (`--account`,
//! `--username`, …) stay per-CLI since their env vars and semantics differ.

use clap::Args;

#[derive(Args, Debug, Default, Clone)]
pub struct CommonArgs {
    /// Emit machine-readable JSON on stdout (diagnostics go to stderr).
    #[arg(long, global = true)]
    pub json: bool,

    /// Extra diagnostics on stderr (never secrets).
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// Suppress non-error stderr output.
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Disable ANSI color. Also honored via $NO_COLOR.
    #[arg(long, global = true, env = "NO_COLOR")]
    pub no_color: bool,
}

impl CommonArgs {
    /// True when interactive prompting is acceptable: stdin is a TTY and the
    /// caller hasn't asked for machine-readable output.
    pub fn interactive(&self) -> bool {
        use std::io::IsTerminal;
        !self.json && std::io::stdin().is_terminal()
    }
}
