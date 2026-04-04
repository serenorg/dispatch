---
name: log-watcher
description: Watch a log file on each heartbeat and emit alerts for new issues.
---

# Log Watching Skill

## On each heartbeat

1. Retrieve the last-seen line number from memory: namespace `state`, key `last_line`. Default to 0 if not set.
2. Get the total line count of the log file using `line_count`.
3. If the total line count equals last_line, there are no new entries, so exit cleanly.
4. Read new log lines using `read_log` with `since_line` set to the last-seen line number.
5. Scan the new lines for: ERROR, FATAL, WARN, panic, exception, timeout, connection refused, out of memory.
6. For each distinct issue found, call `write_alert` once.
7. Save the new line count to memory: namespace `state`, key `last_line`.

## Alert classification

- `error` level: ERROR, FATAL, panic, exception, out of memory
- `warn` level: WARN, timeout, connection refused, retry
- `info` level: anything else worth noting

## Do not alert on

- Normal startup messages
- Routine request/response log lines
- Issues already present at the previous check; compare against memory if needed

## Memory conventions

- namespace: `state`, key: `last_line` for the last line number fully processed
- namespace: `seen`, key: `<issue_hash>` for a short hash of alert messages already sent
