use ferrite_core::btree::BTree;
use ferrite_core::buffer_pool::{
    BufferPoolManager, BufferPoolOptions, EvictionPolicy, PAGE_4K, PAGE_8K,
};
use ferrite_core::pager::Pager;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(prefix: &str) -> Self {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let id = CTR.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "ferrite_btree_test_{prefix}_{id}_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn file_path(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn create_bpm(path: &std::path::Path, page_size: u32, frames: usize) -> BufferPoolManager {
    let pager = Pager::create(path, page_size).unwrap();
    let opts = BufferPoolOptions::from_budget(frames * page_size as usize, page_size)
        .with_frames(frames)
        .with_eviction_policy(EvictionPolicy::Clock);
    BufferPoolManager::with_options(pager, opts)
}

fn open_bpm(path: &std::path::Path, frames: usize) -> BufferPoolManager {
    let pager = Pager::open(path).unwrap();
    let page_size = pager.page_size();
    let opts = BufferPoolOptions::from_budget(frames * page_size as usize, page_size)
        .with_frames(frames)
        .with_eviction_policy(EvictionPolicy::Clock);
    BufferPoolManager::with_options(pager, opts)
}

#[test]
fn btree_create_and_single_leaf_crud() {
    let dir = TempDir::new("single_leaf");
    let file = dir.file_path("test.fdb");
    let bpm = create_bpm(&file, PAGE_4K, 16);
    let tree = BTree::create(bpm).unwrap();

    assert!(tree.is_empty().unwrap());
    assert_eq!(tree.len().unwrap(), 0);

    // Insert keys
    tree.insert(b"key_b", b"val_b").unwrap();
    tree.insert(b"key_a", b"val_a").unwrap();
    tree.insert(b"key_c", b"val_c").unwrap();

    assert_eq!(tree.len().unwrap(), 3);
    assert!(!tree.is_empty().unwrap());

    // Point lookups
    assert_eq!(tree.get(b"key_a").unwrap(), Some(b"val_a".to_vec()));
    assert_eq!(tree.get(b"key_b").unwrap(), Some(b"val_b".to_vec()));
    assert_eq!(tree.get(b"key_c").unwrap(), Some(b"val_c".to_vec()));
    assert_eq!(tree.get(b"key_d").unwrap(), None);

    // Overwrite
    tree.insert(b"key_b", b"val_b_updated").unwrap();
    assert_eq!(tree.get(b"key_b").unwrap(), Some(b"val_b_updated".to_vec()));
    assert_eq!(tree.len().unwrap(), 3);

    // First and last
    assert_eq!(
        tree.first().unwrap(),
        Some((b"key_a".to_vec(), b"val_a".to_vec()))
    );
    assert_eq!(
        tree.last().unwrap(),
        Some((b"key_c".to_vec(), b"val_c".to_vec()))
    );

    // Delete
    assert!(tree.delete(b"key_b").unwrap());
    assert_eq!(tree.get(b"key_b").unwrap(), None);
    assert_eq!(tree.len().unwrap(), 2);
    assert!(!tree.delete(b"key_b").unwrap()); // already deleted

    // Verify invariants
    let stats = tree.verify_integrity().unwrap();
    assert_eq!(stats.depth, 0);
    assert_eq!(stats.total_keys, 2);
    assert_eq!(stats.leaf_pages, 1);
    assert_eq!(stats.internal_pages, 0);
}

#[test]
fn btree_node_split_and_depth_growth() {
    let dir = TempDir::new("split_growth");
    let file = dir.file_path("test.fdb");
    let bpm = create_bpm(&file, PAGE_4K, 64);
    let tree = BTree::create(bpm.clone()).unwrap();

    // Insert 1,000 keys to force multi-level splits
    let mut ground_truth = BTreeMap::new();
    for i in 0..1000 {
        let key = format!("user_{:06}", i).into_bytes();
        let val = format!("payload_data_for_{:06}", i).into_bytes();
        ground_truth.insert(key.clone(), val.clone());
        tree.insert(&key, &val).unwrap();
    }

    assert_eq!(tree.len().unwrap(), 1000);

    // Verify all lookups match
    for (k, v) in &ground_truth {
        assert_eq!(tree.get(k).unwrap(), Some(v.clone()));
    }

    // Verify tree integrity
    let stats = tree.verify_integrity().unwrap();
    assert!(
        stats.depth >= 1,
        "Tree depth should have grown due to splits"
    );
    assert!(stats.leaf_pages > 1);
    assert_eq!(stats.total_keys, 1000);
    assert_eq!(stats.min_key, Some(format!("user_{:06}", 0).into_bytes()));
    assert_eq!(stats.max_key, Some(format!("user_{:06}", 999).into_bytes()));
}

