use std::{fs, path::PathBuf};

use dispatch_core::{BuildOptions, PARCEL_FORMAT_VERSION, PARCEL_SCHEMA_URL, build_agent};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate dir should have workspace parent")
        .parent()
        .expect("workspace root should exist")
        .to_path_buf()
}

#[test]
fn current_repo_schema_matches_manifest_constants() {
    let root = workspace_root();
    let source = root.join("schemas/parcel.v2.json");
    let text = fs::read_to_string(&source).expect("failed to read repo schema");
    let schema: serde_json::Value =
        serde_json::from_str(&text).expect("repo schema should be valid JSON");

    assert_eq!(
        schema.get("$id").and_then(serde_json::Value::as_str),
        Some(PARCEL_SCHEMA_URL),
        "schema $id should match the published parcel schema URL"
    );
    assert_eq!(
        schema
            .get("properties")
            .and_then(|properties| properties.get("$schema"))
            .and_then(|schema_property| schema_property.get("const"))
            .and_then(serde_json::Value::as_str),
        Some(PARCEL_SCHEMA_URL),
        "manifest $schema property should require the published parcel schema URL"
    );
    assert_eq!(
        schema
            .get("required")
            .and_then(serde_json::Value::as_array)
            .map(|required| {
                required
                    .iter()
                    .any(|entry| entry.as_str() == Some("format_version"))
            }),
        Some(true),
        "schema should require format_version"
    );
    assert_eq!(
        schema
            .get("properties")
            .and_then(|properties| properties.get("format_version"))
            .and_then(|format_version| format_version.get("const"))
            .and_then(serde_json::Value::as_u64),
        Some(u64::from(PARCEL_FORMAT_VERSION)),
        "schema should require the exact supported format_version"
    );
}

#[test]
fn historical_v1_schema_remains_published() {
    let source = workspace_root().join("schemas/parcel.v1.json");
    let text = fs::read_to_string(&source).expect("failed to read historical v1 schema");
    let schema: serde_json::Value =
        serde_json::from_str(&text).expect("historical v1 schema should be valid JSON");

    assert_eq!(
        schema.get("$id").and_then(serde_json::Value::as_str),
        Some("https://serenorg.github.io/dispatch/schemas/parcel.v1.json")
    );
    assert!(
        schema
            .get("required")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|required| required
                .iter()
                .any(|entry| entry.as_str() == Some("source_agentfile")))
    );
}

#[test]
fn built_manifest_validates_against_current_repo_schema() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("dispatch.toml");
    fs::write(
        &config_path,
        "[agent]\ncourier_reference = \"native\"\nentrypoint = \"chat\"\n",
    )
    .unwrap();
    let built = build_agent(
        &config_path,
        &BuildOptions {
            output_root: dir.path().join("parcels"),
        },
    )
    .unwrap();
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(built.manifest_path).unwrap()).unwrap();
    let schema: serde_json::Value =
        serde_json::from_slice(&fs::read(workspace_root().join("schemas/parcel.v2.json")).unwrap())
            .unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let errors = validator
        .iter_errors(&manifest)
        .map(|error| error.to_string())
        .collect::<Vec<_>>();

    assert!(errors.is_empty(), "schema validation errors: {errors:?}");
}
