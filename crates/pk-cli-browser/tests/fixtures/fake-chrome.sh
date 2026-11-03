#!/bin/sh
# Stands in for Chrome in pk-cli-browser's launch tests (src/chrome.rs), so
# launch, the debug-port read and teardown are exercised without a browser.
# Records its arguments in the profile, and in `default-at-launch` what its
# Default directory holds as it starts, writes DevToolsActivePort the way
# Chrome does, then sleeps until it is stopped. A start page containing
# `exit-early` makes it exit before writing the port, like a Chrome that
# fails to start. One containing `flush-on-stop` makes it write to its site
# storage when told to stop, like a Chrome flushing as it shuts down, leaving
# `flushed-on-stop` beside it as the proof. One containing
# `leave-holder-<pid>.` makes it name that pid in SingletonLock and exit once
# the port is written, like a wrapper that leaves the browser running.
dir=
flush=
holder=
for arg in "$@"; do
  case "$arg" in
    --user-data-dir=*) dir="${arg#--user-data-dir=}" ;;
    *exit-early*) exit 3 ;;
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
if [ -n "$flush" ]; then
  sleep 600 &
  nap=$!
  trap 'mkdir -p "$dir/Default/Local Storage/leveldb"
    printf dummy-refresh-token > "$dir/Default/Local Storage/leveldb/000003.log"
    : > "$dir/flushed-on-stop"
    kill "$nap"
    exit 0' TERM
fi
printf '9222\n/devtools/browser/fake-browser-id\n' > "$dir/DevToolsActivePort.tmp"
mv "$dir/DevToolsActivePort.tmp" "$dir/DevToolsActivePort"
[ -z "$holder" ] || exit 0
if [ -n "$flush" ]; then
  wait "$nap"
  exit 0
fi
exec sleep 600