#[test]
fn btree_reopen_persists_state() {
    let dir = TempDir::new("persist");
    let file = dir.file_path("test.fdb");
    let root_page_id;

    {
        let bpm = create_bpm(&file, PAGE_4K, 32);
        let tree = BTree::create(bpm.clone()).unwrap();
        for i in 0..500 {
            let key = format!("k_{:04}", i).into_bytes();
            let val = format!("v_{:04}", i).into_bytes();
            tree.insert(&key, &val).unwrap();
        }
        root_page_id = tree.root_page_id();
        bpm.flush_all().unwrap();
    }

    // Reopen from disk
    {
        let bpm = open_bpm(&file, 32);
        let tree = BTree::open(bpm, root_page_id).unwrap();
        assert_eq!(tree.len().unwrap(), 500);

        for i in 0..500 {
            let key = format!("k_{:04}", i).into_bytes();
            let val = format!("v_{:04}", i).into_bytes();
            assert_eq!(tree.get(&key).unwrap(), Some(val));
        }

        let stats = tree.verify_integrity().unwrap();
        assert_eq!(stats.total_keys, 500);
    }
}

#[test]
fn btree_range_scan_and_iterator() {
    let dir = TempDir::new("range_scan");
    let file = dir.file_path("test.fdb");
    let bpm = create_bpm(&file, PAGE_4K, 32);
    let tree = BTree::create(bpm).unwrap();

    for i in 0..300 {
        let key = format!("item_{:05}", i).into_bytes();
        let val = format!("val_{:05}", i).into_bytes();
        tree.insert(&key, &val).unwrap();
    }

    // Test scan with limit
    let scan_res = tree.scan(b"item_00050", 10).unwrap();
    assert_eq!(scan_res.len(), 10);
    assert_eq!(scan_res[0].0, b"item_00050");
    assert_eq!(scan_res[9].0, b"item_00059");

    // Test range iterator
    let start_key = b"item_00100".to_vec();
    let end_key = b"item_00150".to_vec();
    let range_entries: Vec<(Vec<u8>, Vec<u8>)> = tree
        .range(start_key..=end_key)
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    assert_eq!(range_entries.len(), 51);
    assert_eq!(range_entries.first().unwrap().0, b"item_00100");
    assert_eq!(range_entries.last().unwrap().0, b"item_00150");
}

#[test]
fn btree_deletion_merging_and_redistribution() {
    let dir = TempDir::new("delete_merge");
    let file = dir.file_path("test.fdb");
    let bpm = create_bpm(&file, PAGE_4K, 64);
    let tree = BTree::create(bpm).unwrap();

    // Insert 800 keys
    for i in 0..800 {
        let key = format!("key_{:05}", i).into_bytes();
        let val = format!("val_{:05}", i).into_bytes();
        tree.insert(&key, &val).unwrap();
    }

    assert_eq!(tree.len().unwrap(), 800);
    tree.verify_integrity().unwrap();

    // Delete half the keys in mixed order (even numbers)
    for i in (0..800).step_by(2) {
        let key = format!("key_{:05}", i).into_bytes();
        assert!(tree.delete(&key).unwrap());
    }

    assert_eq!(tree.len().unwrap(), 400);
    let stats = tree.verify_integrity().unwrap();
    assert_eq!(stats.total_keys, 400);

    // Verify remaining odd keys
    for i in 0..800 {
        let key = format!("key_{:05}", i).into_bytes();
        if i % 2 == 0 {
            assert_eq!(tree.get(&key).unwrap(), None);
        } else {
            let expected_val = format!("val_{:05}", i).into_bytes();
            assert_eq!(tree.get(&key).unwrap(), Some(expected_val));
        }
    }

    // Delete all remaining odd keys
    for i in (1..800).step_by(2) {
        let key = format!("key_{:05}", i).into_bytes();
        assert!(tree.delete(&key).unwrap());
    }

    assert_eq!(tree.len().unwrap(), 0);
    assert!(tree.is_empty().unwrap());
    let final_stats = tree.verify_integrity().unwrap();
    assert_eq!(final_stats.total_keys, 0);
}

