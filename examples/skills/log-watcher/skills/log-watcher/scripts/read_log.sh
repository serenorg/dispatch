#!/bin/sh
# Read lines from a log file, optionally starting from a given line number.
# Input: TOOL_INPUT JSON with 'path' field and optional 'since_line' (0-indexed, exclusive).
# Returns up to 200 lines to avoid flooding context.
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
    print(json.dumps({"error": f"file not found: {path}"}))
    sys.exit(1)

since_line = int(d.get('since_line', 0))
max_lines = 200

with open(path) as f:
    all_lines = f.readlines()

total = len(all_lines)
new_lines = all_lines[since_line:since_line + max_lines]

output = {
    "path": path,
    "total_lines": total,
    "from_line": since_line,
    "returned_lines": len(new_lines),
    "truncated": len(all_lines[since_line:]) > max_lines,
    "content": "".join(new_lines),
}
print(json.dumps(output))
EOF
