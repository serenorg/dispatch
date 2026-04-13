# Examples

- `parcels/` contains general Dispatch parcel and courier examples.
- `runtime/` contains project-level `dispatch.toml` examples.
- `skills/` contains Agent Skills-compatible bundle examples.
- `parcels/codex/` is the minimal Codex backend example. It is useful for
  manually testing `PROVIDER codex` without mixing in built-in tools or other
  runtime features.

The skill examples intentionally keep the skill bundle under `skills/<name>/` inside each example so the Agent Skills frontmatter `name` matches the immediate bundle directory.
