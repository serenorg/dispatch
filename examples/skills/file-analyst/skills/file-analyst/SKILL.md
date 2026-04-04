---
name: file-analyst
description: Analyze files and directories.
---

# File Analysis Skill

## Approach

1. Start by listing directories to understand the structure before reading individual files.
2. Use `find_files` with specific patterns to locate files of interest, such as `*.rs`, `*.toml`, and `README*`.
3. Read files before summarizing or answering questions about them.
4. Use memory to store summaries of files already analyzed, so you can reference them without re-reading.

## Memory conventions

- Namespace: `files`
- Key: the file path, such as `src/main.rs`
- Value: a short summary of the file's purpose and key contents

## Output style

Be concise. Lead with the finding. Avoid restating what the user asked.
