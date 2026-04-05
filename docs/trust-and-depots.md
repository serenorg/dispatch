# Trust, Provenance, and Depot Policy

Dispatch separates three related but different concerns:

- integrity: does the pulled parcel still match its manifest and packaged file hashes?
- publisher identity: was this parcel signed by a key you trust for this repository or reference?
- provenance metadata: what framework or toolchain produced the parcel?

These signals should not be conflated.

## What Dispatch Verifies Today

At pull time, Dispatch can enforce:

- parcel integrity by recomputing the manifest digest and packaged file hashes
- detached Ed25519 signature verification with explicit `--public-key` inputs
- trust-policy-driven signature verification with `--trust-policy` or `DISPATCH_TRUST_POLICY`
- depot-reference matching so policy can apply to only part of a depot namespace

Important behavior:

- pulled parcels are verified in staging before they are committed into the local parcel store
- `dispatch depot pull --public-key ...` adds explicit trusted keys for that command
- `dispatch depot pull --trust-policy ...` resolves public keys and signature requirements from matching rules
- explicit `--public-key` values compose with trust-policy keys instead of replacing them

## What `FRAMEWORK` Means

`FRAMEWORK` is provenance metadata, not a trust primitive.

Use it to record facts like:

- which framework authored the parcel
- which toolchain version produced it
- which target the parcel was built for

Do not use `FRAMEWORK` as proof that a parcel is safe to run.

`FRAMEWORK` does not:

- prove who signed or published the parcel
- prove reproducibility
- authorize tools, secrets, or mounts
- replace signature verification or trust policy

Treat it as operator-visible metadata that can help with debugging, routing, auditing, or inventory.

## Depot Policy

### File Depots

`file://` depots are a convenience transport.

Recommended policy:

- use them for local development, air-gapped transfer, or controlled internal workflows
- do not treat local filesystem access as a substitute for parcel signatures
- still sign parcels if the depot is shared across users or automation boundaries

### HTTP Depots

`http://` and `https://` depots are distribution endpoints.

Recommended policy:

- prefer `https://` for any non-local deployment
- use `DISPATCH_DEPOT_TOKEN` or an equivalent operator secret for depot authentication
- treat depot auth as transport authorization, not as publisher identity
- require signatures for parcels that cross organizational or trust boundaries

Bearer auth answers "may this client fetch from this depot?" It does not answer "should I trust this publisher?" Signature verification and trust policy answer the second question.

## Trust Policy Recommendations

Trust policy rules match on depot references and repository prefixes, then compose:

- `require_signatures` becomes true if any matching rule requires signatures
- `public_keys` from all matching rules are merged and deduplicated
- signature verification succeeds if any one of the merged keys verifies the parcel; matching rules do not require all merged keys to verify

Recommended patterns:

### Internal Depot

- match a private repository prefix such as `acme/internal/`
- require signatures
- distribute the expected public keys with your deployment tooling
- keep key ownership narrow enough that one team does not implicitly authorize every repository

### Third-Party or Marketplace-Style Depot

- require signatures for every matched repository
- pin specific publisher keys per repository or per namespace
- avoid broad wildcard trust where one key authorizes unrelated publishers
- review `FRAMEWORK` and labels as metadata only after signature policy passes

### Local Development

- explicit `--public-key` is often enough for one-off verification
- unsigned pulls are acceptable only when the operator already controls both the parcel source and the local execution boundary

## Publisher Guidance

If you publish parcels for others to consume:

1. Build the parcel.
2. Run `dispatch parcel verify` locally before signing.
3. Sign with a stable Ed25519 key id dedicated to that publisher or release channel.
4. Push the parcel to the depot.
5. Publish the matching public key and the trust-policy rule consumers should use.

Recommended key hygiene:

- use distinct signing keys for distinct publishers or release channels
- rotate keys intentionally and keep old public keys available long enough for old parcels to verify
- avoid sharing one signing key across unrelated repositories if repository-level trust separation matters

## Suggested Operator Profiles

### Single-Team Internal Use

- private HTTPS depot
- `DISPATCH_DEPOT_TOKEN` set in automation
- trust policy pinned to repository prefixes and internal public keys
- signatures required for CI and production pulls

### Third-Party Consumption

- signatures required on every pull
- trust policy checked in alongside deployment config
- repository-specific key pinning
- optional additional review of provenance metadata and labels before execution approval

### Local Experimentation

- file depot or direct local parcel paths
- signatures optional
- no broad trust-policy assumptions carried into production

## Current Limits

Dispatch does not yet provide:

- signed tag metadata separate from parcel signatures
- transparency logs
- reproducible-build attestations
- a full PKI or certificate-based publisher identity system
- a revocation or replay-prevention mechanism that can invalidate a signed parcel already pulled into a local cache

Those may be added later, but the current safe baseline is:

- verify integrity
- require signatures where trust boundaries exist
- scope trust policy narrowly
- treat provenance metadata as advisory, not authoritative
