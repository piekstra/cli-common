#!/bin/sh
# Stands in for Chrome in pk-cli-browser's launch tests (src/chrome.rs), so
# launch, the debug-port read and teardown are exercised without a browser.
# Records its arguments in the profile, writes DevToolsActivePort the way
# Chrome does, then sleeps until it is stopped. A start page containing
# `exit-early` makes it exit before writing the port, like a Chrome that
# fails to start.
dir=
for arg in "$@"; do
  case "$arg" in
    --user-data-dir=*) dir="${arg#--user-data-dir=}" ;;
    *exit-early*) exit 3 ;;
  esac
done
[ -n "$dir" ] || exit 2
for arg in "$@"; do
  printf '%s\n' "$arg"
done > "$dir/args"
printf '9222\n/devtools/browser/fake-browser-id\n' > "$dir/DevToolsActivePort.tmp"
mv "$dir/DevToolsActivePort.tmp" "$dir/DevToolsActivePort"
exec sleep 600
