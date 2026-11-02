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

    /// Disable ANSI color. Also honored via $NO_COLOR (any non-empty value).
    #[arg(long, global = true, default_value_t = no_color_env())]
    pub no_color: bool,
}

/// `NO_COLOR` follows <https://no-color.org>: any non-empty value disables
/// color, whatever it says. It is the flag's default rather than clap's
/// `env`, because `env` parses the variable as a boolean, and the
/// conventional `NO_COLOR=1` then fails every command with a usage error.
fn no_color_env() -> bool {
    std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty())
}

impl CommonArgs {
    /// True when interactive prompting is acceptable: stdin is a TTY and the
    /// caller hasn't asked for machine-readable output.
    pub fn interactive(&self) -> bool {
        use std::io::IsTerminal;
        !self.json && std::io::stdin().is_terminal()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser, Debug)]
    struct Probe {
        #[command(flatten)]
        common: CommonArgs,
    }

    #[test]
    fn the_flag_alone_parses_and_defaults_off() {
        // NO_COLOR may be set in the developer's shell; the env path is
        // covered by `example-cli`'s black-box test (`tests/no_color.rs`),
        // which controls the child's environment.
        if std::env::var_os("NO_COLOR").is_none() {
            assert!(!Probe::try_parse_from(["probe"]).unwrap().common.no_color);
        }
        assert!(
            Probe::try_parse_from(["probe", "--no-color"])
                .unwrap()
                .common
                .no_color
        );
    }
}
