use std::fs::OpenOptions;
use std::io::Write;

use ferrite_core::wal::{Recovery, Wal};

fn temp_path(name: &str) -> std::path::PathBuf {
    let id = std::process::id();
    std::env::temp_dir().join(format!("ferrite-{name}-{id}.wal"))
}

#[test]
fn recovery_returns_only_fully_committed_transactions() {
    let path = temp_path("committed");
    let _ = std::fs::remove_file(&path);

    {
        let mut wal = Wal::create(&path).unwrap();
        wal.begin(1).unwrap();
        wal.put(1, b"users/1", b"Koray").unwrap();
        wal.commit(1).unwrap();
        wal.begin(2).unwrap();
        wal.put(2, b"users/2", b"Ada").unwrap();
        // Simulate a process dying before transaction 2's commit record.
    }

    let recovery = Recovery::read(&path).unwrap();
    assert_eq!(recovery.committed().len(), 1);
    assert_eq!(recovery.committed()[0].id(), 1);
    assert_eq!(recovery.committed()[0].writes()[0].key(), b"users/1");
    assert_eq!(recovery.committed()[0].writes()[0].value(), b"Koray");

    std::fs::remove_file(path).unwrap();
}

#[test]
fn recovery_rejects_a_corrupted_record() {
    let path = temp_path("corrupt");
    let _ = std::fs::remove_file(&path);

    {
        let mut wal = Wal::create(&path).unwrap();
        wal.begin(1).unwrap();
        wal.put(1, b"users/1", b"Koray").unwrap();
        wal.commit(1).unwrap();
    }

    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(b"corrupt").unwrap();
    file.sync_all().unwrap();

    let error = Recovery::read(&path).unwrap_err();
    assert!(error.to_string().contains("corrupt WAL"));

    std::fs::remove_file(path).unwrap();
}
