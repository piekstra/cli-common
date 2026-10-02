#!/usr/bin/env bash
# Tests for scripts/self-view.sh: the PID binding, every refusal path and its
# exit code, and drive's success path. The platform effects (window list,
# lock state, raise, event post) are stubbed through the script's seams, so
# this runs on Linux CI with no display and never posts a real event or
# captures pixels.
set -uo pipefail

sv="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/self-view.sh"
work="$(mktemp -d "${TMPDIR:-/tmp}/self-view-test.XXXXXX")"
pf="$work/app.pid"
fails=0 passes=0

cleanup() {
  "$sv" stop --pidfile "$pf" >/dev/null 2>&1 || true
  "$sv" stop --pidfile "$work/other.pid" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT

# run NAME WANT_RC [WANT_SUBSTRING] -- CMD...
run() {
  local name="$1" want="$2" needle="" out rc
  shift 2
  if [ "$1" != "--" ]; then needle="$1"; shift; fi
  shift
  out="$("$@" 2>&1)"; rc=$?
  if [ "$rc" != "$want" ] || { [ -n "$needle" ] && [[ "$out" != *"$needle"* ]]; }; then
    echo "FAIL $name: rc=$rc (want $want)${needle:+, want output containing: $needle}"
    printf '    %s\n' "$out"
    fails=$((fails + 1))
  else
    passes=$((passes + 1))
  fi
  LAST="$out"
}
check() { # check NAME CONDITION...
  local name="$1"; shift
  if "$@"; then passes=$((passes + 1)); else echo "FAIL $name"; fails=$((fails + 1)); fi
}

# windows LINE... — the window list the stub prints.
windows() { printf '%s\n' "$@" >"$work/windows.txt"; }
export SELF_VIEW_WINDOW_LIST="cat '$work/windows.txt'"
export SELF_VIEW_LOCK_STATE="echo open"
export SELF_VIEW_POST_EVENT="echo \"\$*\" >>'$work/posted'"
export SELF_VIEW_RAISE="echo \"\$1\" >>'$work/raised'"

# ── usage ───────────────────────────────────────────────────────────────────
run "no args is usage" 2 -- "$sv"
run "unknown command is usage" 2 -- "$sv" frobnicate
run "help exits 0" 0 "launch" -- "$sv" help
run "launch needs a command" 2 -- "$sv" launch --pidfile "$pf" --
run "shot needs a pidfile" 2 "never by name" -- "$sv" shot

# ── no launch, no capture ──────────────────────────────────────────────────
windows "4242 7 0 0 0 1600 1000"
run "shot without a launch: 4" 4 "never captures a window it did not launch" -- "$sv" shot --pidfile "$pf" --wait 0
run "bounds without a launch: 4" 4 "no pidfile" -- "$sv" bounds --pidfile "$pf" --wait 0

# ── launch records PID and start time ───────────────────────────────────────
run "launch" 0 -- "$sv" launch --pidfile "$pf" -- bash -c 'sleep 60 & sleep 60 & wait'
root="$LAST"
check "pidfile line 1 is the PID" [ "$(head -n1 "$pf")" = "$root" ]
check "pidfile records a start time" [ -n "$(sed -n 2p "$pf")" ]
run "second launch on a live pidfile: 1" 1 "already tracks" -- "$sv" launch --pidfile "$pf" -- sleep 1

sleep 0.3
run "pids lists the tree" 0 -- "$sv" pids --pidfile "$pf"
tree="$LAST"
check "tree has the root and two children" [ "$(wc -l <<<"$tree" | tr -d ' ')" -ge 3 ]
check "tree starts at the root" [ "$(head -n1 <<<"$tree")" = "$root" ]
child="$(sed -n 2p <<<"$tree")"

# ── selection is by PID only ────────────────────────────────────────────────
# A bigger window owned by a foreign process (the owner's running copy of the
# same app) sits in front; it must never be chosen.
windows "1 900 0 0 0 3000 2000" "$root 11 0 100 50 800 600"
run "picks the launched window over a bigger foreign one" 0 "100 50 800 600" -- "$sv" bounds --pidfile "$pf" --wait 0

windows "1 900 0 0 0 3000 2000" "$child 12 0 10 20 640 480" "$root 13 0 0 0 200 100"
run "a descendant's window counts; the largest wins" 0 "10 20 640 480" -- "$sv" bounds --pidfile "$pf" --wait 0

windows "$root 14 25 0 0 3000 30" "$root 15 0 5 5 300 200"
run "non-normal layers (menu bar, overlays) are ignored" 0 "5 5 300 200" -- "$sv" bounds --pidfile "$pf" --wait 0

windows "1 900 0 0 0 3000 2000" "4242 7 0 0 0 1600 1000"
run "only foreign windows: 4" 4 "Refusing to fall back" -- "$sv" bounds --pidfile "$pf" --wait 0
run "only foreign windows: shot refuses too" 4 "Refusing to fall back" -- "$sv" shot --pidfile "$pf" --wait 0 --out "$work/x.png"
check "a refused shot left no file" [ ! -e "$work/x.png" ]

windows ""
run "no windows at all: 4" 4 "no on-screen window" -- "$sv" bounds --pidfile "$pf" --wait 0

run "a failing window source: 5, with its error" 5 "boom" -- \
  env SELF_VIEW_WINDOW_LIST="echo boom >&2; exit 7" "$sv" bounds --pidfile "$pf" --wait 30

# ── drive ───────────────────────────────────────────────────────────────────
windows "$root 11 0 100 50 800 600"
run "drive outside the window is usage" 2 "outside" -- "$sv" drive --pidfile "$pf" click 900 10 --wait 0
run "drive with bad coordinates is usage" 2 -- "$sv" drive --pidfile "$pf" click -5 10 --wait 0
run "drive while locked: 5, nothing sent" 5 "locked" -- env SELF_VIEW_LOCK_STATE="echo locked" "$sv" drive --pidfile "$pf" click 10 10 --wait 0
run "drive with an unreadable lock state: 5" 5 "unexpected" -- env SELF_VIEW_LOCK_STATE="echo" "$sv" drive --pidfile "$pf" click 10 10 --wait 0
check "nothing posted while locked" [ ! -e "$work/posted" ]

run "drive click" 0 -- "$sv" drive --pidfile "$pf" click 40 30 --wait 0
check "click posted at window origin + point" [ "$(tail -n1 "$work/posted")" = "click 140 80" ]
check "no raise when the point is ours" [ ! -e "$work/raised" ]

run "drive without Accessibility: 3" 3 "Accessibility" -- env SELF_VIEW_POST_EVENT="exit 3" "$sv" drive --pidfile "$pf" move 40 30 --wait 0
run "drive unconfirmed: 5" 5 "could not be confirmed" -- env SELF_VIEW_POST_EVENT="exit 5" "$sv" drive --pidfile "$pf" move 40 30 --wait 0

# A foreign window covers the point; raising does not help -> refuse, no event.
windows "1 900 0 300 100 200 200" "$root 11 0 100 50 800 600"
: >"$work/posted"
run "drive refuses when a foreign window stays in front: 1" 1 "no event was sent" -- "$sv" drive --pidfile "$pf" click 250 100 --wait 0
check "raise was attempted for the launched window's owner" [ "$(tail -n1 "$work/raised")" = "$root" ]
check "nothing posted under a foreign window" [ ! -s "$work/posted" ]

# Raising brings our window forward -> the click goes through.
run "drive raises, then clicks" 0 -- env SELF_VIEW_RAISE="printf '%s\n' '$root 11 0 100 50 800 600' >'$work/windows.txt'" \
  "$sv" drive --pidfile "$pf" click 250 100 --wait 0
check "click posted after the raise" [ "$(tail -n1 "$work/posted")" = "click 350 150" ]

# ── shot ────────────────────────────────────────────────────────────────────
windows "$root 11 0 100 50 800 600"
run "shot while locked: 5" 5 "locked" -- env SELF_VIEW_LOCK_STATE="echo locked" "$sv" shot --pidfile "$pf" --wait 0 --out "$work/x.png"

# ── stale pidfiles ──────────────────────────────────────────────────────────
printf '%s\n%s\n%s\n' "$root" "Thu Jan  1 00:00:00 1970" "x" >"$work/tampered.pid"
run "a recycled PID (start time differs): 4" 4 "different process" -- "$sv" bounds --pidfile "$work/tampered.pid" --wait 0
echo "not-a-pid" >"$work/garbage.pid"
run "a garbage pidfile: 4" 4 "does not hold a PID" -- "$sv" bounds --pidfile "$work/garbage.pid" --wait 0

# ── web: loopback only, and only a server we launched ──────────────────────
run "web needs --serve or --pidfile" 2 "only a server this script launched" -- "$sv" web --url http://127.0.0.1:1/
run "web refuses a non-loopback URL: 1" 1 "not a loopback" -- "$sv" web --pidfile "$pf" --url https://example.com/
run "web refuses a loopback lookalike host: 1" 1 "not a loopback" -- "$sv" web --pidfile "$pf" --url http://localhost.example.com/
run "web rejects both --serve and --pidfile" 2 "not both" -- "$sv" web --pidfile "$pf" --serve true --url http://127.0.0.1:1/
if command -v lsof >/dev/null 2>&1; then
  run "web when the launched process does not listen: 4" 4 "nothing listens" -- "$sv" web --pidfile "$pf" --url http://127.0.0.1:9/ --wait 0
  if command -v python3 >/dev/null 2>&1; then
    port=$(( 20000 + RANDOM % 20000 ))
    "$sv" launch --pidfile "$work/other.pid" -- python3 -m http.server "$port" --bind 127.0.0.1 >/dev/null
    for _ in 1 2 3 4 5 6 7 8 9 10; do
      lsof -nP -iTCP:"$port" -sTCP:LISTEN -t >/dev/null 2>&1 && break
      sleep 0.5
    done
    run "web refuses a port served outside the launched tree: 1" 1 "outside the launched process" -- "$sv" web --pidfile "$pf" --url "http://127.0.0.1:$port/" --wait 0
    "$sv" stop --pidfile "$work/other.pid" >/dev/null
  fi
fi

# ── stop ────────────────────────────────────────────────────────────────────
run "stop" 0 "stopped $root" -- "$sv" stop --pidfile "$pf"
check "stop removed the pidfile" [ ! -e "$pf" ]
sleep 0.2
for p in $tree; do
  if ps -p "$p" >/dev/null 2>&1 && [ "$(ps -o stat= -p "$p" | cut -c1)" != Z ]; then
    echo "FAIL stop left PID $p running"; fails=$((fails + 1))
  fi
done
run "stop with no pidfile is a no-op" 0 -- "$sv" stop --pidfile "$pf"

# ── a launched process that exits ───────────────────────────────────────────
run "launch a short-lived process" 0 -- "$sv" launch --pidfile "$pf" -- sleep 0.5
sleep 1
windows "1 900 0 0 0 3000 2000"
run "an exited launch: 4, naming the hand-off case" 4 "has exited" -- "$sv" shot --pidfile "$pf" --wait 0
run "stop clears an exited launch" 0 -- "$sv" stop --pidfile "$pf"
run "launch a command that cannot start: 5" 5 "exited immediately" -- "$sv" launch --pidfile "$pf" -- /nonexistent/binary

echo "self-view: $passes passed, $fails failed"
[ "$fails" -eq 0 ]
