#!/usr/bin/env bash
# self-view.sh — let a builder (a person or an agent) see the app it is
# building, and nothing else on the machine.
#
# The rule: a window is captured or driven only when its owning process is
# the process this script launched, or a descendant of it. The launch records
# the PID and the process start time in a pidfile; every later command reads
# that pidfile and refuses when it is missing, stale, or names a process that
# owns no window. Windows are never matched by app name, title or executable
# path: the owner's own copy of the app may be running with real data, and it
# has the same name.
#
# Commands:
#   launch --pidfile F [--log F] -- CMD [ARG...]
#       Start CMD in the background and record it in F. Prints the PID.
#   shot   --pidfile F [--out PNG] [--wait SECS]
#       Capture only the launched app's largest window. Prints the PNG path.
#   bounds --pidfile F [--wait SECS]
#       Print that window's "x y width height" in screen points.
#   drive  --pidfile F {move|click} X Y [--wait SECS]
#       Move the pointer to, or click, a point given relative to the window's
#       top-left. When another app's window covers the point, raises the
#       launched app once; refuses if the point is still covered.
#   web    --url URL [--out PNG] [--size WxH] [--wait SECS]
#          (--pidfile F | --serve CMD)
#       Render a web frontend in a headless browser with a fresh, empty
#       profile. The URL must be on loopback and its port must be served by
#       the launched process tree. --serve launches CMD, shoots, then stops it.
#   pids   --pidfile F
#       Print the launched process tree, one PID per line.
#   stop   --pidfile F
#       Terminate the launched process tree and remove F.
#
# Exit codes (the family's table, DESIGN.md §1.5):
#   0 ok
#   1 refused for safety: another app's window or listener is involved, the
#     URL is not loopback, or the pidfile already tracks a running launch
#   2 usage
#   3 permission missing: Screen Recording (shot) or Accessibility (drive)
#   4 nothing to act on: no pidfile, the launched process exited or its PID
#     was recycled, no window tied to it, nothing listening
#   5 environment: screen locked, a Swift helper or the browser failed, a
#     needed tool is missing, the command exited at launch
#
# macOS only for shot/bounds/drive (CGWindowList, screencapture, CGEvent):
# shot needs Screen Recording and drive needs Accessibility, granted to the
# terminal or agent process. launch/pids/stop/web are portable.
#
# Test seams. Each replaces one platform effect with a command, run by bash;
# selection and every safety check still apply to what it returns:
#   SELF_VIEW_WINDOW_LIST  prints lines in lib/self-view-windows.swift's format
#   SELF_VIEW_LOCK_STATE   prints "locked" or "open" (lib/self-view-locked.swift)
#   SELF_VIEW_RAISE        gets the owner PID (lib/self-view-raise.swift)
#   SELF_VIEW_POST_EVENT   gets "move|click X Y" (lib/self-view-post.swift)
set -euo pipefail

prog="self-view"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

usage() {
  sed -n '/^# Commands:/,/^#   5 environment/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//' >&2
  exit "${1:-2}"
}
fail() { local code="$1"; shift; echo "$prog: $*" >&2; exit "$code"; }
refuse() { fail 1 "$@"; }
bad_usage() { fail 2 "$@"; }
no_permission() { fail 3 "$@"; }
missing() { fail 4 "$@"; }
env_fail() { fail 5 "$@"; }

is_uint() { [[ "${1:-}" =~ ^[0-9]+$ ]]; }

# Empty (and status 0, so `set -e` callers can test it) when $1 is not running.
start_time() { { ps -o lstart= -p "$1" 2>/dev/null || true; } | sed 's/^ *//; s/ *$//'; }

# helper SEAM_VALUE SWIFT_FILE ARG... — run a platform effect: the seam's
# command when set, else the Swift helper. Keeps the helper's exit status.
helper() {
  local seam="$1" file="$2"
  shift 2
  if [ -n "$seam" ]; then
    bash -c "$seam" "$prog" "$@"
  else
    [ "$(uname)" = Darwin ] || { echo "needs macOS; for a web frontend use '$prog web'" >&2; return 5; }
    command -v swift >/dev/null 2>&1 || { echo "swift not found; install the Xcode command line tools" >&2; return 5; }
    swift "$here/lib/$file" "$@"
  fi
}

