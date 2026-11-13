#!/usr/bin/env bash
# dogfood-macos.sh — macOS dogfood: prove lifecycle containment —
# clean close, hard kill (SIGKILL of the host), restart — leaves no
# surviving runtime or worker processes.
#
# NOT wired into CI — run it on a real Mac (a WindowServer session is
# required; there is no xvfb on macOS).
#
# Usage:
#   Tests-only mode (a .app already built — e.g. after `pylair build`):
#       scripts/dogfood-macos.sh --app /path/to/Foo.app
#   or point straight at the inner binary:
#       scripts/dogfood-macos.sh --exe /path/Foo.app/Contents/MacOS/foo
#
#   Full mode (local dev): scaffold a fresh app, build, copy the .app
#   to a temp dir, run the tests on it:
#       scripts/dogfood-macos.sh [--version next]
#
# Options:
#   --app PATH            .app bundle to exercise (tests-only mode)
#   --exe PATH            inner binary (Contents/MacOS/<name>)
#   --name NAME           app name (default: basename minus .app / random)
#   --workdir DIR         scratch dir for scaffold/copy (default mktemp)
#   --version TAG         create-pylair version, full mode (default next)
#   --keep                keep the work dir afterwards
#   --spawn-timeout S     wait for the runtime child    (default 90)
#   --exit-timeout S      wait for a clean close        (default 30)
#   --survivor-timeout S  wait for processes to die     (default 15)
#
# Process model (mirrors dogfood-windows.ps1 / dogfood-linux.sh):
#   The Python runtime spawns eagerly at plugin setup, so a poll for a
#   python child under the app token IS the positive control — if it
#   never appears the test fails as invalid.
#   Matching is executable-path-contains-app-basename: the bundled
#   interpreter lives inside the bundle
#   (Foo.app/Contents/Resources/python/bin/python3), so host, runtime
#   and workers all carry the token. A survivor check NEVER uses bare
#   kill -0: each pid carries an lstart token from `ps -o lstart=` —
#   same instance check as the orphan test's (pid, start-time) rule in
#   tests/hardening_step0.rs.
set -euo pipefail

# ---- args -------------------------------------------------------------------
APP="" EXE="" APP_NAME="" WORKDIR="" VERSION="next" KEEP=0
SPAWN_TIMEOUT=90 EXIT_TIMEOUT=30 SURVIVOR_TIMEOUT=15
while [ $# -gt 0 ]; do
    case "$1" in
        --app) APP=$2; shift 2;;
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
# start-time token: `lstart` — same pid AND same start timestamp means the
# same instance; a recycled pid fails the comparison.
proc_start_time() {
    ps -p "$1" -o lstart= 2>/dev/null
}

alive_since() { # pid token — same process instance still up
    local now
    now=$(proc_start_time "$1") || return 1
    [ -n "$now" ] && [ "$now" = "$2" ]
}

# "pid exe" for every process whose command path contains the app token.
# macOS `ps -o comm` reports the executable path as invoked — the bundled
# interpreter's path runs through the .app, so it carries the token.
procs_matching() {
    local pid comm tok
    while read -r pid comm; do
        case "$comm" in
            *"$APP_TOKEN"*)
                tok=$(proc_start_time "$pid" 2>/dev/null) || tok='?'
                printf '%s %s\n' "$pid" "$tok";;
        esac
    done < <(ps -ax -o pid=,comm= 2>/dev/null)
}

python_child_seen() { ps -ax -o comm= | grep python | grep -q "$APP_TOKEN"; }
no_survivors()      { [ -z "$(procs_matching)" ]; }

kill_matching() {
    procs_matching | awk '{print $1}' | while read -r p; do
        kill -9 "$p" 2>/dev/null || true
    done
}

# The REAL app pid: the process whose comm is exactly the binary path.
resolve_app_pid() {
    ps -ax -o pid=,comm= | awk -v e="$EXE" '$2 == e {print $1; exit}'
}

# ---- launcher -----------------------------------------------------------------
# start_app <mode> <tag>: mode "close" sets the self-close hook; tag names
# the log files. The app is a direct child of THIS shell so `wait` yields
# its exit status.
start_app() { # mode tag
    local mode=$1 tag=$2
    if [ "$mode" = close ]; then
        PYLAIR_SELF_TEST_WINDOW_CLOSE=1 "$EXE" \
            >"$LOGDIR/$tag.out" 2>"$LOGDIR/$tag.err" &
    else
        "$EXE" >"$LOGDIR/$tag.out" 2>"$LOGDIR/$tag.err" &
    fi
}

