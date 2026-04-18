# Dispatch Plugin Ecosystem

Dispatch's extension model currently assumes a user already has the manifest
file in hand: `dispatch courier install ./courier-plugin.json` or
`dispatch channel install ./channel-plugin.json`. That works for repo-local
development but does not scale to a third-party ecosystem where plugins live
in many separate repositories (for example `dispatch-courier-seren-cloud`) and
where operators need a way to find and install them.

This document describes a four-tier roadmap for Dispatch's plugin discovery,
distribution, and trust model. The tiers are incremental: each one is useful
on its own and provides the foundation for the next.

- Tier 1 (v0.3.0): **Catalog sources** — Homebrew-tap style discovery
- Tier 2 (v0.3.0): **Install by name** — resolve, fetch, verify published binaries
- Tier 3 (vFuture): **Capability-based trust** — manifest-declared permissions
- Tier 4 (vFuture): **Official index** — curated registry, if warranted

The core principle across all tiers: manifest-first, decentralized, signed.
Dispatch should never execute plugin code to find out what a plugin is or
what it needs.

---

## Tier 1: Catalog sources

**Status:** implemented in v0.3.0.

### Problem

There is no command that answers "what plugins are available?" or "where is
the seren-cloud courier?" The only discovery paths are: already know the repo
URL, or read `dispatch-plugins/catalog/extensions.json` by hand.

### Design

A catalog is a JSON document at a stable URL that lists plugins with enough
metadata to drive discovery. The schema already exists in
`dispatch-plugins/catalog/extensions.json` and is now canonicalized in
`dispatch-core::catalog::ExtensionCatalog`.

Users register catalogs in `~/.config/dispatch/catalogs.toml`:

```toml
[[catalogs]]
name = "dispatch-plugins"
url  = "https://raw.githubusercontent.com/serenorg/dispatch-plugins/master/catalog/extensions.json"

[[catalogs]]
name = "seren-cloud"
url  = "https://raw.githubusercontent.com/serenorg/dispatch-courier-seren-cloud/main/catalog/extensions.json"
```

Fetched catalogs are cached as JSON under
`~/.config/dispatch/catalog-cache/<name>.json`. Refresh is explicit
(`dispatch extension catalog refresh`), not automatic on every search.

### Commands

- `dispatch extension catalog add <url> [--name <name>]` — register a catalog
- `dispatch extension catalog ls [--json]` — list registered catalogs
- `dispatch extension catalog rm <name>` — remove a catalog
- `dispatch extension catalog refresh [<name>]` — re-fetch one or all catalogs
- `dispatch extension search <query> [--kind channel|courier] [--json]`
  — search catalog entries by name, description, or tag
- `dispatch extension show <name> [--json]` — show full entry for a plugin

### Catalog schema v1

Already shipped in `dispatch-plugins/catalog/extensions.json`. Each entry has:

- `name` — catalog-unique identifier (e.g. `channel-telegram`)
- `display_name`, `description`, `version`
- `kind` — `channel`, `courier`, or `connector`
- `protocol` / `protocol_version` — wire format and version
- `manifest_path` OR `manifest_url` — relative-to-catalog path or absolute URL
- `source_dir` — relative-to-catalog source tree (optional)
- `keywords`, `tags` — for search
- `install_hint` — human-readable install string for the current Tier 1 flow
- `auth` — `{ method, provider, setup_url? }`
- `requirements` — `{ secrets[], optional_secrets[], network_domains[], platforms[] }`

Everything above is declarative. Dispatch never executes plugin code to
populate or validate a catalog entry.

### Publishing a catalog

Any repository that hosts Dispatch extensions can publish a catalog by
committing a JSON document that matches the schema above and exposing it at a
stable URL.

Conventions used by existing catalogs:

- Place the catalog at `catalog/extensions.json` in the repo root so GitHub's
  raw file URL (`https://raw.githubusercontent.com/<owner>/<repo>/<branch>/catalog/extensions.json`)
  is stable.
- List every extension the repo publishes as a separate entry. Single-plugin
  repos ship a catalog with one entry.
- Set `manifest_path` relative to the catalog file, or `manifest_url` if the
  manifest is served from a different host.