# ── pidfile ─────────────────────────────────────────────────────────────────
# Line 1: PID. Line 2: the process start time as `ps -o lstart=` reports it,
# so a recycled PID (the app exited, the OS reused the number) is caught.
# Line 3: the launched command, for messages.

ROOT=""
load_pidfile() {
  local f="$1" pid started cmd now
  [ -n "$f" ] || bad_usage "--pidfile is required; windows are only ever chosen by the PID this script launched, never by name"
  [ -f "$f" ] || missing "no pidfile at $f. Start the app with '$prog launch --pidfile $f -- <cmd>' first. This script never captures a window it did not launch."
  { read -r pid; read -r started; read -r cmd; } <"$f" || true
  is_uint "$pid" || missing "$f does not hold a PID; relaunch with '$prog launch'."
  now="$(start_time "$pid")"
  if [ -z "$now" ]; then
    missing "the launched process $pid (${cmd:-?}) has exited. If the app hands off to an already-running copy, that copy is not ours and will not be captured. Run '$prog stop --pidfile $f', then launch again."
  fi
  if [ "$now" != "$started" ]; then
    missing "PID $pid is now a different process (started $now, the launch recorded $started). Run '$prog stop --pidfile $f', then launch again."
  fi
  ROOT="$pid"
}

# The launched process and every descendant, one PID per line.
tree_pids() {
  ps -A -o pid= -o ppid= | awk -v root="$1" '
    { parent[$1] = $2; pids[++n] = $1 }
    END {
      keep[root] = 1; print root
      do {
        grew = 0
        for (i = 1; i <= n; i++) {
          p = pids[i]
          if (!(p in keep) && (parent[p] in keep)) { keep[p] = 1; print p; grew = 1 }
        }
      } while (grew)
    }'
}

# Set WINDOWS to the on-screen window list; a failing source ends the run
# with its own error rather than reading as "no window".
ERRF=""
read_windows() {
  [ -n "$ERRF" ] || { ERRF="$(mktemp "${TMPDIR:-/tmp}/self-view-err.XXXXXX")"; trap 'rm -f "$ERRF"' EXIT; }
  if ! WINDOWS="$(helper "${SELF_VIEW_WINDOW_LIST:-}" self-view-windows.swift 2>"$ERRF")"; then
    env_fail "listing windows failed: $(tr '\n' ' ' <"$ERRF")"
  fi
}

# Set WIN_PID/WIN_ID/WIN_X/WIN_Y/WIN_W/WIN_H to the largest normal (layer 0)
# window owned by the launched tree, polling up to $1 seconds while the app
# starts. Runs in the caller's shell, not a command substitution, so a
# refusal exits the script and WINDOWS stays available to drive.
WINDOWS=""
WIN_PID="" WIN_ID="" WIN_X="" WIN_Y="" WIN_W="" WIN_H=""
find_window() {
  local wait="$1" deadline pids win
  deadline=$(( $(date +%s) + wait ))
  while :; do
    pids="$(tree_pids "$ROOT" | tr '\n' ' ')"
    read_windows
    win="$(awk -v pids="$pids" '
      BEGIN { n = split(pids, a, " "); for (i = 1; i <= n; i++) ours[a[i]] = 1 }
      ($1 in ours) && $3 == 0 {
        area = $6 * $7
        if (area > best) { best = area; out = $1 " " $2 " " $4 " " $5 " " $6 " " $7 }
      }
      END { if (best > 0) print out }' <<<"$WINDOWS")"
    if [ -n "$win" ]; then
      read -r WIN_PID WIN_ID WIN_X WIN_Y WIN_W WIN_H <<<"$win"
      return 0
    fi
    [ -n "$(start_time "$ROOT")" ] || missing "the launched process $ROOT exited before it opened a window; nothing captured."
    [ "$(date +%s)" -lt "$deadline" ] || break
    sleep 1
  done
  missing "no on-screen window is owned by the launched process $ROOT or its $(( $(wc -w <<<"$pids") - 1 )) descendants after ${wait}s. Refusing to fall back to any other window. Check that the app finished starting and is not minimized, or raise --wait."
}

