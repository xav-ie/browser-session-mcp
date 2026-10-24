#!/usr/bin/env bash
# End-to-end smoke test: spins up chrome-headless-shell on a random port,
# launches the MCP server pointed at it, drives a session through the full
# tool surface, and tears everything down.
#
# Uses a bash `coproc` for sequential stdio JSON-RPC so dependent requests
# wait for their predecessors' responses (rmcp dispatches concurrently, so
# pipelined requests can race).
#
# Overrides:
#   CHROME_BIN  — path to chrome-headless-shell (default: nix build .#chrome-headless-shell)
#   MCP_BIN     — path to browser-session       (default: target/release/browser-session)
set -euo pipefail

cd "$(dirname "$0")/.."

# ---- resolve binaries ----------------------------------------------------

if [[ -z ${CHROME_BIN:-} ]]; then
  echo "+ resolving chrome-headless-shell via nix"
  CHROME_BIN=$(nix build --no-link --print-out-paths \
    "$(git rev-parse --show-toplevel)#chrome-headless-shell")/bin/chrome-headless-shell
fi
if [[ ! -x $CHROME_BIN ]]; then
  echo "CHROME_BIN ($CHROME_BIN) is not executable" >&2
  exit 1
fi

if [[ -z ${MCP_BIN:-} ]]; then
  MCP_BIN="target/release/browser-session"
fi
if [[ ! -x $MCP_BIN ]]; then
  echo "+ building release binary"
  cargo build --release
fi

if ! command -v python3 >/dev/null 2>&1; then
  echo "python3 is required (used for JSON extraction)" >&2
  exit 1
fi
if ! python3 -c 'import jsonschema' 2>/dev/null; then
  echo "python3's jsonschema module is required (checks results against each tool's outputSchema)" >&2
  exit 1
fi

# ---- tempdir + port ------------------------------------------------------

PORT=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')
TMP=$(mktemp -d /tmp/browser-session-mcp-smoke.XXXXXX)
export STATE_DIR="$TMP"
export BROWSER_URL="http://127.0.0.1:$PORT"
# Only builds takeover links; nothing serves them during the smoke run.
export TAKEOVER_BASE_URL="http://127.0.0.1:1"

# Runs from the EXIT trap below.
# shellcheck disable=SC2329
cleanup() {
  rc=$?
  [[ -n ${SERVER_PID:-} ]] && kill "$SERVER_PID" 2>/dev/null || true
  [[ -n ${CHROME_PID:-} ]] && kill "$CHROME_PID" 2>/dev/null || true
  # Wait for chrome to fully exit before removing its data dir — otherwise it's
  # still writing chrome-data/ and `rm -rf` races it ("Directory not empty").
  [[ -n ${CHROME_PID:-} ]] && wait "$CHROME_PID" 2>/dev/null || true
  if ((rc != 0)); then
    echo
    echo "---- chrome log (tail) ----"
    tail -n 40 "$TMP/chrome.log" 2>/dev/null || true
    echo "---- mcp log (tail) ----"
    tail -n 40 "$TMP/mcp.log" 2>/dev/null || true
  fi
  rm -rf "$TMP" 2>/dev/null || true
  exit "$rc"
}
trap cleanup EXIT

# ---- start chrome --------------------------------------------------------

"$CHROME_BIN" \
  --headless \
  --no-sandbox \
  --disable-gpu \
  --disable-dev-shm-usage \
  --remote-debugging-port="$PORT" \
  --user-data-dir="$TMP/chrome-data" \
  >"$TMP/chrome.log" 2>&1 &
CHROME_PID=$!

echo "+ waiting for chrome on :$PORT (pid $CHROME_PID)"
for _ in $(seq 1 40); do
  if curl -fs "http://127.0.0.1:$PORT/json/version" >/dev/null 2>&1; then
    break
  fi
  sleep 0.25
