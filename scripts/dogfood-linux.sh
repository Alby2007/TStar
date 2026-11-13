#!/usr/bin/env bash
# dogfood-linux.sh — Linux dogfood: prove lifecycle containment —
# clean close, hard kill (SIGKILL of the host), restart — leaves no
# surviving runtime or worker processes.
#
# Usage:
#   Tests-only mode (app already built/installed — CI, or a manual run):
#       scripts/dogfood-linux.sh --exe /path/to/app-binary
#
#   Full mode (local dev): scaffold a fresh app, build, dpkg -x the
#   .deb into a temp dir, run the tests on the extracted binary:
#       scripts/dogfood-linux.sh [--version next]
#
# Options:
#   --exe PATH            app binary to exercise (tests-only mode)
#   --name NAME           app name (default: basename of --exe / random)
#   --workdir DIR         scratch dir for scaffold/extract (default mktemp)
#   --version TAG         create-pylair version, full mode (default next)
#   --keep                keep the work dir afterwards
#   --spawn-timeout S     wait for the runtime child    (default 90)
#   --exit-timeout S      wait for a clean close        (default 30)
#   --survivor-timeout S  wait for processes to die     (default 15)
#
# Environment:
#   DISPLAY unset + xvfb-run present => launches are wrapped in
#   `xvfb-run -a` (CI docker legs have no display). Set DISPLAY to run
#   against a real server. PYLAIR_SELF_TEST_WINDOW_CLOSE is set for the
#   close tests — the POSIX mirror of Windows' WM_CLOSE.
#
# Process model (mirrors dogfood-windows.ps1):
#   The Python runtime spawns eagerly at plugin setup, so a poll for a
#   python child under the app token IS the positive control — if it
#   never appears the test fails as invalid.
#   Matching is executable-path-contains-app-basename: on Linux the
#   bundled interpreter lands under a dir named after the app
#   (/usr/lib/<app>/python/bin/python3), so host, runtime and workers
#   all carry the token. A survivor check NEVER uses bare kill -0:
#   each pid carries a start-time token (/proc/<pid>/stat field 22) so
#   a recycled pid cannot fake death — the same identity rule as the
#   orphan test in tests/hardening_step0.rs.
set -euo pipefail

# ---- args -------------------------------------------------------------------
EXE="" APP_NAME="" WORKDIR="" VERSION="next" KEEP=0
SPAWN_TIMEOUT=90 EXIT_TIMEOUT=30 SURVIVOR_TIMEOUT=15
while [ $# -gt 0 ]; do
    case "$1" in
        --exe) EXE=$2; shift 2;;
        --name) APP_NAME=$2; shift 2;;
        --workdir) WORKDIR=$2; shift 2;;
        --version) VERSION=$2; shift 2;;
        --keep) KEEP=1; shift;;
        --spawn-timeout) SPAWN_TIMEOUT=$2; shift 2;;
        --exit-timeout) EXIT_TIMEOUT=$2; shift 2;;
        --survivor-timeout) SURVIVOR_TIMEOUT=$2; shift 2;;
        *) echo "unknown arg: $1" >&2; exit 2;;
    esac
done

declare -a RESULTS=()
add_result() { RESULTS+=("$1|$2|$3"); printf '[%s] %s -- %s\n' "$2" "$1" "$3"; }

wait_until() { # cond-fn timeout-sec
    local deadline=$(( $(date +%s) + $2 ))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        $1 && return 0
        sleep 0.25
    done
    return 1
}

