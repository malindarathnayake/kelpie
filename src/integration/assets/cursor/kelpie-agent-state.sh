#!/bin/sh
# managed by kelpie; reinstalling the integration replaces this file.
# KELPIE_INTEGRATION_ID=cursor
# KELPIE_INTEGRATION_VERSION=2

if [ "${1:-}" = "prompt" ]; then
    # Cursor must receive its allow response even when reporting is unavailable.
    exec 2>/dev/null
    hook_input_file=""
    trap '[ -z "$hook_input_file" ] || rm -f "$hook_input_file"; printf "%s\n" "{\"continue\":true}"; exit 0' 0
    trap 'exit 0' HUP INT TERM
    [ "${KELPIE_ENV:-}" = "1" ] || exit 0
    [ -n "${KELPIE_PANE_ID:-}" ] || exit 0

    hook_input_file="$(mktemp "${TMPDIR:-/tmp}/kelpie-cursor-hook.XXXXXX")" || exit 0
    cat >"$hook_input_file" || exit 0
    # Unlink the raw input before starting any reporting/parser child.
    exec 3<"$hook_input_file"
    rm -f "$hook_input_file" || exit 0
    hook_input_file=""

    if command -v python3 >/dev/null 2>&1; then
        python3 -c '
import os
import re
import signal
import subprocess
import sys
import time

deadline = time.monotonic() + 5

def expired(signum, frame):
    raise TimeoutError()

signal.signal(signal.SIGALRM, expired)
signal.setitimer(signal.ITIMER_REAL, 5)

def report(arguments, raw=None):
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        return
    try:
        subprocess.run(
            [os.environ.get("KELPIE_BIN_PATH") or "kelpie", "pane"] + arguments,
            input=raw, stdin=subprocess.DEVNULL if raw is None else None,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            timeout=remaining, check=False,
        )
    except Exception:
        pass

# Decode and validate in a private child: C-backed parsing can defer Python signals.
metadata_script = r"""
import json
import re
import sys

def reject_constant(value):
    raise ValueError("invalid JSON constant")

metadata = sys.stdin.buffer.read().decode("utf-8", errors="strict")
if metadata.startswith("\ufeff"):
    metadata = metadata[1:]
payload = json.loads(metadata, parse_constant=reject_constant)
if not isinstance(payload, dict) or payload.get("hook_event_name") != "beforeSubmitPrompt":
    raise SystemExit(0)
session_id = payload.get("conversation_id")
if isinstance(session_id, str) and re.fullmatch(r"[A-Za-z0-9_][A-Za-z0-9_-]{0,255}", session_id):
    sys.stdout.buffer.write(session_id.encode("ascii"))
"""

try:
    raw = sys.stdin.buffer.read()
    pane_id = os.environ["KELPIE_PANE_ID"]
    report(["report-prompt-submitted", pane_id, "--source", "kelpie:cursor", "--agent", "cursor"], raw)
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise SystemExit(0)
    # The same deadline covers child input, parsing, exit, and kill-and-wait cleanup.
    metadata_result = subprocess.run(
        [sys.executable, "-c", metadata_script], input=raw,
        stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
        timeout=remaining, check=False,
    )
    if metadata_result.returncode == 0 and re.fullmatch(
        rb"[A-Za-z0-9_][A-Za-z0-9_-]{0,255}", metadata_result.stdout,
    ):
        session_id = metadata_result.stdout.decode("ascii")
        seq = str(time.time_ns())
        report(["report-agent-session", pane_id, "--source", "kelpie:cursor", "--agent", "cursor",
                "--seq", seq, "--agent-session-id", session_id, "--session-start-source", "prompt"])
except Exception:
    pass
finally:
    signal.setitimer(signal.ITIMER_REAL, 0)
' <&3 3<&- >/dev/null 2>&1 || true
    else
        # Metadata tooling is optional; still forward raw bytes without it.
        "${KELPIE_BIN_PATH:-kelpie}" pane report-prompt-submitted "$KELPIE_PANE_ID" \
            --source kelpie:cursor --agent cursor <&3 3<&- >/dev/null 2>&1 &
        report_pid=$!
        ( sleep 5; kill -KILL "$report_pid" 2>/dev/null ) >/dev/null 2>&1 &
        watchdog_pid=$!
        wait "$report_pid" 2>/dev/null || true
        kill "$watchdog_pid" 2>/dev/null || true
        wait "$watchdog_pid" 2>/dev/null || true
    fi
    exec 3<&-
    exit 0
fi

[ "${1:-}" = "session" ] || exit 0
[ "${KELPIE_ENV:-}" = "1" ] || exit 0
[ -n "${KELPIE_SOCKET_PATH:-}" ] || exit 0
[ -n "${KELPIE_PANE_ID:-}" ] || exit 0
command -v python3 >/dev/null 2>&1 || exit 0

python3 -c '
import json
import os
import socket
import sys
import time

try:
    payload = json.load(sys.stdin)
except Exception:
    raise SystemExit(0)

def first_text(*names):
    for name in names:
        value = payload.get(name)
        if isinstance(value, str) and value:
            return value
    return None

event = first_text("hook_event_name", "hookEventName")
if event not in (None, "sessionStart"):
    raise SystemExit(0)

session_id = first_text("session_id", "sessionId", "conversation_id", "conversationId")
if session_id is None:
    raise SystemExit(0)

seq = time.time_ns()
request = json.dumps({
    "id": f"kelpie:cursor:{seq}",
    "method": "pane.report_agent_session",
    "params": {
        "pane_id": os.environ["KELPIE_PANE_ID"],
        "source": "kelpie:cursor",
        "agent": "cursor",
        "seq": seq,
        "agent_session_id": session_id,
    },
})
try:
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
        client.settimeout(0.5)
        client.connect(os.environ["KELPIE_SOCKET_PATH"])
        client.sendall((request + "\n").encode())
        client.recv(4096)
except Exception:
    pass
' 2>/dev/null || true
