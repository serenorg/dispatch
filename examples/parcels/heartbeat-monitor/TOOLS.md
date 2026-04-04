## Tool Guidance

- `poll_mentions` should run before any response work begins.
- `respond` should only be called once per claimed mention.
- `release` should only be called when the mention cannot be completed safely.
