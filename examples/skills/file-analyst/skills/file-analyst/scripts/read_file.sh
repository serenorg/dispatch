#!/bin/sh
# Read the contents of a file.
# Input: TOOL_INPUT JSON with 'path' field.
set -eu

PATH_VAL=$(python3 -c "
import json, os, sys
inp = os.environ.get('TOOL_INPUT', '{}')
try:
    d = json.loads(inp)
    print(d.get('path', ''))
except Exception as e:
    print('', end='')
")

if [ -z "$PATH_VAL" ]; then
    echo '{"error": "missing required field: path"}' >&2
    exit 1
fi

if [ ! -e "$PATH_VAL" ]; then
    echo "{\"error\": \"path not found: $PATH_VAL\"}" >&2
    exit 1
fi

if [ ! -f "$PATH_VAL" ]; then
    echo "{\"error\": \"not a file: $PATH_VAL\"}" >&2
    exit 1
fi

cat "$PATH_VAL"
