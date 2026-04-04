#!/bin/sh
# Find files matching a pattern under a directory.
# Input: TOOL_INPUT JSON with 'dir' and 'pattern' fields.
# 'pattern' is a glob passed to find's -name flag (e.g. "*.rs", "*.toml", "README*").
# Optional 'max_depth' field (default: 6).
set -eu

python3 - <<'EOF'
import json, os, subprocess, sys

inp = os.environ.get('TOOL_INPUT', '{}')
try:
    d = json.loads(inp)
except Exception as e:
    print(json.dumps({"error": f"invalid JSON input: {e}"}))
    sys.exit(1)

directory = d.get('dir', '.')
pattern = d.get('pattern', '*')
max_depth = str(d.get('max_depth', 6))

if not os.path.isdir(directory):
    print(json.dumps({"error": f"directory not found: {directory}"}))
    sys.exit(1)

result = subprocess.run(
    ['find', directory, '-maxdepth', max_depth, '-type', 'f', '-name', pattern],
    capture_output=True, text=True
)

files = [line for line in result.stdout.splitlines() if line]
print(json.dumps({"files": files, "count": len(files)}))
EOF
