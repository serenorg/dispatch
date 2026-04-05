use super::*;

#[test]
fn builtin_mounts_scope_session_state_per_session_and_memory_per_parcel() {
    let test_image = build_test_image(
        "\
FROM dispatch/native:latest
MOUNT SESSION sqlite
MOUNT MEMORY sqlite
MOUNT ARTIFACTS local
ENTRYPOINT chat
",
        &[],
    );
    let courier = NativeCourier::default();
    let first_session =
        futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();
    let second_session =
        futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

    let first_session_db = mount_path(&first_session, MountKind::Session, "sqlite");
    let second_session_db = mount_path(&second_session, MountKind::Session, "sqlite");
    let first_memory_db = mount_path(&first_session, MountKind::Memory, "sqlite");
    let second_memory_db = mount_path(&second_session, MountKind::Memory, "sqlite");
    let first_artifacts = mount_path(&first_session, MountKind::Artifacts, "local");
    let second_artifacts = mount_path(&second_session, MountKind::Artifacts, "local");

    assert_ne!(first_session_db, second_session_db);
    assert!(first_session_db.contains("/sessions/"));
    assert!(second_session_db.contains("/sessions/"));
    assert_eq!(first_memory_db, second_memory_db);
    assert!(first_memory_db.ends_with("memory.sqlite"));
    assert_eq!(first_artifacts, second_artifacts);
    assert!(first_artifacts.ends_with("artifacts"));
}

#[test]
fn builtin_mounts_use_explicit_state_root_for_custom_output_layouts() {
    let root = tempdir().unwrap();
    let output_root = root.path().join("pulled");
    let test_image = build_test_image_with_output_root(
        "\
FROM dispatch/native:latest
MOUNT SESSION sqlite
MOUNT MEMORY sqlite
MOUNT ARTIFACTS local
ENTRYPOINT chat
",
        &[],
        &output_root,
    );
    let courier = NativeCourier::default();
    let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

    let session_db = mount_path(&session, MountKind::Session, "sqlite");
    let memory_db = mount_path(&session, MountKind::Memory, "sqlite");
    let artifacts_dir = mount_path(&session, MountKind::Artifacts, "local");

    let expected_root = output_root
        .canonicalize()
        .unwrap()
        .join(".dispatch-state")
        .join(&test_image.image.config.digest);
    assert!(session_db.starts_with(expected_root.join("sessions").to_string_lossy().as_ref()));
    assert_eq!(
        memory_db,
        expected_root.join("memory.sqlite").to_string_lossy()
    );
    assert_eq!(
        artifacts_dir,
        expected_root.join("artifacts").to_string_lossy()
    );
}
