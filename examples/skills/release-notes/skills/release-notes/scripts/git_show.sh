#!/bin/sh
# Show details of a specific commit including its diff.
# Input: TOOL_INPUT JSON with 'ref' field.
set -eu

python3 - <<'EOF'
import json, os, subprocess, sys

inp = os.environ.get('TOOL_INPUT', '{}')
try:
    d = json.loads(inp) if inp.strip() else {}
except Exception as e:
    print(json.dumps({"error": f"invalid JSON input: {e}"}))
    sys.exit(1)

ref = d.get('ref', '')
if not ref:
    print(json.dumps({"error": "missing required field: ref"}))
    sys.exit(1)

result = subprocess.run(
    ['git', 'show', '--stat', ref],
    capture_output=True, text=True
)
if result.returncode != 0:
    print(json.dumps({"error": result.stderr.strip()}))
    sys.exit(1)

print(result.stdout)
EOF
