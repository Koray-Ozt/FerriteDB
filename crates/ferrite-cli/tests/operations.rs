use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

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
fn failed_import_does_not_delete_a_replacement_destination() {
    let imported = temp_path("replaced-import");
    let input = temp_path("replacement-race.jsonl");
    let marker = imported.join("do-not-delete");
    let mut contents = String::new();
    for index in 0..20_000 {
        contents.push_str(&format!("{{\"key\":\"key-{index}\",\"value\":{index}}}\n"));
    }
    contents.push_str("{not-json}\n");
    fs::write(&input, contents).unwrap();

    let child = Command::new(env!("CARGO_BIN_EXE_ferrite"))
        .args([
            "import",
            imported.to_str().unwrap(),
            input.to_str().unwrap(),
        ])
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let parent = imported.parent().unwrap().to_path_buf();
    let staging_prefix = format!(
        ".{}.ferrite-staging-",
        imported.file_name().unwrap().to_str().unwrap()
    );
    let staging = loop {
        if let Some(path) = fs::read_dir(&parent).unwrap().find_map(|entry| {
            let entry = entry.unwrap();
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(&staging_prefix)
                .then(|| entry.path())
        }) {
            break path;
        }
        assert!(Instant::now() < deadline, "import did not create staging");
        thread::yield_now();
    };
    assert_eq!(
        fs::metadata(staging).unwrap().permissions().mode() & 0o777,
        0o700
    );
    fs::create_dir(&imported).unwrap();
    fs::write(&marker, b"owned by another process").unwrap();

    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    assert_eq!(fs::read(&marker).unwrap(), b"owned by another process");

    let _ = fs::remove_dir_all(imported);
    for entry in fs::read_dir(parent).unwrap() {
        let entry = entry.unwrap();
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(&staging_prefix)
        {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
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
fn export_does_not_write_inside_the_source_database() {
    let db = temp_path("reserved-export-db");
    assert!(
        ferrite(&["put", db.to_str().unwrap(), "users/1", r#"{"name":"Ada"}"#])
            .status
            .success()
    );

    let schema_path = db.join("schema.json");
    assert!(
        !ferrite(&[
            "export",
            db.to_str().unwrap(),
            schema_path.to_str().unwrap()
        ])
        .status
        .success()
    );
    assert!(!schema_path.exists());
    assert!(ferrite(&["verify", db.to_str().unwrap()]).status.success());

    let _ = fs::remove_dir_all(db);
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
    assert_eq!(
        fs::metadata(&backup).unwrap().permissions().mode() & 0o777,
        0o700
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
    assert_eq!(
        fs::metadata(&export).unwrap().permissions().mode() & 0o777,
        0o600
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
