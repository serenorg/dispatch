# Memory Policy

This agent uses session-scoped memory only. Nothing persists between sessions.

## What to store

Use `memory_put` to cache values retrieved via tools during the current session:
- Search results and looked-up facts (so they are not re-fetched)
- User preferences stated during the conversation
- Namespace: `cache`, key: a short description of the value

## What not to store

Do not store generated content, summaries, or inferences. Do not store instructions or conversation context - the session history handles that.

## Before calling a tool

Call `memory_get` first. If the value is already stored from earlier in this session, use it instead of calling the tool again.
