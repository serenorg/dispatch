#!/bin/sh
# List the contents of a directory.
# Input: TOOL_INPUT JSON with optional 'path' field (defaults to current directory).
set -eu

DIR_VAL=$(python3 -c "
import json, os
inp = os.environ.get('TOOL_INPUT', '{}')
try:
    d = json.loads(inp)
    print(d.get('path', '.'))
except Exception:
    print('.')
")

if [ ! -d "$DIR_VAL" ]; then
    echo "{\"error\": \"directory not found: $DIR_VAL\"}" >&2
    exit 1
fi

ls -la "$DIR_VAL"
