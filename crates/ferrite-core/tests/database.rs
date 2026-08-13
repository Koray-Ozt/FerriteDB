use ferrite_core::{Database, Error, Operation};
use serde_json::json;

fn temp_dir(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("ferrite-db-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    path
}

#[test]
fn atomic_crud_recovers_after_restart() {
    let path = temp_dir("crud");
    {
        let mut db = Database::open(&path).unwrap();
        db.transaction(&[
            Operation::Put {
                key: "a".into(),
                value: json!({"n": 1}),
            },
            Operation::Put {
                key: "b".into(),
                value: json!({"n": 2}),
            },
        ])
        .unwrap();
        db.transaction(&[Operation::Delete { key: "a".into() }])
            .unwrap();
        assert_eq!(db.get("a").unwrap(), None);
        assert_eq!(db.list(None).unwrap(), vec![("b".into(), json!({"n": 2}))]);
    }
    let db = Database::open(&path).unwrap();
    assert_eq!(db.get("b").unwrap(), Some(json!({"n": 2})));
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn schema_validates_primary_and_unique_fields_after_restart() {
    let path = temp_dir("schema");
    let schema = json!({"collections":{"users":{"primary_key":"id","unique":["email"]}}});
    {
        let mut db = Database::open_with_schema(&path, &schema).unwrap();
        db.put("users", "u1", json!({"id":"u1","email":"a@example.com"}))
            .unwrap();
        let error = db
            .put("users", "u2", json!({"id":"u2","email":"a@example.com"}))
            .unwrap_err();
        assert!(matches!(error, Error::UniqueViolation { .. }));
        let error = db.put("users", "wrong", json!({"id":"u3"})).unwrap_err();
        assert!(matches!(error, Error::Schema(_)));
    }
    let mut db = Database::open_with_schema(&path, &schema).unwrap();
    let error = db
        .put("users", "u2", json!({"id":"u2","email":"a@example.com"}))
        .unwrap_err();
    assert!(matches!(error, Error::UniqueViolation { .. }));
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn incompatible_schema_open_does_not_install_schema() {
    let path = temp_dir("schema-open-rollback");
    {
        let mut db = Database::open(&path).unwrap();
        db.put_key("legacy", json!({"value": 1})).unwrap();
    }
    let schema = json!({"collections":{"users":{"primary_key":"id","unique":[]}}});

    let error = match Database::open_with_schema(&path, &schema) {
        Ok(_) => panic!("incompatible schema unexpectedly opened"),
        Err(error) => error,
    };
    assert!(matches!(error, Error::Schema(_)));
    assert!(!path.join("schema.json").exists());

    let db = Database::open(&path).unwrap();
    assert_eq!(db.get("legacy").unwrap(), Some(json!({"value": 1})));
    drop(db);
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn transaction_id_exhaustion_does_not_install_schema() {
    let path = temp_dir("schema-id-exhaustion");
    {
        let db = Database::open(&path).unwrap();
        drop(db);
        let mut wal = ferrite_core::wal::Wal::open(path.join("data.wal")).unwrap();
        wal.begin(u64::MAX).unwrap();
    }
    let schema = json!({"collections":{"users":{"primary_key":"id","unique":[]}}});

    let error = match Database::open_with_schema(&path, &schema) {
        Ok(_) => panic!("exhausted database unexpectedly opened"),
        Err(error) => error,
    };
    assert!(matches!(error, Error::Limit("transaction id exhausted")));
    assert!(!path.join("schema.json").exists());

    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn validation_failure_rolls_back_the_entire_transaction() {
    let path = temp_dir("atomic-validation");
    let schema = json!({"collections":{"users":{"primary_key":"id","unique":["email"]}}});
    let mut db = Database::open_with_schema(&path, &schema).unwrap();

    let error = db
        .transaction(&[
            Operation::Put {
                key: "users/u1".into(),
                value: json!({"id":"u1","email":"same@example.com"}),
            },
            Operation::Put {
                key: "users/u2".into(),
                value: json!({"id":"u2","email":"same@example.com"}),
            },
        ])
        .unwrap_err();

    assert!(matches!(error, Error::UniqueViolation { .. }));
    assert_eq!(db.get("users/u1").unwrap(), None);
    assert_eq!(db.get("users/u2").unwrap(), None);
    drop(db);

    let db = Database::open_with_schema(&path, &schema).unwrap();
    assert_eq!(db.get("users/u1").unwrap(), None);
    assert_eq!(db.get("users/u2").unwrap(), None);
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn schema_cannot_be_bypassed_with_an_unscoped_key() {
    let path = temp_dir("schema-bypass");
    let schema = json!({"collections":{"users":{"primary_key":"id","unique":["email"]}}});
    let mut db = Database::open_with_schema(&path, &schema).unwrap();

    let error = db
        .put_key("unscoped", json!({"id":"u1","email":"a@example.com"}))
        .unwrap_err();

    assert!(matches!(error, Error::Schema(_)));
    assert_eq!(db.get("unscoped").unwrap(), None);
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn a_second_writer_is_rejected_while_the_database_is_open() {
    let path = temp_dir("exclusive-lock");
    let first = Database::open(&path).unwrap();

    let second = Database::open(&path);
    assert!(
        second.is_err(),
        "a second writer unexpectedly acquired the database"
    );

    drop(first);
    Database::open(&path).unwrap();
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn database_continues_after_a_fully_written_uncommitted_tail() {
    let path = temp_dir("pending-tail");
    {
        let mut db = Database::open(&path).unwrap();
        db.put_key("stable", json!(1)).unwrap();
    }
    {
        let mut wal = ferrite_core::wal::Wal::open(path.join("data.wal")).unwrap();
        wal.begin(2).unwrap();
        wal.put(2, b"discard", br#"2"#).unwrap();
    }

    {
        let mut db = Database::open(&path).unwrap();
        assert_eq!(db.get("discard").unwrap(), None);
        db.put_key("future", json!(3)).unwrap();
    }
    let db = Database::open(&path).unwrap();
    assert_eq!(db.get("stable").unwrap(), Some(json!(1)));
    assert_eq!(db.get("future").unwrap(), Some(json!(3)));
    assert_eq!(db.get("discard").unwrap(), None);
    drop(db);
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn conservative_limits_are_explicit() {
    let path = temp_dir("limits");
    let mut db = Database::open(&path).unwrap();
    let error = db
        .put_key(&"x".repeat(ferrite_core::MAX_KEY_BYTES + 1), json!(1))
        .unwrap_err();
    assert!(matches!(error, Error::Limit(_)));
    let operations = (0..=ferrite_core::MAX_TRANSACTION_OPERATIONS)
        .map(|n| Operation::Delete {
            key: format!("k{n}"),
        })
        .collect::<Vec<_>>();
    assert!(matches!(db.transaction(&operations), Err(Error::Limit(_))));
    std::fs::remove_dir_all(path).unwrap();
}
