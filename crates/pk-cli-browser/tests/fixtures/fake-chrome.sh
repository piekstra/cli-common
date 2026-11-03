#!/bin/sh
# Stands in for Chrome in pk-cli-browser's launch tests (src/chrome.rs), so
# launch, the debug-port read and teardown are exercised without a browser.
# Records its arguments in the profile, and in `default-at-launch` what its
# Default directory holds as it starts, writes DevToolsActivePort the way
# Chrome does, then sleeps until it is stopped. A start page containing
# `exit-early` makes it exit before writing the port, like a Chrome that
# fails to start. One containing `close-port-<port>.` makes it advertise that
# port and run until `close-now` appears in the profile, like a Chrome asked to
# quit over its debug port, leaving `closed-over-port` there. SIGTERM ends it
# instead, leaving `stopped-by-signal`. One containing `flush-on-stop` makes it
# write to its site storage on either way out, like a Chrome flushing as it
# shuts down, leaving `flushed-on-stop` beside it as the proof. One containing
# `leave-holder-<pid>.` makes it name that pid in SingletonLock and, without a
# close port, exit once the port is written, like a wrapper that leaves the
# browser running.
dir=
close_port=
flush=
holder=
for arg in "$@"; do
  case "$arg" in
    --user-data-dir=*) dir="${arg#--user-data-dir=}" ;;
    *exit-early*) exit 3 ;;
    *close-port-*)
      close_port="${arg#*close-port-}"
      close_port="${close_port%%.*}"
      ;;
    *flush-on-stop*) flush=1 ;;
    *leave-holder-*)
      holder="${arg#*leave-holder-}"
      holder="${holder%%.*}"
      ;;
  esac
done
[ -n "$dir" ] || exit 2
for arg in "$@"; do
  printf '%s\n' "$arg"
done > "$dir/args"
ls -A "$dir/Default" > "$dir/default-at-launch" 2>/dev/null
[ -z "$holder" ] || ln -s "fake-host-$holder" "$dir/SingletonLock"
flush_storage() {
  [ -n "$flush" ] || return 0
  mkdir -p "$dir/Default/Local Storage/leveldb"
  printf dummy-refresh-token > "$dir/Default/Local Storage/leveldb/000003.log"
  : > "$dir/flushed-on-stop"
}
# Before the port is advertised, so the first signal finds the trap in place.
[ -z "$close_port$flush" ] || trap 'flush_storage; : > "$dir/stopped-by-signal"; exit 0' TERM
printf '%s\n/devtools/browser/fake-browser-id\n' "${close_port:-9222}" > "$dir/DevToolsActivePort.tmp"
mv "$dir/DevToolsActivePort.tmp" "$dir/DevToolsActivePort"
[ -z "$holder" ] || [ -n "$close_port" ] || exit 0
[ -n "$close_port$flush" ] || exec sleep 600
while [ ! -e "$dir/close-now" ]; do
  sleep 0.02
done
flush_storage
: > "$dir/closed-over-port"
