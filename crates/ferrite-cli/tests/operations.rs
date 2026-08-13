use serde_json::Value;
use std::fs;
use std::process::Command;

fn temp_path(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("ferrite-ops-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    let _ = fs::remove_file(&path);
    path
}

fn ferrite(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_ferrite"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn verify_and_backup_do_not_create_a_missing_source() {
    let missing = temp_path("missing");
    let backup = temp_path("missing-backup");

    assert!(
        !ferrite(&["verify", missing.to_str().unwrap()])
            .status
            .success()
    );
    assert!(!missing.exists());
    assert!(
        !ferrite(&[
            "backup",
            missing.to_str().unwrap(),
            backup.to_str().unwrap()
        ])
        .status
        .success()
    );
    assert!(!missing.exists());
    assert!(!backup.exists());
}

#[test]
fn failed_import_removes_its_partial_destination() {
    let imported = temp_path("failed-import");
    let input = temp_path("malformed.jsonl");
    let mut contents = String::new();
    for index in 0..ferrite_core::MAX_TRANSACTION_OPERATIONS {
        contents.push_str(&format!("{{\"key\":\"key-{index}\",\"value\":{index}}}\n"));
    }
    contents.push_str("{not-json}\n");
    fs::write(&input, contents).unwrap();

    assert!(
        !ferrite(&[
            "import",
            imported.to_str().unwrap(),
            input.to_str().unwrap()
        ])
        .status
        .success()
    );
    assert!(!imported.exists());

    let _ = fs::remove_file(input);
}

#[test]
fn jsonl_round_trip_preserves_schema_constraints() {
    let db = temp_path("schema-db");
    let imported = temp_path("schema-imported");
    let export = temp_path("schema-export.jsonl");
    let schema = serde_json::json!({
        "collections": {"users": {"primary_key": "id", "unique": ["email"]}}
    });
    {
        let mut database = ferrite_core::Database::open_with_schema(&db, &schema).unwrap();
        database
            .put(
                "users",
                "u1",
                serde_json::json!({"id":"u1","email":"same@example.com"}),
            )
            .unwrap();
    }

    assert!(
        ferrite(&["export", db.to_str().unwrap(), export.to_str().unwrap()])
            .status
            .success()
    );
    assert!(
        ferrite(&[
            "import",
            imported.to_str().unwrap(),
            export.to_str().unwrap()
        ])
        .status
        .success()
    );

    let mut database = ferrite_core::Database::open_with_schema(&imported, &schema).unwrap();
    let duplicate = database.put(
        "users",
        "u2",
        serde_json::json!({"id":"u2","email":"same@example.com"}),
    );
    assert!(matches!(
        duplicate,
        Err(ferrite_core::Error::UniqueViolation { .. })
    ));
    drop(database);

    let _ = fs::remove_dir_all(db);
    let _ = fs::remove_dir_all(imported);
    let _ = fs::remove_file(export);
}

#[test]
fn backup_restore_and_jsonl_round_trip_without_overwriting() {
    let db = temp_path("db");
    let backup = temp_path("backup");
    let restored = temp_path("restored");
    let imported = temp_path("imported");
    let export = temp_path("export.jsonl");

    assert!(
        ferrite(&["put", db.to_str().unwrap(), "users/1", r#"{"name":"Ada"}"#])
            .status
            .success()
    );
    assert!(
        ferrite(&["backup", db.to_str().unwrap(), backup.to_str().unwrap()])
            .status
            .success()
    );
    assert!(
        ferrite(&[
            "restore",
            backup.to_str().unwrap(),
            restored.to_str().unwrap()
        ])
        .status
        .success()
    );

    let restored_value = ferrite(&["get", restored.to_str().unwrap(), "users/1"]);
    assert!(restored_value.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&restored_value.stdout).unwrap(),
        serde_json::json!({"name":"Ada"})
    );

    assert!(
        ferrite(&["export", db.to_str().unwrap(), export.to_str().unwrap()])
            .status
            .success()
    );
    assert!(
        ferrite(&[
            "import",
            imported.to_str().unwrap(),
            export.to_str().unwrap()
        ])
        .status
        .success()
    );
    let imported_value = ferrite(&["get", imported.to_str().unwrap(), "users/1"]);
    assert_eq!(
        serde_json::from_slice::<Value>(&imported_value.stdout).unwrap(),
        serde_json::json!({"name":"Ada"})
    );

    assert!(
        !ferrite(&["backup", db.to_str().unwrap(), backup.to_str().unwrap()])
            .status
            .success()
    );
    assert!(
        !ferrite(&[
            "restore",
            backup.to_str().unwrap(),
            restored.to_str().unwrap()
        ])
        .status
        .success()
    );
    assert!(
        !ferrite(&["export", db.to_str().unwrap(), export.to_str().unwrap()])
            .status
            .success()
    );
    assert!(
        !ferrite(&[
            "import",
            imported.to_str().unwrap(),
            export.to_str().unwrap()
        ])
        .status
        .success()
    );

    let _ = fs::remove_dir_all(db);
    let _ = fs::remove_dir_all(backup);
    let _ = fs::remove_dir_all(restored);
    let _ = fs::remove_dir_all(imported);
    let _ = fs::remove_file(export);
}