# Refuse while the session is locked: windows still list, but no pixels can
# be read and no window can be raised. Anything but a clear "open" refuses.
refuse_if_locked() {
  local state
  state="$(helper "${SELF_VIEW_LOCK_STATE:-}" self-view-locked.swift 2>&1)" \
    || env_fail "$1: could not read the screen-lock state: $state"
  case "$state" in
    open) ;;
    locked) env_fail "$1: the screen is locked; unlock it and retry. Nothing was captured or sent." ;;
    *) env_fail "$1: unexpected screen-lock state '$state'; nothing was captured or sent." ;;
  esac
}

# The id of the frontmost normal window containing screen point ($1, $2).
front_window_at() {
  awk -v px="$1" -v py="$2" '
    $3 == 0 && px >= $4 && px < $4 + $6 && py >= $5 && py < $5 + $7 { print $2; exit }' <<<"$WINDOWS"
}

# ── commands ────────────────────────────────────────────────────────────────

cmd_launch() {
  local pidfile="" log="" pid started old
  while [ $# -gt 0 ]; do
    case "$1" in
      --pidfile) pidfile="${2:-}"; shift 2 ;;
      --log) log="${2:-}"; shift 2 ;;
      --) shift; break ;;
      *) bad_usage "launch: unknown argument '$1' (the command goes after --)" ;;
    esac
  done
  [ -n "$pidfile" ] || bad_usage "launch: --pidfile is required"
  [ $# -gt 0 ] || bad_usage "launch: no command given after --"
  if [ -f "$pidfile" ]; then
    old="$(head -n1 "$pidfile")"
    if is_uint "$old" && [ -n "$(start_time "$old")" ] \
      && [ "$(start_time "$old")" = "$(sed -n 2p "$pidfile")" ]; then
      refuse "$pidfile already tracks running process $old. Run '$prog stop --pidfile $pidfile' first."
    fi
  fi
  mkdir -p "$(dirname "$pidfile")"
  log="${log:-${pidfile%.pid}.log}"
  nohup "$@" >"$log" 2>&1 </dev/null &
  pid=$!
  started="$(start_time "$pid")"
  # A command that cannot start (not found, bad flags) dies within moments.
  sleep 0.2
  [ -n "$started" ] && [ "$(start_time "$pid")" = "$started" ] \
    || env_fail "'$*' exited immediately; see $log"
  printf '%s\n%s\n%s\n' "$pid" "$started" "$*" >"$pidfile"
  echo "$pid"
}

cmd_stop() {
  local pidfile="" pids p q
  while [ $# -gt 0 ]; do
    case "$1" in
      --pidfile) pidfile="${2:-}"; shift 2 ;;
      *) bad_usage "stop: unknown argument '$1'" ;;
    esac
  done
  [ -n "$pidfile" ] || bad_usage "stop: --pidfile is required"
  [ -f "$pidfile" ] || { echo "$prog: nothing to stop ($pidfile does not exist)" >&2; return 0; }
  if ( load_pidfile "$pidfile" ) 2>/dev/null; then
    load_pidfile "$pidfile"
    pids="$(tree_pids "$ROOT")"
    # shellcheck disable=SC2086 # word-splitting the PID list is intended
    kill -TERM $pids 2>/dev/null || true
    for _ in 1 2 3 4 5 6 7 8 9 10; do
      p=""
      for q in $pids; do kill -0 "$q" 2>/dev/null && p="$p $q"; done
      [ -n "$p" ] || break
      sleep 0.5
    done
    # shellcheck disable=SC2086
    [ -z "$p" ] || kill -KILL $p 2>/dev/null || true
    echo "stopped $ROOT"
  else
    echo "$prog: the recorded process is gone; removing $pidfile" >&2
  fi
  rm -f "$pidfile"
}

