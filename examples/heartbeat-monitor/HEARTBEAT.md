# Heartbeat Loop

1. Poll for mention work.
2. If there is no work, return `HEARTBEAT_OK`.
3. For each claimed mention, produce one final response.
4. Respond exactly once on success.
5. Release exactly once on failure.
