# Schema Publication and Compatibility Policy

Dispatch publishes a JSON Schema for every supported parcel manifest format.

Current published schema:

- schema URL: `https://schema.dispatch.run/parcel.v1.json`
- canonical copy: [`schemas/parcel.v1.json`](../schemas/parcel.v1.json)
- current `format_version`: `1`

## Publication Model

Dispatch keeps one tracked copy of the current parcel schema in the repo:
The schema file is the single source of truth.
The schema file is the single source of truth.
The schema file is the single source of truth.
The schema file is the single source of truth.
The schema file is the single source of truth.
The schema file is the single source of truth.
The schema file is the single source of truth.

- `schemas/parcel.v1.json` is the canonical schema checked into the repo

## Compatibility Rules

Every built parcel manifest declares:

- a `$schema` URL
- an integer `format_version`

Courier compatibility rules:

- couriers must validate parcels against the schema URL and `format_version` they claim to support
- couriers must reject parcels whose `$schema` URL they do not recognize
- couriers must reject parcels whose `format_version` they do not support
- support for one schema URL does not imply support for future schema URLs

Reference implementation policy:

- the Dispatch reference implementation currently supports exactly `https://schema.dispatch.run/parcel.v1.json`
- the Dispatch reference implementation currently supports exactly `format_version: 1`

## Stability Promise

Published schema URLs are immutable release artifacts.

That means:

- once `parcel.v1.json` is published, its validation shape must not change in place
- clarifications that affect validation require a new schema file and a new `format_version`
- adding new required manifest fields requires a new schema file and a new `format_version`
- changing the meaning of an existing field in a way that could alter courier behavior requires a new schema file and a new `format_version`

Editorial documentation changes outside the schema file can still happen, but the schema payload at a published URL is treated as fixed.

## Versioning Guidance

Dispatch is still in a `v0.x` product stage, but published parcel schema versions are still treated as stable contracts.

Practical implications:

- the CLI and reference couriers may add features around the parcel format in `v0.x`
- manifest-shape changes must still go through a new schema URL and `format_version`
- third-party couriers should pin the Dispatch release range and schema versions they have validated, instead of assuming forward compatibility
- the lifetime of old `format_version` support inside the reference implementation is not yet a stability promise for `v0.x`, even though published schema files remain available by URL

## Release Process

When Dispatch introduces a new parcel manifest version:

1. Add a new tracked schema file under `schemas/`, for example `schemas/parcel.v2.json`.
2. Update the manifest constants and parser/build/courier support for the new schema URL and `format_version`.
3. Update the compatibility docs and any release notes to state which versions the reference implementation supports.
4. Keep old schema files published and loadable by URL even after newer versions exist.

Dispatch should not reuse an old schema URL for a new format.
