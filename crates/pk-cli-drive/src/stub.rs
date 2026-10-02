//! A stub `rclone` for tests: a shell script in its own temp dir that logs
//! each call's argv and answers from a script body. Tests never call a real
//! rclone.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use crate::rclone::Remote;

pub struct Stub {
    pub dir: tempfile::TempDir,
}

impl Stub {
    /// `body` runs after the argv is logged, with rclone's arguments in
    /// `$@`.
    pub fn new(body: &str) -> Stub {
        let dir = tempfile::tempdir().unwrap();
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"{}\"\n{body}\n",
            dir.path().join("argv").display()
        );
        let path = dir.path().join("rclone");
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        Stub { dir }
    }

    /// A stub backed by a plain directory: `cat`, `copyto` (both ways) and
    /// `lsf` act on `<stub dir>/remote/<path after the colon>`.
    pub fn backed() -> Stub {
        Stub::new(
            r#"root="$(dirname "$0")/remote"
path() { printf '%s/%s' "$root" "${1#*:}"; }
case "$1" in
  cat) [ -f "$(path "$2")" ] || exit 3; cat "$(path "$2")";;
  copyto)
    case "$2" in
      *:*) [ -f "$(path "$2")" ] || exit 3; cp "$(path "$2")" "$3";;
      *) mkdir -p "$(dirname "$(path "$3")")"; cp "$2" "$(path "$3")";;
    esac;;
  lsf) [ -e "$(path "$2")" ] || exit 3;;
  *) exit 1;;
esac"#,
        )
    }

    pub fn remote(&self) -> Remote {
        Remote::new("example:State").program(self.dir.path().join("rclone").display().to_string())
    }

    /// The file the backed stub keeps for `rel`.
    pub fn file(&self, rel: &str) -> PathBuf {
        self.dir.path().join("remote/State").join(rel)
    }

    pub fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.dir.path().join("argv"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}
