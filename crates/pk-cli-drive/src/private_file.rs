//! Owner-only file output for the read cache's copies and the temporary
//! file an rclone upload goes through. The writer is `pk-cli-core`'s
//! [`pk_cli_core::private_file`], the one every crate in the family uses,
//! re-exported here so a CLI that took it from this crate keeps building.

pub use pk_cli_core::private_file::{write, Replace};