done
if ! curl -fs "http://127.0.0.1:$PORT/json/version" >/dev/null 2>&1; then
  echo "chrome failed to come up" >&2
  exit 1
fi

# ---- start MCP server ----------------------------------------------------

coproc SERVER { exec "$MCP_BIN" mcp 2>"$TMP/mcp.log"; }

req() {
  printf '%s\n' "$1" >&"${SERVER[1]}"
  IFS= read -r -u "${SERVER[0]}" line
  check_output "$1" "$line"
  printf '%s' "$line"
}

# Validate a tools/call result's structuredContent against the tool's
# outputSchema from tools/list, appending any mismatch to schema-failures.
# req() runs in a subshell, so failures go through a file rather than FAIL.
check_output() {
  [[ -f $TMP/tools.json ]] || return 0
  python3 - "$TMP/tools.json" "$1" "$2" >>"$TMP/schema-failures" <<'PY'
import json, sys
from jsonschema import validators

tools = {t["name"]: t for t in json.load(open(sys.argv[1]))}
request, response = json.loads(sys.argv[2]), json.loads(sys.argv[3])
if request.get("method") != "tools/call":
    sys.exit()
name = request["params"]["name"]
result = response.get("result", {})
schema = tools.get(name, {}).get("outputSchema")
if not schema or result.get("isError"):
    sys.exit()
if "structuredContent" not in result:
    print(f"{name}: declares an outputSchema but returned no structuredContent")
    sys.exit()
validator = validators.validator_for(schema)(schema)
for error in validator.iter_errors(result["structuredContent"]):
    print(f"{name}: {error.json_path}: {error.message}")
PY
}

# Call a tool by name with a JSON arguments object.
call() {
  req "$(python3 -c 'import json, sys
print(json.dumps({"jsonrpc": "2.0", "id": 100, "method": "tools/call",
                  "params": {"name": sys.argv[1], "arguments": json.loads(sys.argv[2])}}))' "$1" "$2")"
}

notify() {
  printf '%s\n' "$1" >&"${SERVER[1]}"
}

# Extract a field from a JSON-RPC response.
# Usage: pluck "$RESP" .result.structuredContent.sessionId
pluck() {
  printf '%s' "$1" | python3 -c "
import json, sys
path = sys.argv[1].split('.')
data = json.load(sys.stdin)
for p in path[1:]:
    data = data[p] if not p.isdigit() else data[int(p)]
print(data)
" "$2"
}

PASS=0
FAIL=0
check() {
  if [[ $2 == *"$3"* ]]; then
    echo "[PASS] $1"
    PASS=$((PASS + 1))
  else
    echo "[FAIL] $1"
    echo "       expected substring: $3"
    echo "       got: $(printf '%s' "$2" | head -c 280)"
    FAIL=$((FAIL + 1))
  fi
}

# ---- protocol handshake --------------------------------------------------

INIT=$(req '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}')
check "initialize" "$INIT" '"browser-session-mcp"'

notify '{"jsonrpc":"2.0","method":"notifications/initialized"}'

LIST=$(req '{"jsonrpc":"2.0","id":2,"method":"tools/list"}')
check "tools/list has open_browser_session" "$LIST" '"open_browser_session"'
check "tools/list has navigate" "$LIST" '"navigate"'
check "tools/list has take_snapshot" "$LIST" '"take_snapshot"'
check "tools/list has save_browser_state" "$LIST" '"save_browser_state"'
printf '%s' "$LIST" | python3 -c 'import json, sys
json.dump(json.load(sys.stdin)["result"]["tools"], open(sys.argv[1], "w"))' "$TMP/tools.json"

# ---- session lifecycle ---------------------------------------------------

OPEN=$(req '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"open_browser_session","arguments":{}}}')
check "open_browser_session returns sessionId" "$OPEN" '"sessionId"'
SID=$(pluck "$OPEN" .result.structuredContent.sessionId)
echo "       session: $SID"

# ---- navigate + snapshot -------------------------------------------------

