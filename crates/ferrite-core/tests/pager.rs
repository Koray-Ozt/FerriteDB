use ferrite_core::pager::{PagerError, Pager, PAGE_4K, PAGE_8K};

fn temp_path(name: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir()
        .join(format!("ferrite-pager-{name}-{}", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

// ── create / open roundtrip ───────────────────────────────────────────────────

#[test]
fn create_and_open_roundtrip() {
    let path = temp_path("roundtrip");
    {
        let p = Pager::create(&path, PAGE_4K).unwrap();
        assert_eq!(p.page_size(), PAGE_4K);
        assert_eq!(p.page_count(), 1); // header only
        assert_eq!(p.last_wal_seq(), 0);
    }
    let p = Pager::open(&path).unwrap();
    assert_eq!(p.page_size(), PAGE_4K);
    assert_eq!(p.page_count(), 1);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn create_with_8k_page_size() {
    let path = temp_path("8k");
    let p = Pager::create(&path, PAGE_8K).unwrap();
    assert_eq!(p.page_size(), PAGE_8K);
    drop(p);
    let p2 = Pager::open(&path).unwrap();
    assert_eq!(p2.page_size(), PAGE_8K);
    std::fs::remove_file(path).unwrap();
}

// ── sequential allocation ─────────────────────────────────────────────────────

#[test]
fn alloc_sequential_pages() {
    let path = temp_path("seq-alloc");
    let mut p = Pager::create(&path, PAGE_4K).unwrap();
    let idx1 = p.alloc().unwrap();
    let idx2 = p.alloc().unwrap();
    let idx3 = p.alloc().unwrap();
    assert_eq!(idx1, 1);
    assert_eq!(idx2, 2);
    assert_eq!(idx3, 3);
    assert_eq!(p.page_count(), 4);
    std::fs::remove_file(path).unwrap();
}

// ── free-list reuse ───────────────────────────────────────────────────────────

#[test]
fn free_list_reuse_single() {
    let path = temp_path("free-reuse");
    let mut p = Pager::create(&path, PAGE_4K).unwrap();
    let idx = p.alloc().unwrap(); // page 1
    p.free(idx).unwrap();
    let reused = p.alloc().unwrap();
    assert_eq!(reused, idx, "must reuse freed page");
    std::fs::remove_file(path).unwrap();
}

#[test]
fn free_list_lifo_chain() {
    let path = temp_path("free-chain");
    let mut p = Pager::create(&path, PAGE_4K).unwrap();
    let a = p.alloc().unwrap(); // 1
    let b = p.alloc().unwrap(); // 2
    let c = p.alloc().unwrap(); // 3
    p.free(a).unwrap();
    p.free(b).unwrap();
    p.free(c).unwrap();
    // LIFO: last freed (c=3) comes back first
    assert_eq!(p.alloc().unwrap(), c);
    assert_eq!(p.alloc().unwrap(), b);
    assert_eq!(p.alloc().unwrap(), a);
    // No more free pages → extend file
    let next = p.alloc().unwrap();
    assert_eq!(next, 4);
    std::fs::remove_file(path).unwrap();
}

// ── read / write page data ────────────────────────────────────────────────────

#[test]
fn read_write_page_data_roundtrip() {
    let path = temp_path("rw");
    let mut p = Pager::create(&path, PAGE_4K).unwrap();
    let idx = p.alloc().unwrap();

    let mut payload = vec![0u8; PAGE_4K as usize];
    payload[0] = 0xDE;
    payload[1] = 0xAD;
    payload[PAGE_4K as usize - 1] = 0xFF;
    p.write_page(idx, &payload).unwrap();

    let r = p.read(idx).unwrap();
    assert_eq!(r.as_bytes()[0], 0xDE);
    assert_eq!(r.as_bytes()[1], 0xAD);
    assert_eq!(r.as_bytes()[PAGE_4K as usize - 1], 0xFF);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn page_mut_flush_roundtrip() {
    let path = temp_path("page-mut");
    let mut p = Pager::create(&path, PAGE_4K).unwrap();
    let idx = p.alloc().unwrap();
    {
        let mut pg = p.write(idx).unwrap();
        pg.as_bytes_mut()[42] = 0xBE;
        pg.as_bytes_mut()[43] = 0xEF;
        pg.flush().unwrap();
    }
    let r = p.read(idx).unwrap();
    assert_eq!(r[42], 0xBE);
    assert_eq!(r[43], 0xEF);
    std::fs::remove_file(path).unwrap();
}

// ── WAL sequence persistence ──────────────────────────────────────────────────

#[test]
fn wal_seq_persists_across_reopen() {
    let path = temp_path("wal-seq");
    {
        let mut p = Pager::create(&path, PAGE_4K).unwrap();
        p.record_wal_commit(42).unwrap();
        assert_eq!(p.last_wal_seq(), 42);
    }
    let p = Pager::open(&path).unwrap();
    assert_eq!(p.last_wal_seq(), 42);
    std::fs::remove_file(path).unwrap();
}

// ── Corruption / invalid input rejection ─────────────────────────────────────

#[test]
fn corrupt_magic_rejected() {
    let path = temp_path("bad-magic");
    {
        Pager::create(&path, PAGE_4K).unwrap();
    }
    // Overwrite the first 8 bytes with garbage.
    let mut raw = std::fs::read(&path).unwrap();
    raw[0] = 0xFF;
    std::fs::write(&path, &raw).unwrap();

    let err = Pager::open(&path).unwrap_err();
    assert!(matches!(err, PagerError::Corrupt(_)));
    std::fs::remove_file(path).unwrap();
}

#[test]
fn invalid_page_size_rejected() {
    let err = Pager::create(temp_path("bad-ps"), 512).unwrap_err();
    assert!(matches!(err, PagerError::InvalidPageSize(512)));
}

#[test]
fn page_out_of_range_rejected() {
    let path = temp_path("out-of-range");
    let mut p = Pager::create(&path, PAGE_4K).unwrap();
    // page_count == 1, so idx=1 is out of range
    let err = p.read(1).unwrap_err();
    assert!(matches!(err, PagerError::PageOutOfRange(1)));
    std::fs::remove_file(path).unwrap();
}
