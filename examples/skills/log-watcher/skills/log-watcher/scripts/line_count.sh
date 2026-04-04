#!/bin/sh
# Get the total number of lines in a file.
# Input: TOOL_INPUT JSON with 'path' field.
set -eu

python3 - <<'EOF'
import json, os, sys

inp = os.environ.get('TOOL_INPUT', '{}')
try:
    d = json.loads(inp) if inp.strip() else {}
except Exception as e:
    print(json.dumps({"error": f"invalid JSON input: {e}"}))
    sys.exit(1)

path = d.get('path', os.environ.get('LOG_PATH', ''))
if not path:
    print(json.dumps({"error": "missing required field: path (or set LOG_PATH secret)"}))
    sys.exit(1)

if not os.path.isfile(path):
    print(json.dumps({"count": 0, "path": path, "exists": False}))
    sys.exit(0)

with open(path) as f:
    count = sum(1 for _ in f)

print(json.dumps({"path": path, "count": count, "exists": True}))
EOF