NAV_REQ=$(python3 -c '
import json,sys
print(json.dumps({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{
  "name":"navigate","arguments":{
    "sessionId": sys.argv[1],
    "url":"data:text/html,<title>hi</title><h1>hello%20smoke</h1>"
  }}}))' "$SID")
NAV=$(req "$NAV_REQ")
check "navigate succeeds" "$NAV" 'Navigated to'

SNAP_REQ=$(python3 -c '
import json,sys
print(json.dumps({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{
  "name":"take_snapshot","arguments":{"sessionId":sys.argv[1]}}}))' "$SID")
SNAP=$(req "$SNAP_REQ")
check "snapshot contains heading text" "$SNAP" 'hello smoke'

# ---- evaluate (both forms) -----------------------------------------------

EVAL_REQ=$(python3 -c '
import json,sys
print(json.dumps({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{
  "name":"evaluate","arguments":{"sessionId":sys.argv[1],"expression":"1 + 1"}}}))' "$SID")
EVAL=$(req "$EVAL_REQ")
check "evaluate bare expression -> 2" "$EVAL" '"result":2'

EVAL2_REQ=$(python3 -c '
import json,sys
print(json.dumps({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{
  "name":"evaluate","arguments":{
    "sessionId":sys.argv[1],
    "expression":"const t = document.title; return t;"
  }}}))' "$SID")
EVAL2=$(req "$EVAL2_REQ")
check "evaluate with explicit return" "$EVAL2" '"hi"'

# Regression: document.returnValue should NOT trigger the "looks-like-return"
# wrap and instead be evaluated as an expression. The page hasn't set that
# property, so the result is undefined -> JSON null. The check is that the
# call succeeds and returns null (not a syntax error from wrapping).
EVAL3_REQ=$(python3 -c '
import json,sys
print(json.dumps({"jsonrpc":"2.0","id":8,"method":"tools/call","params":{
  "name":"evaluate","arguments":{
    "sessionId":sys.argv[1],
    "expression":"document.foo_return_bar === undefined"
  }}}))' "$SID")
EVAL3=$(req "$EVAL3_REQ")
check "evaluate: word-bounded 'return' check" "$EVAL3" '"result":true'

# ---- session listing -----------------------------------------------------

SESS=$(req '{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"list_browser_sessions"}}')
check "list_browser_sessions includes our id" "$SESS" "$SID"

# ---- saved-state round-trip ----------------------------------------------

SAVE_REQ=$(python3 -c '
import json,sys
print(json.dumps({"jsonrpc":"2.0","id":10,"method":"tools/call","params":{
  "name":"save_browser_state","arguments":{
    "sessionId":sys.argv[1],"name":"smoke_test"}}}))' "$SID")
SAVE=$(req "$SAVE_REQ")
check "save_browser_state succeeds" "$SAVE" 'Saved'

STATES=$(req '{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"list_browser_states"}}')
check "list_browser_states sees smoke_test" "$STATES" 'smoke_test'

LOAD_REQ=$(python3 -c '
import json,sys
print(json.dumps({"jsonrpc":"2.0","id":12,"method":"tools/call","params":{
  "name":"load_browser_state","arguments":{
    "sessionId":sys.argv[1],"name":"smoke_test"}}}))' "$SID")
LOAD=$(req "$LOAD_REQ")
check "load_browser_state succeeds" "$LOAD" 'Loaded'

DEL=$(req '{"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":"delete_browser_state","arguments":{"name":"smoke_test"}}}')
check "delete_browser_state succeeds" "$DEL" 'Deleted'

# Disk artifact left over?
if [[ -e "$STATE_DIR/states/smoke_test.json" ]]; then
  echo "[FAIL] delete_browser_state left smoke_test.json on disk"
  FAIL=$((FAIL + 1))
else
  echo "[PASS] delete_browser_state removed smoke_test.json"
  PASS=$((PASS + 1))
fi

# ---- tabs ----------------------------------------------------------------

NEW_REQ=$(python3 -c '
import json,sys
print(json.dumps({"jsonrpc":"2.0","id":14,"method":"tools/call","params":{
  "name":"new_page","arguments":{
    "sessionId":sys.argv[1],
    "url":"data:text/html,<title>tab2</title>"
  }}}))' "$SID")
NEW=$(req "$NEW_REQ")
check "new_page opens a tab" "$NEW" 'Opened tab'

LIST_PAGES_REQ=$(python3 -c '
import json,sys
print(json.dumps({"jsonrpc":"2.0","id":15,"method":"tools/call","params":{
  "name":"list_pages","arguments":{"sessionId":sys.argv[1]}}}))' "$SID")
LIST_PAGES=$(req "$LIST_PAGES_REQ")
check "list_pages shows both tabs" "$LIST_PAGES" 'tab2'

# ---- close ---------------------------------------------------------------

# The remaining tools with an outputSchema, so check_output sees each result.
ARGS=$(printf '{"sessionId":"%s"}' "$SID")
check "switch_tab activates tab 0" "$(call switch_tab "$(printf '{"sessionId":"%s","tabIndex":0}' "$SID")")" 'Switched to tab #0'
check "set_stealth on" "$(call set_stealth "$(printf '{"sessionId":"%s","enabled":true}' "$SID")")" '"stealth":true'
check "get_stealth reads it back" "$(call get_stealth "$ARGS")" '"stealth":true'
check "set_stealth off" "$(call set_stealth "$(printf '{"sessionId":"%s","enabled":false}' "$SID")")" '"stealth":false'
check "list_visits returns visits" "$(call list_visits "$ARGS")" '"visits"'
check "list_console_messages returns messages" "$(call list_console_messages "$ARGS")" '"messages"'
check "list_network_requests returns requests" "$(call list_network_requests "$ARGS")" '"requests"'
check "start_profiling starts" "$(call start_profiling "$ARGS")" 'CPU profiler running'
check "stop_profiling returns a summary" "$(call stop_profiling "$ARGS")" '"durationMs"'
TAKEOVER=$(call request_human_takeover "$ARGS")
check "request_human_takeover returns a link" "$TAKEOVER" '/takeover/'
TOKEN=$(pluck "$TAKEOVER" .result.structuredContent.token)
check "await_human_takeover times out with nobody there" "$(call await_human_takeover "$(printf '{"token":"%s","timeout":1}' "$TOKEN")")" '"completed":false'

CLOSE_REQ=$(python3 -c '
import json,sys
print(json.dumps({"jsonrpc":"2.0","id":16,"method":"tools/call","params":{
  "name":"close_browser_session","arguments":{"sessionId":sys.argv[1]}}}))' "$SID")
CLOSE=$(req "$CLOSE_REQ")
check "close_browser_session succeeds" "$CLOSE" 'Closed'

# The session's state file should be gone. Poll briefly rather than checking the
# instant close returns, in case a debounced flush (~1s, see src/state.rs) is
# still settling.
cleared=0
for _ in $(seq 1 20); do
  if [[ ! -f $STATE_DIR/sessions/$SID.json ]]; then
    cleared=1
    break
  fi
  sleep 0.25
done
if ((cleared)); then
  echo "[PASS] session state file removed on close"
  PASS=$((PASS + 1))
else
  echo "[FAIL] session state file still present after close"
  FAIL=$((FAIL + 1))
fi

if [[ -s $TMP/schema-failures ]]; then
  echo "[FAIL] results match their outputSchema:"
  sed 's/^/       /' "$TMP/schema-failures"
  FAIL=$((FAIL + 1))
else
  echo "[PASS] every tool result matches its outputSchema"
  PASS=$((PASS + 1))
fi

echo
echo "$PASS pass · $FAIL fail"
exit $((FAIL == 0 ? 0 : 1))
