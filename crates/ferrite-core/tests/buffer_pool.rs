use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;

use ferrite_core::buffer_pool::{
    BufferPoolError, BufferPoolManager, BufferPoolOptions, EvictionPolicy, PAGE_4K, PAGE_8K,
};
use ferrite_core::pager::Pager;

fn temp_path(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("ferrite-bpm-{name}-{}", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

// ── Basic Creation & Open ─────────────────────────────────────────────────────

#[test]
fn bpm_create_and_open_roundtrip() {
    let path = temp_path("roundtrip");
    {
        let bpm = BufferPoolManager::open_or_create(&path, PAGE_4K, None).unwrap();
        assert_eq!(bpm.page_size(), PAGE_4K);
        assert_eq!(bpm.page_count(), 1); // header page 0
        let stats = bpm.stats();
        assert_eq!(stats.active_pages, 0);
        assert_eq!(stats.pinned_pages, 0);
    }
    {
        let bpm = BufferPoolManager::open_or_create(&path, PAGE_4K, None).unwrap();
        assert_eq!(bpm.page_size(), PAGE_4K);
        assert_eq!(bpm.page_count(), 1);
    }
    let _ = std::fs::remove_file(path);
}

// ── Cache Hit & Pin / Unpin ───────────────────────────────────────────────────

#[test]
fn bpm_fetch_page_hit_and_pin_tracking() {
    let path = temp_path("fetch-hit");
    let pager = Pager::create(&path, PAGE_4K).unwrap();
    let opts = BufferPoolOptions::default().with_frames(4);
    let bpm = BufferPoolManager::with_options(pager, opts);

    // Allocate page 1
    let (p1, mut guard1) = bpm.new_page().unwrap();
    assert_eq!(p1, 1);
    guard1[0] = 0xAA;
    guard1[1] = 0xBB;
    drop(guard1); // unpinned

    let stats = bpm.stats();
    assert_eq!(stats.active_pages, 1);
    assert_eq!(stats.pinned_pages, 0);
    assert_eq!(stats.dirty_pages, 1);

    // Fetch page 1 (cache hit)
    {
        let r1 = bpm.fetch_page(p1).unwrap();
        assert_eq!(r1[0], 0xAA);
        assert_eq!(r1[1], 0xBB);
        let s = bpm.stats();
        assert_eq!(s.pinned_pages, 1);
        assert_eq!(s.hits, 1);
    } // guard dropped, unpinned

    let s = bpm.stats();
    assert_eq!(s.pinned_pages, 0);

    let _ = std::fs::remove_file(path);
}

// ── Dirty Page Eviction & Persistence ─────────────────────────────────────────

#[test]
fn bpm_dirty_page_evicted_and_persisted_to_disk() {
    let path = temp_path("dirty-evict");
    let pager = Pager::create(&path, PAGE_4K).unwrap();
    // Pool capacity: 2 frames
    let opts = BufferPoolOptions::default().with_frames(2);
    let bpm = BufferPoolManager::with_options(pager, opts);

    let (p1, mut g1) = bpm.new_page().unwrap();
    g1[0] = 0x11;
    g1[1] = 0x22;
    drop(g1);

    let (_p2, mut g2) = bpm.new_page().unwrap();
    g2[0] = 0x33;
    g2[1] = 0x44;
    drop(g2);

    // Now pool has 2 pages (p1, p2). Allocating p3 forces eviction of p1.
    let (_p3, mut g3) = bpm.new_page().unwrap();
    g3[0] = 0x55;
    g3[1] = 0x66;
    drop(g3);

    let stats = bpm.stats();
    assert_eq!(stats.evictions, 1);
    assert_eq!(stats.dirty_evictions, 1);

    // Fetch p1 back (causes cache miss and reads persisted data from disk)
    let r1 = bpm.fetch_page(p1).unwrap();
    assert_eq!(r1[0], 0x11);
    assert_eq!(r1[1], 0x22);
    drop(r1);

    let _ = std::fs::remove_file(path);
}

// ── All Frames Pinned Returns Error ───────────────────────────────────────────

#[test]
fn bpm_all_frames_pinned_returns_no_free_frame() {
    let path = temp_path("all-pinned");
    let pager = Pager::create(&path, PAGE_4K).unwrap();
    let opts = BufferPoolOptions::default().with_frames(2);
    let bpm = BufferPoolManager::with_options(pager, opts);

    let (_p1, _g1) = bpm.new_page().unwrap(); // frame 0 pinned
    let (_p2, _g2) = bpm.new_page().unwrap(); // frame 1 pinned

    // Both frames are pinned: attempting to allocate or fetch a new page should fail
    let err = bpm.new_page().err().unwrap();
    match err {
        BufferPoolError::NoFreeFrame => {}
        other => panic!("expected NoFreeFrame, got {other:?}"),
    }

    drop(_g1); // unpin frame 0
    let (_p3, _g3) = bpm.new_page().unwrap(); // succeeds by evicting p1
    assert_eq!(_p3, 3);

    let _ = std::fs::remove_file(path);
}

// ── LRU-K Eviction Policy ─────────────────────────────────────────────────────

#[test]
fn bpm_lru_k_eviction_policy() {
    let path = temp_path("lru-k");
    let pager = Pager::create(&path, PAGE_4K).unwrap();
    // Capacity 3 frames, K = 2
    let opts = BufferPoolOptions::default()
        .with_frames(3)
        .with_eviction_policy(EvictionPolicy::LruK(2));
    let bpm = BufferPoolManager::with_options(pager, opts);

    let (p1, mut g1) = bpm.new_page().unwrap();
    g1[0] = 1;
    drop(g1);

    let (p2, mut g2) = bpm.new_page().unwrap();
    g2[0] = 2;
    drop(g2);

    let (_p3, mut g3) = bpm.new_page().unwrap();
    g3[0] = 3;
    drop(g3);

    // Access p1 and p2 again so they have >= 2 accesses (finite backward 2-distance).
    // p3 has only 1 access (infinite backward distance).
    {
        let _ = bpm.fetch_page(p1).unwrap();
        let _ = bpm.fetch_page(p2).unwrap();
    }

    // Allocating p4 should evict p3 (infinite backward distance).
    let (_p4, mut g4) = bpm.new_page().unwrap();
    g4[0] = 4;
    drop(g4);

    let stats = bpm.stats();
    assert_eq!(stats.evictions, 1);

    // Verify p3 was evicted (active pages: p1, p2, p4)
    let s = bpm.stats();
    assert_eq!(s.active_pages, 3);

    let _ = std::fs::remove_file(path);
}

// ── WAL Sync Invariant Strict Enforcement ─────────────────────────────────────

#[test]
fn bpm_wal_sync_invariant_fails_without_sync() {
    let path = temp_path("wal-fail");
    let pager = Pager::create(&path, PAGE_4K).unwrap();
    // Capacity 1 frame, no WAL sync hook
    let opts = BufferPoolOptions::default().with_frames(1);
    let bpm = BufferPoolManager::with_options(pager, opts);

    let (_p1, mut g1) = bpm.new_page().unwrap();
    g1[0] = 0xFF;
    g1.mark_dirty(10); // LSN = 10, but flushed WAL LSN is 0!
    drop(g1);

    // Evicting p1 or flushing without WAL sync must fail with WalNotSynced error
    let err = bpm.new_page().err().unwrap();
    match err {
        BufferPoolError::WalNotSynced {
            page_idx,
            page_lsn,
            flushed_lsn,
        } => {
            assert_eq!(page_idx, 1);
            assert_eq!(page_lsn, 10);
            assert_eq!(flushed_lsn, 0);
        }
        other => panic!("expected WalNotSynced error, got {other:?}"),
    }

    // Record WAL sync up to LSN 10
    bpm.record_wal_sync(10);

    // Now eviction succeeds because WAL invariant is satisfied!
    let (p2, g2) = bpm.new_page().unwrap();
    assert_eq!(p2, 2);
    drop(g2);

    let _ = std::fs::remove_file(path);
}

#[test]
fn bpm_wal_sync_hook_automatically_syncs_wal_on_dirty_eviction() {
    let path = temp_path("wal-hook");
    let pager = Pager::create(&path, PAGE_4K).unwrap();

    let synced_lsn = Arc::new(AtomicU32::new(0));
    let hook_synced = synced_lsn.clone();

    let opts = BufferPoolOptions::default()
        .with_frames(1)
        .with_wal_sync_hook(move |lsn| {
            hook_synced.store(lsn, Ordering::Release);
            Ok(())
        });
    let bpm = BufferPoolManager::with_options(pager, opts);

    let (p1, mut g1) = bpm.new_page().unwrap();
    g1[0] = 0x77;
    g1.mark_dirty(42);
    drop(g1);

    // Evict p1 by allocating p2. The hook should be called with LSN 42.
    let (p2, g2) = bpm.new_page().unwrap();
    assert_eq!(p2, 2);
    drop(g2);
    assert_eq!(synced_lsn.load(Ordering::Acquire), 42);

    let stats = bpm.stats();
    assert_eq!(stats.wal_syncs, 1);
    assert_eq!(stats.dirty_evictions, 1);

    // Verify p1 data on disk
    let r1 = bpm.fetch_page(p1).unwrap();
    assert_eq!(r1[0], 0x77);
    drop(r1);

    let _ = std::fs::remove_file(path);
}

// ── Delete Page & Free-List ───────────────────────────────────────────────────

#[test]
fn bpm_delete_page_frees_in_pager() {
    let path = temp_path("delete-page");
    let pager = Pager::create(&path, PAGE_4K).unwrap();
    let opts = BufferPoolOptions::default().with_frames(4);
    let bpm = BufferPoolManager::with_options(pager, opts);

    let (p1, g1) = bpm.new_page().unwrap();
    let (_p2, g2) = bpm.new_page().unwrap();
    drop(g1);
    drop(g2);

    bpm.delete_page(p1).unwrap();
    let stats = bpm.stats();
    assert_eq!(stats.active_pages, 1);

    // Allocating new page should reuse freed page 1
    let (reused, g3) = bpm.new_page().unwrap();
    assert_eq!(reused, p1);
    drop(g3);

    let _ = std::fs::remove_file(path);
}

// ── Flush All ─────────────────────────────────────────────────────────────────

#[test]
fn bpm_flush_all_persists_pages() {
    let path = temp_path("flush-all");
    {
        let pager = Pager::create(&path, PAGE_4K).unwrap();
        let bpm = BufferPoolManager::new(pager);

        for i in 1..=5 {
            let (pid, mut g) = bpm.new_page().unwrap();
            assert_eq!(pid, i);
            g[0] = i as u8;
        }

        bpm.flush_all().unwrap();
        let stats = bpm.stats();
        assert_eq!(stats.dirty_pages, 0);
    }

    // Reopen and check pages directly
    let mut pager = Pager::open(&path).unwrap();
    for i in 1..=5 {
        let page_ref = pager.read(i).unwrap();
        assert_eq!(page_ref[0], i as u8);
    }
    let _ = std::fs::remove_file(path);
}

// ── 8 KiB Pages ───────────────────────────────────────────────────────────────

#[test]
fn bpm_supports_8k_pages() {
    let path = temp_path("8k-bpm");
    let pager = Pager::create(&path, PAGE_8K).unwrap();
    let bpm = BufferPoolManager::new(pager);

    assert_eq!(bpm.page_size(), PAGE_8K);
    let (pid, mut g) = bpm.new_page().unwrap();
    assert_eq!(pid, 1);
    g[0] = 0x88;
    g[PAGE_8K as usize - 1] = 0x99;
    drop(g);

    bpm.flush_all().unwrap();

    let r = bpm.fetch_page(1).unwrap();
    assert_eq!(r[0], 0x88);
    assert_eq!(r[PAGE_8K as usize - 1], 0x99);
    drop(r);

    let _ = std::fs::remove_file(path);
}

// ── Memory Bounded Random Access Dataset ──────────────────────────────────────

#[test]
fn bpm_bounds_ram_usage_under_high_page_volume() {
    let path = temp_path("bounded-ram");
    let pager = Pager::create(&path, PAGE_4K).unwrap();

    // Pool size: only 8 frames (32 KiB RAM)
    let opts = BufferPoolOptions::default().with_frames(8);
    let bpm = BufferPoolManager::with_options(pager, opts);

    // Create 100 pages (much larger than 8 frames)
    let total_pages = 100u64;
    for i in 1..=total_pages {
        let (pid, mut g) = bpm.new_page().unwrap();
        assert_eq!(pid, i);
        g[0] = (i % 250) as u8;
        g[1] = ((i * 3) % 250) as u8;
    }

    bpm.flush_all().unwrap();

    // RAM usage strictly bounded by 8 frames:
    let stats = bpm.stats();
    assert_eq!(stats.pool_size, 8);
    assert_eq!(stats.allocated_memory_bytes, 8 * (PAGE_4K as usize));
    assert!(stats.active_pages <= 8);

    // Perform random access across all 100 pages
    for i in 1..=total_pages {
        let idx = ((i * 37) % total_pages) + 1;
        let r = bpm.fetch_page(idx).unwrap();
        assert_eq!(r[0], (idx % 250) as u8);
        assert_eq!(r[1], ((idx * 3) % 250) as u8);
    }

    let end_stats = bpm.stats();
    assert!(end_stats.active_pages <= 8);
    assert!(end_stats.evictions >= 90);

    let _ = std::fs::remove_file(path);
}

// ── Multi-Threaded Concurrent Readers and Writers ─────────────────────────────

#[test]
fn bpm_concurrent_multi_threaded_readers_and_writers() {
    let path = temp_path("concurrent-rw");
    let pager = Pager::create(&path, PAGE_4K).unwrap();
    // Capacity 16 frames with WAL sync hook to satisfy invariant on eviction
    let opts = BufferPoolOptions::default()
        .with_frames(16)
        .with_wal_sync_hook(|_lsn| Ok(()));
    let bpm = BufferPoolManager::with_options(pager, opts);

    // Initialize 30 pages
    for i in 1..=30 {
        let (pid, mut g) = bpm.new_page().unwrap();
        assert_eq!(pid, i);
        g[0] = (i % 250) as u8;
    }
    bpm.flush_all().unwrap();

    let mut handles = Vec::new();
    let num_threads = 8;
    let ops_per_thread = 200;

    for thread_id in 0..num_threads {
        let bpm_clone = bpm.clone();
        let handle = thread::spawn(move || {
            for step in 0..ops_per_thread {
                let page_idx = ((thread_id * 17 + step) % 30) + 1;
                if step % 3 == 0 {
                    // Writer
                    let mut guard = bpm_clone.fetch_page_mut(page_idx).unwrap();
                    guard[1] = (thread_id + step) as u8;
                    guard.mark_dirty(1);
                } else {
                    // Reader
                    let guard = bpm_clone.fetch_page(page_idx).unwrap();
                    assert_eq!(guard[0], (page_idx % 250) as u8);
                }
            }
        });
        handles.push(handle);
    }

    for h in handles {
        h.join().unwrap();
    }

    bpm.flush_all().unwrap();

    let stats = bpm.stats();
    assert_eq!(stats.pinned_pages, 0);
    assert!(stats.hits + stats.misses >= num_threads * ops_per_thread);

    let _ = std::fs::remove_file(path);
}

// ── Acceptance Criteria: 64MB Budget on Large Dataset ─────────────────────────

#[test]
fn bpm_acceptance_criteria_64mb_budget_on_large_dataset() {
    let path = temp_path("acceptance-64mb");
    let pager = Pager::create(&path, PAGE_4K).unwrap();
    // Default 64 MiB buffer pool manager (16,384 frames of 4 KiB)
    let bpm = BufferPoolManager::new(pager);

    let initial_stats = bpm.stats();
    assert_eq!(initial_stats.pool_size, 16384);
    assert_eq!(initial_stats.allocated_memory_bytes, 64 * 1024 * 1024);

    // Create 1,000 pages
    let num_pages = 1000u64;
    for i in 1..=num_pages {
        let (pid, mut g) = bpm.new_page().unwrap();
        assert_eq!(pid, i);
        g[0] = (i % 251) as u8;
        g[1] = ((i * 7) % 251) as u8;
    }
    bpm.flush_all().unwrap();

    // Perform 5,000 pseudo-random reads and writes
    let iterations = 5000;
    for step in 0..iterations {
        let page_idx = ((step * 179 + 13) % num_pages as usize) as u64 + 1;
        if step % 5 == 0 {
            let mut guard = bpm.fetch_page_mut(page_idx).unwrap();
            guard[2] = (step % 255) as u8;
            guard.mark_dirty(0);
        } else {
            let guard = bpm.fetch_page(page_idx).unwrap();
            assert_eq!(guard[0], (page_idx % 251) as u8);
            assert_eq!(guard[1], ((page_idx * 7) % 251) as u8);
        }
    }

    bpm.flush_all().unwrap();

    let stats = bpm.stats();
    // Invariant: allocated memory remains strictly 64 MiB
    assert_eq!(stats.allocated_memory_bytes, 64 * 1024 * 1024);
    assert_eq!(stats.pinned_pages, 0);
    assert!(stats.hits > 0);
    assert!(stats.hit_ratio() > 0.0);

    let _ = std::fs::remove_file(path);
}
