#!/bin/sh
# Append an alert entry to the alerts log file.
# Input: TOOL_INPUT JSON with 'message' field and optional 'level' (info, warn, error).
set -eu

python3 - <<'EOF'
import json, os, sys
from datetime import datetime, timezone

inp = os.environ.get('TOOL_INPUT', '{}')
try:
    d = json.loads(inp) if inp.strip() else {}
except Exception as e:
    print(json.dumps({"error": f"invalid JSON input: {e}"}))
    sys.exit(1)

message = d.get('message', '').strip()
if not message:
    print(json.dumps({"error": "missing required field: message"}))
    sys.exit(1)

level = d.get('level', 'info').upper()
alerts_path = os.environ.get('ALERTS_PATH', '/tmp/dispatch-log-watcher-alerts.log')

timestamp = datetime.now(timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')
entry = f"[{timestamp}] [{level}] {message}\n"

os.makedirs(os.path.dirname(os.path.abspath(alerts_path)), exist_ok=True)
with open(alerts_path, 'a') as f:
    f.write(entry)

print(json.dumps({"written": True, "path": alerts_path, "entry": entry.strip()}))
EOF
