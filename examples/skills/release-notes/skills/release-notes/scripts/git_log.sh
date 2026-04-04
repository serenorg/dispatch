#!/bin/sh
# Get git commit log between two refs.
# Input: TOOL_INPUT JSON with optional 'from' and 'to' fields.
# If 'from' is omitted, returns the last 20 commits up to 'to' (default HEAD).
set -eu

python3 - <<'EOF'
import json, os, subprocess, sys

inp = os.environ.get('TOOL_INPUT', '{}')
try:
    d = json.loads(inp) if inp.strip() else {}
except Exception as e:
    print(json.dumps({"error": f"invalid JSON input: {e}"}))
    sys.exit(1)

to_ref = d.get('to', 'HEAD')
from_ref = d.get('from', '')

if from_ref:
    ref_range = f"{from_ref}..{to_ref}"
else:
    ref_range = to_ref

cmd = [
    'git', 'log', ref_range,
    '--pretty=format:%H %as %s',
    '--no-merges',
]

if not from_ref:
    cmd += ['-20']

result = subprocess.run(cmd, capture_output=True, text=True)
if result.returncode != 0:
    print(json.dumps({"error": result.stderr.strip()}))
    sys.exit(1)

commits = []
for line in result.stdout.splitlines():
    if not line.strip():
        continue
    parts = line.split(' ', 2)
    commits.append({
        "sha": parts[0][:12] if len(parts) > 0 else '',
        "date": parts[1] if len(parts) > 1 else '',
        "subject": parts[2] if len(parts) > 2 else '',
    })

print(json.dumps({"range": ref_range, "commits": commits, "count": len(commits)}))
EOF
