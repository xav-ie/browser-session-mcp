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
  [[ -n ${JEV_PID:-} ]] && kill "$JEV_PID" 2>/dev/null || true
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

# ---- stub Jev API ----------------------------------------------------------
# A tiny /v1/systemone that "picks" the element matching the goal, so every
# jev path runs end-to-end without a real TypeSafe key: explicit `jev:`
# goals, the automatic fallback for selectors that match nothing as CSS, and
# the act loop. Cues it reads from the state's first line (the element menu
# below it mentions every marker too): "email" -> pick the input, otherwise
# the "Log in" button; "bogus" -> answer with an unknown id (fail-closed
# validation); "unsure" -> confidence 0.2 (the confidence floor). act
# requests carry goal_met/stuck questions: it picks whichever element the
# goal names ("name"/"color", or the "Do it" button once one thing is typed),
# answers goal_met 0.97 once the title says act-done, stuck 0.97 for
# "impossible" goals, and second-stage value/option requests (criteria with
# __none__) pick "name" or "Blue".

JEV_PORT=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')
python3 - "$JEV_PORT" >"$TMP/jev-stub.log" 2>&1 <<'PY' &
import json, sys
from http.server import BaseHTTPRequestHandler, HTTPServer

class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        state = body.get("state", "")
        first = state.split("\n")[0].lower()
        criteria = body["questions"]["element"]["criteria"]
        conf = 0.91
        if "__none__" in criteria:
            # second-stage act request: which value to type / option to pick
            choice = "name" if "name" in criteria else "Blue"
            answers = {"element": {"choice": choice, "confidence": conf,
                                   "probabilities": {choice: conf}}}
        elif "goal_met" in body["questions"]:
            # act loop step: one Choice plus goal-met / stuck Nouls
            order = sorted((k for k in criteria if k.startswith("e")),
                           key=lambda k: int(k[1:]))
            descs = {k: str(criteria[k]).lower() for k in order}
            if "color" in first or "blue" in first:
                marker = "color"
            elif "1. type" in state:
                marker = "do it"
            else:
                marker = "name"
            choice = next((k for k in order if marker in descs[k]), order[0])
            done = "act-done" in state.lower()
            stuck = "impossible" in first
            answers = {
                "element": {"choice": choice, "confidence": conf,
                            "probabilities": {choice: conf}},
                "goal_met": {"noul": 0.97 if done else 0.02},
                "stuck": {"noul": 0.97 if stuck else 0.02},
            }
        else:
            goal = first
            conf = 0.2 if "unsure" in goal else 0.91
            if "bogus" in goal:
                choice = "e999"
            else:
                marker = "email" if "email" in goal else "log in"
                order = sorted(criteria, key=lambda k: int(k[1:]))
                choice = next((k for k in order if marker in criteria[k].lower()), order[0])
            answers = {"element": {"choice": choice, "confidence": conf,
                                   "probabilities": {choice: conf}}}
        resp = json.dumps({
            "model": "jev-stub",
            "answers": answers,
            "usage": {"input_tokens": 1234},
        }).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(resp)))
        self.end_headers()
        self.wfile.write(resp)

    def log_message(self, *_):
        pass

