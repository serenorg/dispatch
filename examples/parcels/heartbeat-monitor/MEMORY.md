## Durable Patterns

Record patterns here after a failure mode is observed more than once.

Format:
- **Namespace:** `patterns`
- **Key:** short failure description (e.g., `respond-timeout`, `release-404`)
- **Value:** what was attempted, what failed, and what the current workaround is

## Operational notes

This file is seeded empty. The agent writes to it over time using `memory_put`.

Do not record individual mention content. Do not record one-off failures. Only record patterns that have recurred and are worth tracking across runs.