- Keep `install_hint` accurate for the Tier 1 flow (clone + build + `dispatch
  {courier,channel} install ...`) and self-contained enough to work when an
  operator copies it out of `dispatch extension show`. If install requires the
  repo root, include the `git clone` + `cd` steps explicitly. If the entry also
  publishes a Tier 2 `source` block, `install_hint` becomes advisory rather
  than required.

Third-party authors do not need to coordinate with Dispatch to publish a
catalog. Users add the catalog URL with `dispatch extension catalog add <url>`
and the entries become discoverable.

### Known catalogs

These are the first-party and maintainer-published catalogs the Dispatch
project is aware of. Adding any of them is purely opt-in; Dispatch does not
implicitly register them.

| Catalog | URL |
|---|---|
| dispatch-plugins (channels) | `https://raw.githubusercontent.com/serenorg/dispatch-plugins/master/catalog/extensions.json` |
| dispatch-courier-seren-cloud | `https://raw.githubusercontent.com/serenorg/dispatch-courier-seren-cloud/main/catalog/extensions.json` |

To propose adding a catalog here, open a pull request against Dispatch with
the catalog URL and a one-line description.

### Out of scope for Tier 1

- Fetching the manifest or binary (Tier 2)
- Signature verification (Tier 3)
- Capability enforcement (Tier 3)

---

## Tier 2: Install by name

**Status:** implemented in v0.3.0 for GitHub release binaries.

### Problem

Even with discovery, a user still has to:

1. Read `install_hint`
2. `git clone` the repo
3. Build the plugin binary (or download a release asset manually)
4. Run `dispatch courier install path/to/manifest.json`
5. Hope that the manifest's `exec.command` resolves on their system

Step 3 is particularly painful for 3rd parties — every plugin repo has its
own build toolchain expectations.

### Design

Extend catalog entries with a `source` block that describes how Dispatch can
acquire the binary itself. The shipped v0.3.0 path supports direct GitHub
release binaries.

**GitHub release (preferred — no toolchain needed):**

```json
"source": {
  "type": "github_release",
  "repo": "serenorg/dispatch-courier-seren-cloud",
  "tag": "v0.1.0",
  "checksum_asset": "SHA256SUMS.txt",
  "binaries": [
    {
      "target": "aarch64-apple-darwin",
      "asset": "dispatch-courier-seren-cloud-aarch64-apple-darwin",
      "binary_name": "dispatch-courier-seren-cloud"
    },
    {
      "target": "x86_64-unknown-linux-gnu",
      "asset": "dispatch-courier-seren-cloud-x86_64-unknown-linux-gnu",
      "binary_name": "dispatch-courier-seren-cloud"
    }
  ]
}
```

Each binary entry may either include an inline `sha256` or inherit its hash
from a release-level checksum asset such as `SHA256SUMS.txt`.

### New command

`dispatch extension install <name>`

Flow:

1. Resolve `name` via configured catalogs
2. Select the published `github_release` binary that matches the current host target
3. Download the asset and verify its SHA256
4. Stage the binary under `~/.config/dispatch/bin/<name>/<version>/`
5. Fetch the absolute `manifest_url` declared by the catalog entry and rewrite
  `exec.command` / `entrypoint.command` to the staged binary path
6. Call the existing `install_courier_plugin` / `install_channel_plugin`
  flow with the rewritten manifest

This removes the fragile relative `./target/release/...` manifest path that
third parties (including `dispatch-courier-seren-cloud`) currently have.

Catalog entries that want to participate in install-by-name should declare an
absolute, version-pinned `manifest_url`. Relative `manifest_path` remains fine
for Tier 1 discovery, but it is too loose for versioned binary installs.

### Trust at this tier

- SHA256 pinning on every asset, either inline per binary or through a
  release checksum asset such as `SHA256SUMS.txt`
- First-install prompt: "Install `courier-seren-cloud` v0.1.0 from
  `serenorg/dispatch-courier-seren-cloud`, sha256 `abc...`? [y/N]"
  suppressible via `--yes`
- Installed plugin registries keep the normal installed manifest metadata; the
  SHA256 check happens at install time before the staged binary is trusted

