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
    assert!(
        text.contains("\"$id\""),
        "schema should contain a $id field"
    );
}
