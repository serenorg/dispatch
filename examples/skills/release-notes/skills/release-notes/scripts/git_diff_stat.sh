#!/bin/sh
# Get a summary of files changed between two refs.
# Input: TOOL_INPUT JSON with 'from' and 'to' fields.
set -eu

python3 - <<'EOF'
import json, os, subprocess, sys

inp = os.environ.get('TOOL_INPUT', '{}')
try:
    d = json.loads(inp) if inp.strip() else {}
except Exception as e:
    print(json.dumps({"error": f"invalid JSON input: {e}"}))
    sys.exit(1)

from_ref = d.get('from', 'HEAD~1')
to_ref = d.get('to', 'HEAD')

result = subprocess.run(
    ['git', 'diff', '--stat', f"{from_ref}..{to_ref}"],
    capture_output=True, text=True
)
if result.returncode != 0:
    print(json.dumps({"error": result.stderr.strip()}))
    sys.exit(1)

print(result.stdout)
EOF