cmd_pids() {
  local pidfile=""
  while [ $# -gt 0 ]; do
    case "$1" in
      --pidfile) pidfile="${2:-}"; shift 2 ;;
      *) bad_usage "pids: unknown argument '$1'" ;;
    esac
  done
  load_pidfile "$pidfile"
  tree_pids "$ROOT"
}

cmd_shot() {
  local pidfile="" out="" wait=30
  while [ $# -gt 0 ]; do
    case "$1" in
      --pidfile) pidfile="${2:-}"; shift 2 ;;
      --out) out="${2:-}"; shift 2 ;;
      --wait) wait="${2:-}"; shift 2 ;;
      *) bad_usage "shot: unknown argument '$1'" ;;
    esac
  done
  is_uint "$wait" || bad_usage "shot: --wait takes whole seconds"
  load_pidfile "$pidfile"
  find_window "$wait"
  refuse_if_locked shot
  command -v screencapture >/dev/null 2>&1 || env_fail "shot: screencapture not found (macOS only)."
  out="${out:-${TMPDIR:-/tmp}/self-view-$ROOT.png}"
  rm -f "$out"
  # -l<id>: this one window's pixels only, never the full screen.
  screencapture -o -x -l"$WIN_ID" "$out" || true
  [ -s "$out" ] || no_permission "screencapture could not read window $WIN_ID. Grant Screen Recording to the terminal or agent process (System Settings > Privacy & Security), then retry."
  echo "$out"
}

cmd_bounds() {
  local pidfile="" wait=30
  while [ $# -gt 0 ]; do
    case "$1" in
      --pidfile) pidfile="${2:-}"; shift 2 ;;
      --wait) wait="${2:-}"; shift 2 ;;
      *) bad_usage "bounds: unknown argument '$1'" ;;
    esac
  done
  is_uint "$wait" || bad_usage "bounds: --wait takes whole seconds"
  load_pidfile "$pidfile"
  find_window "$wait"
  echo "$WIN_X $WIN_Y $WIN_W $WIN_H"
}

cmd_drive() {
  local pidfile="" wait=30 action="" px="" py="" sx sy front="" rc
  while [ $# -gt 0 ]; do
    case "$1" in
      --pidfile) pidfile="${2:-}"; shift 2 ;;
      --wait) wait="${2:-}"; shift 2 ;;
      move|click) action="$1"; px="${2:-}"; py="${3:-}"; shift 3 || shift $# ;;
      *) bad_usage "drive: unknown argument '$1'" ;;
    esac
  done
  [ -n "$action" ] || bad_usage "drive: give 'move X Y' or 'click X Y'"
  [[ "$px" =~ ^[0-9]+(\.[0-9]+)?$ && "$py" =~ ^[0-9]+(\.[0-9]+)?$ ]] \
    || bad_usage "drive: X and Y are non-negative numbers relative to the window's top-left"
  is_uint "$wait" || bad_usage "drive: --wait takes whole seconds"
  load_pidfile "$pidfile"
  find_window "$wait"
  awk -v a="$px" -v b="$py" -v w="$WIN_W" -v h="$WIN_H" 'BEGIN { exit !(a < w && b < h) }' \
    || bad_usage "drive: ($px, $py) is outside the ${WIN_W}x${WIN_H} window"
  sx="$(awk -v a="$WIN_X" -v b="$px" 'BEGIN { print a + b }')"
  sy="$(awk -v a="$WIN_Y" -v b="$py" 'BEGIN { print a + b }')"
  refuse_if_locked drive
  # The event lands on whatever window is frontmost at that point, so the
  # frontmost normal window there must be ours. If it is not, bring our app
  # forward once and look again.
  front="$(front_window_at "$sx" "$sy")"
  if [ "$front" != "$WIN_ID" ]; then
    helper "${SELF_VIEW_RAISE:-}" self-view-raise.swift "$WIN_PID" >/dev/null 2>&1 || true
    find_window 0
    sx="$(awk -v a="$WIN_X" -v b="$px" 'BEGIN { print a + b }')"
    sy="$(awk -v a="$WIN_Y" -v b="$py" 'BEGIN { print a + b }')"
    front="$(front_window_at "$sx" "$sy")"
  fi
  [ "$front" = "$WIN_ID" ] \
    || refuse "drive: another window (id ${front:-none}) is in front of the launched app at ($px, $py) and raising the app did not clear it. Bring its window to the front and retry; no event was sent."
  rc=0
  helper "${SELF_VIEW_POST_EVENT:-}" self-view-post.swift "$action" "$sx" "$sy" || rc=$?
  case "$rc" in
    0) ;;
    3) no_permission "drive: this process may not post input events. Grant Accessibility to the terminal or agent process (System Settings > Privacy & Security), then retry; nothing was sent." ;;
    *) env_fail "drive: the $action at ($px, $py) could not be confirmed (helper exit $rc); the pointer is not at the target." ;;
  esac
}