print("ready", flush=True)
HTTPServer(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
PY
JEV_PID=$!
for _ in $(seq 1 40); do
  grep -q ready "$TMP/jev-stub.log" 2>/dev/null && break
  sleep 0.25
done
grep -q ready "$TMP/jev-stub.log" || { echo "jev stub failed to come up" >&2; exit 1; }

# The MCP reads these lazily, per jev: call — but set them before launch so
# the whole run sees them.
export TYPESAFE_API_KEY=smoke-key
export JEV_BASE_URL="http://127.0.0.1:$JEV_PORT"

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

# ---- jev: selectors (against the stub API above) ---------------------------

JEV_URL="data:text/html,<title>jev</title><button id=jev-a>Cancel</button><button id=jev-b>Log%20in</button><input id=jev-in placeholder=Email>"
check "jev: navigate to test page" "$(call navigate "$(printf '{"sessionId":"%s","url":"%s"}' "$SID" "$JEV_URL")")" 'Navigated to'

# A page handler proves the trusted click really landed on the picked button.
check "jev: wire a click handler" "$(call evaluate "$(printf '{"sessionId":"%s","expression":"(() => { document.getElementById(%s).addEventListener(%s, () => { document.title = %s }); return 1 })()"}' "$SID" "'jev-b'" "'click'" "'clicked-jev'")")" '"result":1'

JEV_CLICK=$(call click "$(printf '{"sessionId":"%s","selector":"jev: the Log in button"}' "$SID")")
check "jev: click resolves via the stub" "$JEV_CLICK" 'confidence 0.91'
check "jev: click names the picked element" "$JEV_CLICK" '#jev-b'
check "jev: click really clicked it" "$(call evaluate "$(printf '{"sessionId":"%s","expression":"document.title"}' "$SID")")" 'clicked-jev'

check "jev: type picks the email input" "$(call type "$(printf '{"sessionId":"%s","selector":"jev: the Email field","text":"hi@x.io"}' "$SID")")" 'Typed 7 chars into input #jev-in'
check "jev: type typed into the picked input" "$(call evaluate "$(printf '{"sessionId":"%s","expression":"document.getElementById(%s).value"}' "$SID" "'jev-in'")")" 'hi@x.io'

check "jev: unknown id fails closed" "$(call click "$(printf '{"sessionId":"%s","selector":"jev: bogus target"}' "$SID")")" 'not one of the offered'
check "jev: low confidence fails closed" "$(call click "$(printf '{"sessionId":"%s","selector":"jev: unsure target"}' "$SID")")" 'below the 0.50 floor'

# Auto-fallback: with TYPESAFE_API_KEY set, a selector that matches nothing
# as CSS is retried as a plain-language goal. Short timeout keeps the CSS
# wait quick.
check "auto: reset title" "$(call evaluate "$(printf '{"sessionId":"%s","expression":"document.title = 1"}' "$SID")")" '"result":1'
AUTO_CLICK=$(call click "$(printf '{"sessionId":"%s","selector":"the Log in button","timeout":250}' "$SID")")
check "auto: plain-language selector resolves via jev" "$AUTO_CLICK" 'jev fallback for'
check "auto: names the picked element" "$AUTO_CLICK" '#jev-b'
check "auto: click really clicked it" "$(call evaluate "$(printf '{"sessionId":"%s","expression":"document.title"}' "$SID")")" 'clicked-jev'
check "auto: type falls back too" "$(call type "$(printf '{"sessionId":"%s","selector":"the Email field","text":"auto@x.io","timeout":250}' "$SID")")" 'Typed 9 chars into input #jev-in'
check "auto: fail-closed when jev is unsure" "$(call click "$(printf '{"sessionId":"%s","selector":"the unsure control","timeout":250}' "$SID")")" 'below the 0.85 floor'

# ---- act: the goal loop (against the stub API above) ------------------------
# One page, three goals: type+submit with `values`, set a <select> via a
# second-stage option Choice, and a stuck goal that must stop without acting.
ACT_URL="data:text/html,<title>act</title><button id=act-btn>Do%20it</button><input id=act-name placeholder=Your%20name><select id=color-sel><option>Red</option><option selected>Green</option><option>Blue</option></select>"
check "act: navigate to the loop page" "$(call navigate "$(printf '{"sessionId":"%s","url":"%s"}' "$SID" "$ACT_URL")")" 'Navigated to'
check "act: wire page handlers" "$(call evaluate "$(printf '{"sessionId":"%s","expression":"(() => { document.getElementById(%s).addEventListener(%s, () => { document.title = %s; window.__name = document.getElementById(%s).value }); document.getElementById(%s).addEventListener(%s, () => { document.title = %s }); return 1 })()"}' "$SID" "'act-btn'" "'click'" "'act-done'" "'act-name'" "'color-sel'" "'change'" "'act-done2'")")" '"result":1'

ACT_OUT=$(call act "$(printf '{"sessionId":"%s","goal":"type the name and submit","values":{"name":"Ada"}}' "$SID")")
check "act: completes with values in two steps" "$ACT_OUT" 'Completed the goal in 2 steps'
check "act: typed the provided value into the field" "$(call evaluate "$(printf '{"sessionId":"%s","expression":"window.__name"}' "$SID")")" '"Ada"'
check "act: the button really fired" "$(call evaluate "$(printf '{"sessionId":"%s","expression":"document.title"}' "$SID")")" 'act-done'

check "act: reset title between goals" "$(call evaluate "$(printf '{"sessionId":"%s","expression":"document.title = %s"}' "$SID" "'reset'")")" 'reset'
check "act: sets a select option via a second choice" "$(call act "$(printf '{"sessionId":"%s","goal":"choose the blue color"}' "$SID")")" 'Completed the goal in 1 step'
check "act: the select really changed" "$(call evaluate "$(printf '{"sessionId":"%s","expression":"document.title"}' "$SID")")" 'act-done2'
check "act: reset title before the stuck goal" "$(call evaluate "$(printf '{"sessionId":"%s","expression":"document.title = %s"}' "$SID" "'reset'")")" 'reset'

check "act: a stuck goal stops without acting" "$(call act "$(printf '{"sessionId":"%s","goal":"do an impossible thing"}' "$SID")")" 'Stuck after 0 steps'

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