# ---- process identity ---------------------------------------------------------
# start-time token: field 22 of /proc/<pid>/stat, read after the last ')'
# so a comm containing spaces/parens can't shift the field index.
proc_start_time() {
    local stat rest
    stat=$(cat "/proc/$1/stat" 2>/dev/null) || return 1
    rest=${stat##*) }   # comm may hold spaces/parens — split on the LAST ')'
    # rest begins at field 3 (state); starttime is field 22 -> index 20.
    awk '{print $20}' <<<"$rest"
}

alive_since() { # pid token — same process instance still up
    local now
    now=$(proc_start_time "$1") || return 1
    [ "$now" = "$2" ]
}

# "pid token exe" for every process whose exe path contains the app token.
procs_matching() {
    local pid exe tok
    for d in /proc/[0-9]*/exe; do
        pid=${d#/proc/}; pid=${pid%/exe}
        exe=$(readlink "$d" 2>/dev/null) || continue
        case "$exe" in
            *"$APP_TOKEN"*)
                tok=$(proc_start_time "$pid") || tok='?'
                printf '%s %s %s\n' "$pid" "$tok" "$exe";;
        esac
    done
}

python_child_seen() { procs_matching | grep -qw python; }
no_survivors()      { [ -z "$(procs_matching)" ]; }

kill_matching() {
    procs_matching | awk '{print $1}' | while read -r p; do
        kill -9 "$p" 2>/dev/null || true
    done
}

# The REAL app pid — never the xvfb-run wrapper, whose kill would orphan
# the app instead of killing it.
resolve_app_pid() {
    procs_matching | awk -v e="$EXE_REAL" '$3 == e {print $1; exit}'
}

# ---- launcher ---------------------------------------------------------------
LAUNCH=()
if [ -z "${DISPLAY:-}" ] && command -v xvfb-run >/dev/null 2>&1; then
    LAUNCH=(xvfb-run -a)
fi

# start_app <mode> <tag>: mode "close" sets the self-close hook; tag names
# the log files. The launcher is a direct child of THIS shell so `wait`
# yields the app's exit status (xvfb-run forwards it).
start_app() { # mode tag
    local mode=$1 tag=$2
    if [ "$mode" = close ]; then
        PYLAIR_SELF_TEST_WINDOW_CLOSE=1 "${LAUNCH[@]}" "$EXE_REAL" \
            >"$LOGDIR/$tag.out" 2>"$LOGDIR/$tag.err" &
    else
        "${LAUNCH[@]}" "$EXE_REAL" >"$LOGDIR/$tag.out" 2>"$LOGDIR/$tag.err" &
    fi
}

wait_app_gone() { # app_pid timeout-sec
    local deadline=$(( $(date +%s) + $2 ))
    while [ "$(date +%s)" -lt "$deadline" ] && kill -0 "$1" 2>/dev/null; do
        sleep 0.25
    done
    ! kill -0 "$1" 2>/dev/null
}

# ---- phase: locate or build the binary --------------------------------------
LOGDIR=${TMPDIR:-/tmp}; WORKDIR_CREATED=0
if [ -z "$EXE" ]; then
    if [ -n "$WORKDIR" ]; then mkdir -p "$WORKDIR"; else WORKDIR=$(mktemp -d /tmp/pylair-dogfood-XXXXXX); fi
    WORKDIR=$(readlink -f "$WORKDIR")
    WORKDIR_CREATED=1
    APP_NAME=${APP_NAME:-dogfood-$(head -c4 /dev/urandom | od -An -tx1 | tr -d ' ')}
    echo "scaffolding $APP_NAME (pylair@$VERSION) in $WORKDIR"
    (cd "$WORKDIR" && npm --yes create "pylair@$VERSION" -- "$APP_NAME")
    APP_DIR=$WORKDIR/$APP_NAME
    (cd "$APP_DIR" && npm install && uv sync && npx pylair build --bundles deb)
    DEB=$(find "$APP_DIR/src-tauri/target/release/bundle/deb" -name '*.deb' | head -1)
    [ -n "$DEB" ] || { echo "no .deb under bundle/deb" >&2; exit 1; }
    dpkg -x "$DEB" "$WORKDIR/approot"
    EXE=$(find "$WORKDIR/approot" -type f -name "$APP_NAME" -path '*/bin/*' | head -1)
    [ -n "$EXE" ] || { echo "extracted binary $APP_NAME not found" >&2; exit 1; }
    LOGDIR=$WORKDIR
else
    APP_NAME=${APP_NAME:-$(basename "$EXE")}
fi
[ -x "$EXE" ] || { echo "exe not executable/found: $EXE" >&2; exit 1; }
EXE_REAL=$(readlink -f "$EXE")
APP_TOKEN=$APP_NAME
echo "app: $EXE_REAL"
echo "match token: $APP_TOKEN"

# Refuse a token so generic it would sweep system processes.
case "$APP_TOKEN" in
    ""|usr|bin|lib|python*|app) echo "refusing generic match token: $APP_TOKEN" >&2; exit 2;;
esac

kill_matching; wait_until no_survivors "$SURVIVOR_TIMEOUT" || true

