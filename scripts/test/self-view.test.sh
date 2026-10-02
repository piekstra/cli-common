#!/usr/bin/env bash
# Tests for scripts/self-view.sh: the PID binding and every refusal path.
# Window data comes from a stub (SELF_VIEW_WINDOW_LIST), so this runs on
# Linux CI with no display; no test posts an input event or captures pixels.
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

# windows LINE... — point the window source at a stub printing these lines.
windows() {
  printf '%s\n' "$@" >"$work/windows.txt"
  export SELF_VIEW_WINDOW_LIST="cat '$work/windows.txt'"
}

# ── usage ───────────────────────────────────────────────────────────────────
run "no args is usage" 2 -- "$sv"
run "unknown command is usage" 2 -- "$sv" frobnicate
run "help exits 0" 0 "launch" -- "$sv" help
run "launch needs a command" 2 -- "$sv" launch --pidfile "$pf" --
run "shot needs a pidfile" 2 "never by name" -- "$sv" shot

# ── no launch, no capture ──────────────────────────────────────────────────
windows "4242 7 0 0 0 1600 1000"
run "shot without a launch refuses" 1 "never captures a window it did not launch" -- "$sv" shot --pidfile "$pf" --wait 0
run "bounds without a launch refuses" 1 "no pidfile" -- "$sv" bounds --pidfile "$pf" --wait 0

# ── launch records PID and start time ───────────────────────────────────────
run "launch" 0 -- "$sv" launch --pidfile "$pf" -- bash -c 'sleep 60 & sleep 60 & wait'
root="$LAST"
[ "$(head -n1 "$pf")" = "$root" ] || { echo "FAIL pidfile line 1 is not the PID"; fails=$((fails + 1)); }
[ -n "$(sed -n 2p "$pf")" ] || { echo "FAIL pidfile has no start time"; fails=$((fails + 1)); }
run "second launch on a live pidfile refuses" 1 "already tracks" -- "$sv" launch --pidfile "$pf" -- sleep 1

sleep 0.3
run "pids lists the tree" 0 -- "$sv" pids --pidfile "$pf"
tree="$LAST"
[ "$(wc -l <<<"$tree" | tr -d ' ')" -ge 3 ] || { echo "FAIL tree has fewer than 3 PIDs: $tree"; fails=$((fails + 1)); }
[ "$(head -n1 <<<"$tree")" = "$root" ] || { echo "FAIL tree does not start at the root"; fails=$((fails + 1)); }
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
run "only foreign windows: refuse" 1 "Refusing to fall back" -- "$sv" bounds --pidfile "$pf" --wait 0
run "only foreign windows: shot refuses too" 1 "Refusing to fall back" -- "$sv" shot --pidfile "$pf" --wait 0 --out "$work/x.png"
[ ! -e "$work/x.png" ] || { echo "FAIL a refused shot left a file"; fails=$((fails + 1)); }

windows ""
run "no windows at all: refuse" 1 "no on-screen window" -- "$sv" bounds --pidfile "$pf" --wait 0

# ── drive refuses before sending anything ───────────────────────────────────
windows "$root 11 0 100 50 800 600"
run "drive outside the window is usage" 2 "outside" -- "$sv" drive --pidfile "$pf" click 900 10 --wait 0
run "drive with bad coordinates is usage" 2 -- "$sv" drive --pidfile "$pf" click -5 10 --wait 0
windows "1 900 0 300 100 200 200" "$root 11 0 100 50 800 600"
run "drive refuses when a foreign window covers the point" 1 "no event was sent" -- "$sv" drive --pidfile "$pf" click 250 100 --wait 0

# ── stale pidfiles ──────────────────────────────────────────────────────────
printf '%s\n%s\n%s\n' "$root" "Thu Jan  1 00:00:00 1970" "x" >"$work/tampered.pid"
run "a recycled PID (start time differs) refuses" 1 "different process" -- "$sv" bounds --pidfile "$work/tampered.pid" --wait 0
echo "not-a-pid" >"$work/garbage.pid"
run "a garbage pidfile refuses" 1 "does not hold a PID" -- "$sv" bounds --pidfile "$work/garbage.pid" --wait 0

# ── web: loopback only, and only a server we launched ──────────────────────
run "web needs --serve or --pidfile" 2 "only a server this script launched" -- "$sv" web --url http://127.0.0.1:1/
run "web refuses a non-loopback URL" 1 "not a loopback" -- "$sv" web --pidfile "$pf" --url https://example.com/
run "web refuses a loopback lookalike host" 1 "not a loopback" -- "$sv" web --pidfile "$pf" --url http://localhost.example.com/
run "web rejects both --serve and --pidfile" 2 "not both" -- "$sv" web --pidfile "$pf" --serve true --url http://127.0.0.1:1/
if command -v lsof >/dev/null 2>&1; then
  run "web refuses when the launched process does not listen" 1 "nothing listens" -- "$sv" web --pidfile "$pf" --url http://127.0.0.1:9/ --wait 0
  if command -v python3 >/dev/null 2>&1; then
    port=$(( 20000 + RANDOM % 20000 ))
    "$sv" launch --pidfile "$work/other.pid" -- python3 -m http.server "$port" --bind 127.0.0.1 >/dev/null
    for _ in 1 2 3 4 5 6 7 8 9 10; do
      lsof -nP -iTCP:"$port" -sTCP:LISTEN -t >/dev/null 2>&1 && break
      sleep 0.5
    done
    run "web refuses a port served outside the launched tree" 1 "outside the launched process" -- "$sv" web --pidfile "$pf" --url "http://127.0.0.1:$port/" --wait 0
    "$sv" stop --pidfile "$work/other.pid" >/dev/null
  fi
fi

# ── stop ────────────────────────────────────────────────────────────────────
run "stop" 0 "stopped $root" -- "$sv" stop --pidfile "$pf"
[ ! -e "$pf" ] || { echo "FAIL stop left the pidfile"; fails=$((fails + 1)); }
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
run "an exited launch refuses and names the hand-off case" 1 "has exited" -- "$sv" shot --pidfile "$pf" --wait 0
run "stop clears an exited launch" 0 -- "$sv" stop --pidfile "$pf"
run "launch a command that cannot start" 1 "exited immediately" -- "$sv" launch --pidfile "$pf" -- /nonexistent/binary

echo "self-view: $passes passed, $fails failed"
[ "$fails" -eq 0 ]