#[test]
fn btree_supports_8k_pages() {
    let dir = TempDir::new("8k_pages");
    let file = dir.file_path("test_8k.fdb");
    let bpm = create_bpm(&file, PAGE_8K, 32);
    let tree = BTree::create(bpm).unwrap();

    for i in 0..1500 {
        let key = format!("record_8k_{:06}", i).into_bytes();
        let val = format!("data_8k_{:06}", i).into_bytes();
        tree.insert(&key, &val).unwrap();
    }

    assert_eq!(tree.len().unwrap(), 1500);
    let stats = tree.verify_integrity().unwrap();
    assert_eq!(stats.total_keys, 1500);

    for i in 0..1500 {
        let key = format!("record_8k_{:06}", i).into_bytes();
        let expected = format!("data_8k_{:06}", i).into_bytes();
        assert_eq!(tree.get(&key).unwrap(), Some(expected));
    }
}

#[test]
fn btree_concurrent_readers() {
    let dir = TempDir::new("concurrent_readers");
    let file = dir.file_path("test.fdb");
    let bpm = create_bpm(&file, PAGE_4K, 64);
    let tree = Arc::new(BTree::create(bpm).unwrap());

    for i in 0..1000 {
        let key = format!("shared_{:05}", i).into_bytes();
        let val = format!("value_{:05}", i).into_bytes();
        tree.insert(&key, &val).unwrap();
    }

    let mut handles = Vec::new();
    for thread_id in 0..8 {
        let tree_clone = tree.clone();
        handles.push(thread::spawn(move || {
            for i in 0..1000 {
                let key = format!("shared_{:05}", (i + thread_id * 100) % 1000).into_bytes();
                let expected = format!("value_{:05}", (i + thread_id * 100) % 1000).into_bytes();
                let found = tree_clone.get(&key).unwrap();
                assert_eq!(found, Some(expected));
            }
        }));
    }

    for h in handles {
        h.join().unwrap();
    }
}

#[test]
fn btree_acceptance_criteria_bounded_ram_large_dataset() {
    let dir = TempDir::new("100k_bounded_ram");
    let file = dir.file_path("large.fdb");

    // Strictly bounded buffer pool memory: only 32 frames (128 KiB RAM cache!)
    let bpm = create_bpm(&file, PAGE_4K, 32);
    let tree = BTree::create(bpm.clone()).unwrap();

    let count = 20_000; // Large number of keys to thoroughly exercise continuous dirty page evictions
    let mut rng_state: u64 = 0x853c49e6748fea9b;

    // Linear congruential generator for reproducible pseudo-random numbers
    let mut next_rng = || {
        rng_state = rng_state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        rng_state
    };

    let mut keys = Vec::with_capacity(count);
    for _ in 0..count {
        let val = next_rng() % 1_000_000;
        let k = format!("k_{:08}", val).into_bytes();
        let v = format!("v_{:08}", val).into_bytes();
        tree.insert(&k, &v).unwrap();
        keys.push((k, v));
    }

    // Verify stats
    let stats = bpm.stats();
    assert!(
        stats.evictions > 0,
        "Evictions must have occurred under bounded 128 KiB RAM"
    );
    assert!(
        stats.dirty_evictions > 0,
        "Dirty page evictions must have occurred"
    );

    // Random lookups
    for (k, v) in &keys {
        assert_eq!(tree.get(k).unwrap(), Some(v.clone()));
    }

    // Invariant verification
    let tree_stats = tree.verify_integrity().unwrap();
    assert!(tree_stats.depth >= 2);
    assert_eq!(tree_stats.total_keys, tree.len().unwrap());
}