# ---- TEST A: clean close ------------------------------------------------------
test_a() {
    start_app close A; local host=$!
    if ! wait_until python_child_seen "$SPAWN_TIMEOUT"; then
        kill -9 "$host" 2>/dev/null || true
        add_result 'A clean-close' FAIL "invalid: no python child within ${SPAWN_TIMEOUT}s"
        return
    fi
    local app_pid; app_pid=$(resolve_app_pid)
    [ -n "$app_pid" ] || { add_result 'A clean-close' FAIL "could not resolve app pid"; return; }
    local closed=0 panicked=0 clean=0 code=0
    wait_app_gone "$app_pid" "$EXIT_TIMEOUT" && closed=1
    wait "$host" 2>/dev/null || code=$?
    grep -q 'panicked' "$LOGDIR/A.err" && panicked=1
    wait_until no_survivors "$SURVIVOR_TIMEOUT" && clean=1 || kill_matching
    if [ "$closed" = 1 ] && [ "$code" = 0 ] && [ "$panicked" = 0 ] && [ "$clean" = 1 ]; then
        add_result 'A clean-close' PASS "exit 0, no panic, no survivors"
    else
        add_result 'A clean-close' FAIL "closed=$closed exit=$code panicked=$panicked noSurvivors=$clean"
    fi
}

# ---- TEST B: hard kill --------------------------------------------------------
test_b() {
    start_app plain B; local host=$!
    if ! wait_until python_child_seen "$SPAWN_TIMEOUT"; then
        kill -9 "$host" 2>/dev/null || true
        add_result 'B hard-kill' FAIL "invalid: no python child within ${SPAWN_TIMEOUT}s"
        return
    fi
    local app_pid rtok rpid
    app_pid=$(resolve_app_pid)
    [ -n "$app_pid" ] || { add_result 'B hard-kill' FAIL "could not resolve app pid"; return; }
    # Runtime identity before the kill — proves THAT instance died, not
    # just that some python is or isn't around later.
    rpid=$(procs_matching | awk '/python/{print $1; exit}')
    rtok=$(procs_matching | awk -v p="$rpid" '$1==p{print $2; exit}')
    kill -9 "$app_pid" || true  # SIGKILL the host ONLY — containment must prove itself
    local gone=0
    wait_until no_survivors "$SURVIVOR_TIMEOUT" && gone=1
    if [ "$gone" = 1 ]; then
        local extra=""
        [ -n "${rpid:-}" ] && alive_since "$rpid" "${rtok:-?}" \
            && extra=" (note: runtime pid $rpid lingered past the poll)"
        add_result 'B hard-kill' PASS "containment: nothing survived host SIGKILL$extra"
    else
        local left
        left=$(procs_matching | awk '{printf "%s(%s) ", $3, $1}')
        add_result 'B hard-kill' FAIL "survivors after SIGKILL: $left"
        kill_matching
    fi
}

# ---- TEST C: restart after kill -----------------------------------------------
test_c() {
    start_app close C; local host=$!
    if ! wait_until python_child_seen "$SPAWN_TIMEOUT"; then
        kill -9 "$host" 2>/dev/null || true
        add_result 'C restart' FAIL "no python child within ${SPAWN_TIMEOUT}s — stale state after kill?"
        return
    fi
    local app_pid closed=0 clean=0 code=0
    app_pid=$(resolve_app_pid)
    [ -n "$app_pid" ] || { add_result 'C restart' FAIL "could not resolve app pid"; return; }
    wait_app_gone "$app_pid" "$EXIT_TIMEOUT" && closed=1
    wait "$host" 2>/dev/null || code=$?
    wait_until no_survivors "$SURVIVOR_TIMEOUT" && clean=1 || kill_matching
    if [ "$closed" = 1 ] && [ "$code" = 0 ] && [ "$clean" = 1 ]; then
        add_result 'C restart' PASS "restarted clean, closed clean"
    else
        add_result 'C restart' FAIL "closed=$closed exit=$code noSurvivors=$clean"
    fi
}

test_a; test_b; test_c

# ---- cleanup + report ----------------------------------------------------------
kill_matching
if [ "$WORKDIR_CREATED" = 1 ] && [ "$KEEP" = 0 ]; then rm -rf "$WORKDIR"; fi

echo
echo '-- dogfood results --'
printf '%s\n' "${RESULTS[@]}" | awk -F'|' '{printf "%-14s %-6s %s\n", $1, $2, $3}'
fails=$(printf '%s\n' "${RESULTS[@]}" | grep -c '|FAIL|' || true)
[ "${#RESULTS[@]}" -eq 0 ] && { echo "no tests ran" >&2; exit 1; }
[ "$fails" -gt 0 ] && exit 1
echo 'all dogfood tests passed'