wait_app_gone() { # app_pid timeout-sec
    local deadline=$(( $(date +%s) + $2 ))
    while [ "$(date +%s)" -lt "$deadline" ] && kill -0 "$1" 2>/dev/null; do
        sleep 0.25
    done
    ! kill -0 "$1" 2>/dev/null
}

# ---- phase: locate or build the binary ----------------------------------------
LOGDIR=${TMPDIR:-/tmp}; WORKDIR_CREATED=0
if [ -n "$APP" ] && [ -z "$EXE" ]; then
    base=$(basename "$APP" .app)
    EXE="$APP/Contents/MacOS/$base"
fi
if [ -z "$EXE" ]; then
    WORKDIR=${WORKDIR:-$(mktemp -d /tmp/pylair-dogfood-XXXXXX)}
    WORKDIR_CREATED=1
    APP_NAME=${APP_NAME:-dogfood-$(head -c4 /dev/urandom | od -An -tx1 | tr -d ' ')}
    echo "scaffolding $APP_NAME (pylair@$VERSION) in $WORKDIR"
    (cd "$WORKDIR" && npm --yes create "pylair@$VERSION" -- "$APP_NAME")
    APP_DIR=$WORKDIR/$APP_NAME
    (cd "$APP_DIR" && npm install && uv sync && npx pylair build)
    APP=$(find "$APP_DIR/src-tauri/target/release/bundle/macos" -maxdepth 1 -name '*.app' | head -1)
    [ -n "$APP" ] || { echo "no .app under bundle/macos" >&2; exit 1; }
    ditto "$APP" "$WORKDIR/$APP_NAME.app"
    EXE="$WORKDIR/$APP_NAME.app/Contents/MacOS/$APP_NAME"
    LOGDIR=$WORKDIR
fi
[ -x "$EXE" ] || { echo "exe not executable/found: $EXE" >&2; exit 1; }
APP_NAME=${APP_NAME:-$(basename "$(dirname "$(dirname "$(dirname "$EXE")")")" .app)}
APP_TOKEN=$APP_NAME
echo "app: $EXE"
echo "match token: $APP_TOKEN"

# Refuse a token so generic it would sweep system processes.
case "$APP_TOKEN" in
    ""|usr|bin|lib|python*|app|Contents|MacOS) echo "refusing generic match token: $APP_TOKEN" >&2; exit 2;;
esac

kill_matching; wait_until no_survivors "$SURVIVOR_TIMEOUT" || true

# ---- TEST A: clean close --------------------------------------------------------
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

# ---- TEST B: hard kill ----------------------------------------------------------
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
    rpid=$(ps -ax -o pid=,comm= | awk -v t="$APP_TOKEN" '$2 ~ t && $2 ~ /python/ {print $1; exit}')
    rtok=$(proc_start_time "$rpid" 2>/dev/null || echo '?')
    kill -9 "$app_pid" || true  # SIGKILL the host ONLY — containment must prove itself
    local gone=0
    wait_until no_survivors "$SURVIVOR_TIMEOUT" && gone=1
    if [ "$gone" = 1 ]; then
        add_result 'B hard-kill' PASS "containment: nothing survived host SIGKILL"
    else
        local left
        left=$(ps -ax -o pid=,comm= | awk -v t="$APP_TOKEN" '$2 ~ t {printf "%s(%s) ", $2, $1}')
        add_result 'B hard-kill' FAIL "survivors after SIGKILL: $left"
        kill_matching
    fi
}

# ---- TEST C: restart after kill --------------------------------------------------
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

# ---- cleanup + report -------------------------------------------------------------
kill_matching
if [ "$WORKDIR_CREATED" = 1 ] && [ "$KEEP" = 0 ]; then rm -rf "$WORKDIR"; fi

echo
echo '-- dogfood results --'
printf '%s\n' "${RESULTS[@]}" | awk -F'|' '{printf "%-14s %-6s %s\n", $1, $2, $3}'
fails=$(printf '%s\n' "${RESULTS[@]}" | grep -c '|FAIL|' || true)
[ "${#RESULTS[@]}" -eq 0 ] && { echo "no tests ran" >&2; exit 1; }
[ "$fails" -gt 0 ] && exit 1
echo 'all dogfood tests passed'