### Out of scope for Tier 2

- Alternate acquisition sources such as `cargo_git`
- Archive extraction for packaged tarballs or zip files
- Cryptographic signature verification beyond SHA256 (Tier 3)
- Runtime capability enforcement (Tier 3)
- Auto-updates (Tier 2 can ship with an explicit `extension upgrade`; auto is
  later)

---

## Tier 3: Capability-based trust

**Status:** future; largest architectural change.

### Problem

Even with pinned SHAs and a prompt, a plugin that gets installed can do
anything its manifest doesn't constrain — make arbitrary HTTP calls, read
arbitrary filesystem paths, read arbitrary environment variables. This is
the same footgun as shell-install curl pipes.

### Design

Adopt the ironclaw-style capability manifest: every plugin declares what it
will access, and Dispatch enforces those declarations at runtime.

```json
{
  "kind": "channel",
  "name": "channel-slack",
  "capabilities": {
    "http": {
      "allowlist": [
        { "host": "slack.com", "path_prefix": "/api/" },
        { "host": "hooks.slack.com" }
      ]
    },
    "secrets": {
      "allowed_names": ["SLACK_*"]
    },
    "ingress": {
      "allowed_paths": ["/slack/events"],
      "signature_secret_env": "SLACK_SIGNING_SECRET"
    },
    "rate_limits": {
      "outbound_per_minute": 120
    }
  }
}
```

Dispatch enforces these at the plugin-process boundary:

- **HTTP egress**: plugin makes outbound requests via a dispatch-managed proxy
  (or via explicit permission check for each connection). Requests outside
  the allowlist fail closed.
- **Secrets**: the set of env vars forwarded to the plugin subprocess is
  filtered to match `allowed_names` patterns.
- **Ingress**: the dispatch ingress router only delivers requests whose path
  matches `allowed_paths`.
- **Rate limits**: enforced in dispatch's delivery wrapper, not the plugin.

### Signed releases

Catalog entries gain a `signatures` block alongside `binaries`:

```json
"signatures": {
  "minisign_pubkey": "RWTxxx...",
  "signature_asset": "dispatch-courier-seren-cloud-v0.1.0.minisig"
}
```

Install flow verifies the signature over the release asset using the pubkey
from the catalog. Catalog itself is pinned when first added (`catalogs.toml`
records SHA256 of the first fetch for TOFU), rejecting silent replacement.

### Migration

- Plugins without a `capabilities` block are run in a permissive legacy mode
  with a one-line warning. v1.0 removes the legacy path.
- `dispatch extension inspect <name>` shows the declared capabilities so
  operators can review before install.

### Out of scope for Tier 3

- Sandboxing the plugin process (containerization, seccomp) — that's a
  separate and much larger investment. Capability enforcement at the
  dispatch-plugin boundary is the pragmatic middle ground.

---

## Tier 4: Official index

**Status:** only if the ecosystem demands it.

A hosted index at something like `registry.dispatch.sh` that mirrors approved
catalog URLs, serves a search API, and offers curated discovery. Analogous to
Docker Hub or the VS Code marketplace.

Do not build this prematurely. The decentralized catalog model in Tiers 1-3
answers the core question ("how does a user find `dispatch-courier-seren-cloud`")
without any central service. Build Tier 4 only if:

- The ecosystem reaches the point where a single search endpoint has clear
  value over catalog aggregation
- There is appetite (and budget) for ongoing moderation, takedowns, and
  uptime
- The trust model from Tier 3 is proven and the index is the natural place
  to enforce it at listing time

---

## Summary

| Tier | Scope | Answers |
|---|---|---|
| 1 | Catalog sources, search, show | "What plugins exist? Where is this one?" |
| 2 | Install by name, binary acquisition | "Install it for me" |
| 3 | Capability declarations, signed releases | "Should I trust it?" |
| 4 | Official index | "Browse curated plugins" |

The seren-cloud courier and any other third-party plugin becomes discoverable
as soon as it ships a `catalog/extensions.json` pointing at itself and
publishes the catalog URL. Users add the catalog once and everything below
works through `dispatch extension ...`.