find_browser() {
  local c p v
  if [ -n "${SELF_VIEW_BROWSER:-}" ]; then echo "$SELF_VIEW_BROWSER"; return; fi
  # Playwright's headless shell first, newest build: it has no keychain
  # integration and no UI, so nothing can prompt.
  c="$(for p in "$HOME"/Library/Caches/ms-playwright/chromium_headless_shell-*/*/chrome-headless-shell \
                "$HOME"/.cache/ms-playwright/chromium_headless_shell-*/*/chrome-headless-shell; do
         v="${p#*chromium_headless_shell-}"
         [ -x "$p" ] && printf '%s\t%s\n' "${v%%/*}" "$p"
       done | sort -n | tail -n1 | cut -f2-)"
  [ -n "$c" ] && { echo "$c"; return; }
  command -v chrome-headless-shell >/dev/null 2>&1 && { command -v chrome-headless-shell; return; }
  for c in \
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
    "/Applications/Chromium.app/Contents/MacOS/Chromium"; do
    [ -x "$c" ] && { echo "$c"; return; }
  done
  for c in google-chrome chromium chromium-browser chrome; do
    command -v "$c" >/dev/null 2>&1 && { command -v "$c"; return; }
  done
  return 1
}

cmd_web() {
  local pidfile="" serve="" url="" out="" size="1280x800" wait=30 port
  local browser bpid profile deadline listeners outside served=""
  while [ $# -gt 0 ]; do
    case "$1" in
      --pidfile) pidfile="${2:-}"; shift 2 ;;
      --serve) serve="${2:-}"; shift 2 ;;
      --url) url="${2:-}"; shift 2 ;;
      --out) out="${2:-}"; shift 2 ;;
      --size) size="${2:-}"; shift 2 ;;
      --wait) wait="${2:-}"; shift 2 ;;
      *) bad_usage "web: unknown argument '$1'" ;;
    esac
  done
  [ -n "$url" ] || bad_usage "web: --url is required"
  [[ "$size" =~ ^[0-9]+x[0-9]+$ ]] || bad_usage "web: --size is WIDTHxHEIGHT"
  is_uint "$wait" || bad_usage "web: --wait takes whole seconds"
  if [ -n "$pidfile" ] && [ -n "$serve" ]; then bad_usage "web: give --pidfile or --serve, not both"; fi
  if [ -z "$pidfile" ] && [ -z "$serve" ]; then
    bad_usage "web: give --serve CMD, or --pidfile F from '$prog launch'; only a server this script launched is rendered"
  fi
  [[ "$url" =~ ^https?://(localhost|127\.0\.0\.1|\[::1\])(:([0-9]+))?(/.*)?$ ]] \
    || refuse "web: $url is not a loopback http(s) URL; only a local dev server the builder launched is rendered."
  port="${BASH_REMATCH[3]}"
  if [ -z "$port" ]; then
    case "$url" in https:*) port=443 ;; *) port=80 ;; esac
  fi
  command -v lsof >/dev/null 2>&1 || env_fail "web: lsof is needed to tie port $port to the launched process; install it."

  if [ -n "$serve" ]; then
    pidfile="$(mktemp -d "${TMPDIR:-/tmp}/self-view-web.XXXXXX")/server.pid"
    served="$pidfile"
    cmd_launch --pidfile "$pidfile" -- bash -c "$serve" >/dev/null
    # shellcheck disable=SC2064 # expand now: the pidfile path is fixed
    trap "cmd_stop --pidfile '$pidfile' >/dev/null 2>&1 || true" EXIT
  fi
  load_pidfile "$pidfile"

  # Wait until the port listens, then require every listener to be ours.
  deadline=$(( $(date +%s) + wait ))
  while :; do
    listeners="$(lsof -nP -iTCP:"$port" -sTCP:LISTEN -t 2>/dev/null | sort -u || true)"
    [ -n "$listeners" ] && break
    [ -n "$(start_time "$ROOT")" ] || missing "web: the launched server $ROOT exited before listening on port $port${served:+; see ${served%.pid}.log}"
    [ "$(date +%s)" -lt "$deadline" ] || missing "web: nothing listens on port $port after ${wait}s."
    sleep 1
  done
  outside="$(comm -23 <(echo "$listeners") <(tree_pids "$ROOT" | sort -u))"
  [ -z "$outside" ] \
    || refuse "web: port $port is served by PID(s) $(echo "$outside" | tr '\n' ' ')outside the launched process $ROOT. Refusing to render a server this script did not launch."

  browser="$(find_browser)" || env_fail "web: no Chrome/Chromium found; set SELF_VIEW_BROWSER to a Chromium-family binary."
  out="${out:-${TMPDIR:-/tmp}/self-view-web-$ROOT.png}"
  rm -f "$out"
  # A throwaway profile: no cookies, logins or extensions from the owner's
  # browser can reach the render. The mock keychain and basic password store
  # keep a full Chrome from asking the macOS keychain for its storage key, a
  # prompt that hangs a session nobody is watching.
  profile="$(mktemp -d "${TMPDIR:-/tmp}/self-view-profile.XXXXXX")"
  "$browser" --headless --disable-gpu --hide-scrollbars --no-first-run \
    --no-default-browser-check --use-mock-keychain --password-store=basic \
    --user-data-dir="$profile" \
    --window-size="${size/x/,}" --virtual-time-budget=5000 \
    --screenshot="$out" "$url" >/dev/null 2>&1 &
  bpid=$!
  deadline=$(( $(date +%s) + wait + 30 ))
  while kill -0 "$bpid" 2>/dev/null && [ "$(date +%s)" -lt "$deadline" ]; do sleep 0.5; done
  if kill -0 "$bpid" 2>/dev/null; then
    # shellcheck disable=SC2046 # one PID per word
    kill -KILL $(tree_pids "$bpid") 2>/dev/null || true
    rm -rf "$profile"
    env_fail "web: the headless browser ($browser) did not finish within $(( wait + 30 ))s; killed it."
  fi
  rm -rf "$profile"
  [ -s "$out" ] || env_fail "web: the headless browser produced no screenshot of $url."
  echo "$out"
}

[ $# -gt 0 ] || usage
sub="$1"; shift
case "$sub" in
  launch) cmd_launch "$@" ;;
  stop) cmd_stop "$@" ;;
  pids) cmd_pids "$@" ;;
  shot) cmd_shot "$@" ;;
  bounds) cmd_bounds "$@" ;;
  drive) cmd_drive "$@" ;;
  web) cmd_web "$@" ;;
  -h|--help|help) usage 0 ;;
  *) bad_usage "unknown command '$sub' (launch, shot, bounds, drive, web, pids, stop)" ;;
esac
