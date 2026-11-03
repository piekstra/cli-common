#!/bin/sh
# Stands in for Chrome in pk-cli-browser's launch tests (src/chrome.rs), so
# launch, the debug-port read and teardown are exercised without a browser.
# Records its arguments in the profile, writes DevToolsActivePort the way
# Chrome does, then sleeps until it is stopped. A start page containing
# `exit-early` makes it exit before writing the port, like a Chrome that
# fails to start. One containing `close-port-<port>.` makes it advertise that
# port and run until `close-now` appears in the profile, like a Chrome asked to
# quit over its debug port, leaving `closed-over-port` there. SIGTERM ends it
# instead, leaving `stopped-by-signal`.
dir=
close_port=
for arg in "$@"; do
  case "$arg" in
    --user-data-dir=*) dir="${arg#--user-data-dir=}" ;;
    *exit-early*) exit 3 ;;
    *close-port-*)
      close_port="${arg#*close-port-}"
      close_port="${close_port%%.*}"
      ;;
  esac
done
[ -n "$dir" ] || exit 2
for arg in "$@"; do
  printf '%s\n' "$arg"
done > "$dir/args"
# Before the port is advertised, so the first signal finds the trap in place.
[ -z "$close_port" ] || trap 'touch "$dir/stopped-by-signal"; exit 0' TERM
printf '%s\n/devtools/browser/fake-browser-id\n' "${close_port:-9222}" > "$dir/DevToolsActivePort.tmp"
mv "$dir/DevToolsActivePort.tmp" "$dir/DevToolsActivePort"
[ -n "$close_port" ] || exec sleep 600
while [ ! -e "$dir/close-now" ]; do
  sleep 0.02
done
touch "$dir/closed-over-port"
