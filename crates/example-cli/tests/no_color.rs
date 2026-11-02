//! `NO_COLOR` (https://no-color.org) is honored for any non-empty value and
//! never fails a command. Black-box, because the variable is read from the
//! process environment, which a unit test cannot set without racing the
//! other tests in its process.

use std::process::Command;

fn example(no_color: Option<&str>) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_example-cli"));
    cmd.args(["devices", "list", "--json"]);
    match no_color {
        Some(v) => cmd.env("NO_COLOR", v),
        None => cmd.env_remove("NO_COLOR"),
    };
    cmd.output().expect("example-cli runs")
}

#[test]
fn every_no_color_value_is_accepted() {
    for v in [
        None,
        Some(""),
        Some("1"),
        Some("true"),
        Some("0"),
        Some("yes"),
    ] {
        let out = example(v);
        assert!(
            out.status.success(),
            "NO_COLOR={v:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
