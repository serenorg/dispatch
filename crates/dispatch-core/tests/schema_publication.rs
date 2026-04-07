use std::{fs, path::PathBuf};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate dir should have workspace parent")
        .parent()
        .expect("workspace root should exist")
        .to_path_buf()
}

#[test]
fn repo_schema_is_readable() {
    let root = workspace_root();
    let source = root.join("schemas/parcel.v1.json");
    let text = fs::read_to_string(&source).expect("failed to read repo schema");
    let schema: serde_json::Value =
        serde_json::from_str(&text).expect("repo schema should be valid JSON");

    assert_eq!(
        schema.get("$id").and_then(serde_json::Value::as_str),
        Some("https://schema.dispatch.run/parcel.v1.json"),
        "schema $id should match the published parcel schema URL"
    );
    assert_eq!(
        schema
            .get("properties")
            .and_then(|properties| properties.get("$schema"))
            .and_then(|schema_property| schema_property.get("const"))
            .and_then(serde_json::Value::as_str),
        Some("https://schema.dispatch.run/parcel.v1.json"),
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
            .and_then(|format_version| format_version.get("minimum"))
            .and_then(serde_json::Value::as_i64),
        Some(1),
        "schema should constrain format_version to 1 or newer"
    );
}
