#![cfg(feature = "crash-testing")]

use serde_json::Value;
use std::fs;
use std::process::Command;

fn temp_path(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("ferrite-crash-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    path
}

fn ferrite(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_ferrite"))
        .args(args)
        .output()
        .unwrap()
}

fn read(db: &std::path::Path, key: &str) -> Option<Value> {
    let output = ferrite(&["get", db.to_str().unwrap(), key]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn killed_writes_never_expose_partial_transactions() {
    for point in [
        "wal-after-begin",
        "wal-after-write",
        "wal-after-commit-record",
        "wal-after-sync",
    ] {
        let db = temp_path(point);
        assert!(
            ferrite(&["put", db.to_str().unwrap(), "stable", "1"])
                .status
                .success()
        );

        let status = Command::new(env!("CARGO_BIN_EXE_ferrite"))
            .env("FERRITE_CRASH_AT", point)
            .args(["put", db.to_str().unwrap(), "candidate", "2"])
            .status()
            .unwrap();
        assert!(
            !status.success(),
            "failpoint {point} did not kill the child"
        );

        assert_eq!(read(&db, "stable"), Some(serde_json::json!(1)));
        let candidate = read(&db, "candidate");
        match point {
            "wal-after-begin" | "wal-after-write" => assert_eq!(candidate, None),
            "wal-after-commit-record" => {
                assert!(candidate.is_none() || candidate == Some(serde_json::json!(2)));
            }
            "wal-after-sync" => assert_eq!(candidate, Some(serde_json::json!(2))),
            _ => unreachable!(),
        }
        fs::remove_dir_all(db).unwrap();
    }
}

#[test]
fn killed_checkpoints_reopen_to_complete_state() {
    for point in ["checkpoint-after-staging-sync", "checkpoint-after-rename"] {
        let db = temp_path(point);
        assert!(
            ferrite(&["put", db.to_str().unwrap(), "stable", "1"])
                .status
                .success()
        );

        let status = Command::new(env!("CARGO_BIN_EXE_ferrite"))
            .env("FERRITE_CRASH_AT", point)
            .args(["checkpoint", db.to_str().unwrap()])
            .status()
            .unwrap();
        assert!(
            !status.success(),
            "failpoint {point} did not kill the child"
        );

        assert_eq!(read(&db, "stable"), Some(serde_json::json!(1)));
        let retry = ferrite(&["checkpoint", db.to_str().unwrap()]);
        assert!(
            retry.status.success(),
            "checkpoint retry after {point} failed: {}",
            String::from_utf8_lossy(&retry.stderr)
        );
        fs::remove_dir_all(db).unwrap();
    }
}
