//! Persistent on-disk B+Tree data structure over Pager and BufferPoolManager.
//!
//! # Architecture
//!
//! The B+Tree maps variable-length keys to variable-length values directly onto
//! 4 KiB or 8 KiB slotted disk pages. All page accesses flow through the
//! [`BufferPoolManager`], ensuring memory usage is strictly bounded regardless of
//! dataset size.
//!
//! ```text
//!                      ┌───────────────────────┐
//!                      │  Internal Root Node   │
//!                      │ [Page 1] (Keys, Ptrs) │
//!                      └───────────┬───────────┘
//!                                  │
//!                 ┌────────────────┴────────────────┐
//!                 ▼                                 ▼
//!      ┌───────────────────────┐         ┌───────────────────────┐
//!      │ Internal Branch Node  │         │ Internal Branch Node  │
//!      │ [Page 2] (Keys, Ptrs) │         │ [Page 3] (Keys, Ptrs) │
//!      └──────────┬────────────┘         └──────────┬────────────┘
//!                 │                                 │
//!        ┌────────┴────────┐               ┌────────┴────────┐
//!        ▼                 ▼               ▼                 ▼
//! ┌─────────────┐   ┌─────────────┐ ┌─────────────┐   ┌─────────────┐
//! │  Leaf Node  │◄─►│  Leaf Node  │◄│  Leaf Node  │◄─►│  Leaf Node  │
//! │  [Page 4]   │   │  [Page 5]   │ │  [Page 6]   │   │  [Page 7]   │
//! └─────────────┘   └─────────────┘ └─────────────┘   └─────────────┘
//!      ▲                                                   │
//!      └────────────── Doubly Linked Leaves ───────────────┘
//! ```
//!
//! # Key Invariants
//!
//! 1. **Leaf Level Balancing**: All leaf nodes reside at the exact same depth.
//! 2. **Ordered Keys**: Keys within every node (interior and leaf) are strictly sorted.
//! 3. **Search Invariant**: For interior node key $K_i$, all keys in child $C_i$ are $< K_i$,
//!    and all keys in child $C_{i+1}$ are $\ge K_i$.
//! 4. **Doubly Linked Leaves**: Leaves form a bidirectional chain via `prev_leaf` and `next_leaf`
//!    enabling $O(1)$ sequential range scans.
//! 5. **Dynamic Rebalancing**: Nodes split on overflow and merge/redistribute on underflow.

use std::cmp::Ordering;
use std::fmt;
use std::ops::{Bound, RangeBounds};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use crate::buffer_pool::{BufferPoolError, BufferPoolManager, PageMutGuard};

// ── Constants & Formats ───────────────────────────────────────────────────────

pub const PAGE_TYPE_LEAF: u8 = 0x01;
pub const PAGE_TYPE_INTERNAL: u8 = 0x02;

/// Maximum allowable key size for B+Tree entries (4 KiB).
pub const MAX_BTREE_KEY_BYTES: usize = 4096;
/// Maximum allowable value size for B+Tree leaf entries (64 KiB).
pub const MAX_BTREE_VALUE_BYTES: usize = 64 * 1024;

const LEAF_HEADER_LEN: usize = 32;
const INTERNAL_HEADER_LEN: usize = 24;

const LEAF_SLOT_LEN: usize = 8;
const INTERNAL_SLOT_LEN: usize = 12;

// Header offsets for Leaf Pages:
// [0..1]   page_type (1 = LEAF)
// [1..2]   flags
// [2..4]   num_keys (u16 LE)
// [4..6]   free_start (u16 LE)
// [6..8]   free_end (u16 LE)
// [8..16]  parent_page_id (u64 LE)
// [16..24] prev_leaf (u64 LE)
// [24..32] next_leaf (u64 LE)
const OFF_TYPE: usize = 0;
const OFF_FLAGS: usize = 1;
const OFF_NUM_KEYS: usize = 2;
const OFF_FREE_START: usize = 4;
const OFF_FREE_END: usize = 6;
const OFF_PARENT: usize = 8;
const OFF_LEAF_PREV: usize = 16;
const OFF_LEAF_NEXT: usize = 24;

// Header offsets for Internal Pages:
// [0..1]   page_type (2 = INTERNAL)
// [1..2]   flags
// [2..4]   num_keys (u16 LE)
// [4..6]   free_start (u16 LE)
// [6..8]   free_end (u16 LE)
// [8..16]  parent_page_id (u64 LE)
// [16..24] first_child (u64 LE)
const OFF_INT_FIRST_CHILD: usize = 16;

// ── Error Handling ────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum BTreeError {
    BufferPool(BufferPoolError),
    Corrupt(String),
    KeyTooLarge(usize),
    ValueTooLarge(usize),
    EmptyKey,
    PageTypeMismatch { expected: u8, found: u8 },
    PageOutOfRange(u64),
}

impl fmt::Display for BTreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BufferPool(e) => write!(f, "buffer pool error: {e}"),
            Self::Corrupt(msg) => write!(f, "btree corruption: {msg}"),
            Self::KeyTooLarge(len) => write!(
                f,
                "btree key too large: {len} bytes (max {MAX_BTREE_KEY_BYTES})"
            ),
            Self::ValueTooLarge(len) => write!(
                f,
                "btree value too large: {len} bytes (max {MAX_BTREE_VALUE_BYTES})"
            ),
            Self::EmptyKey => f.write_str("btree key cannot be empty"),
            Self::PageTypeMismatch { expected, found } => {
                write!(
                    f,
                    "btree page type mismatch: expected 0x{expected:02x}, found 0x{found:02x}"
                )
            }
            Self::PageOutOfRange(id) => write!(f, "btree page id out of range: {id}"),
        }
    }
}

impl std::error::Error for BTreeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::BufferPool(e) => Some(e),
            _ => None,
        }
    }
}

impl From<BufferPoolError> for BTreeError {
    fn from(err: BufferPoolError) -> Self {
        Self::BufferPool(err)
    }
}

// ── Raw Page Helpers ──────────────────────────────────────────────────────────

#[inline]
fn read_u16(buf: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(buf[offset..offset + 2].try_into().unwrap())
}

#[inline]
fn write_u16(buf: &mut [u8], offset: usize, val: u16) {
    buf[offset..offset + 2].copy_from_slice(&val.to_le_bytes());
}

#[inline]
fn read_u64(buf: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(buf[offset..offset + 8].try_into().unwrap())
}

#[inline]
fn write_u64(buf: &mut [u8], offset: usize, val: u64) {
    buf[offset..offset + 8].copy_from_slice(&val.to_le_bytes());
}

// ── Node Types & Low-Level Operations ─────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeafEntry {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InternalEntry {
    pub key: Vec<u8>,
    pub child_page_id: u64,
}

/// Key-value byte pair returned from B+Tree queries.
pub type KeyValuePair = (Vec<u8>, Vec<u8>);

/// Error returned when a node does not have sufficient space to accommodate an entry.
#[derive(Debug, PartialEq, Eq)]
pub struct NodeFull;

pub struct LeafNode;

impl LeafNode {
    pub fn init(buf: &mut [u8], parent: u64, prev_leaf: u64, next_leaf: u64) {
        let ps = buf.len() as u16;
        buf.fill(0);
        buf[OFF_TYPE] = PAGE_TYPE_LEAF;
        buf[OFF_FLAGS] = 0;
        write_u16(buf, OFF_NUM_KEYS, 0);
        write_u16(buf, OFF_FREE_START, LEAF_HEADER_LEN as u16);
        write_u16(buf, OFF_FREE_END, ps);
        write_u64(buf, OFF_PARENT, parent);
        write_u64(buf, OFF_LEAF_PREV, prev_leaf);
        write_u64(buf, OFF_LEAF_NEXT, next_leaf);
    }

    pub fn validate(buf: &[u8]) -> Result<(), BTreeError> {
        if buf.len() < LEAF_HEADER_LEN {
            return Err(BTreeError::Corrupt("leaf page buffer too small".into()));
        }
        let ptype = buf[OFF_TYPE];
        if ptype != PAGE_TYPE_LEAF {
            return Err(BTreeError::PageTypeMismatch {
                expected: PAGE_TYPE_LEAF,
                found: ptype,
            });
        }
        let num_keys = read_u16(buf, OFF_NUM_KEYS) as usize;
        let free_start = read_u16(buf, OFF_FREE_START) as usize;
        let free_end = read_u16(buf, OFF_FREE_END) as usize;
        let expected_free_start = LEAF_HEADER_LEN + num_keys * LEAF_SLOT_LEN;

        if free_start != expected_free_start || free_start > free_end || free_end > buf.len() {
            return Err(BTreeError::Corrupt(
                "leaf page header offsets invalid".into(),
            ));
        }
        Ok(())
    }

    pub fn num_keys(buf: &[u8]) -> u16 {
        read_u16(buf, OFF_NUM_KEYS)
    }

    pub fn parent(buf: &[u8]) -> u64 {
        read_u64(buf, OFF_PARENT)
    }

    pub fn set_parent(buf: &mut [u8], parent: u64) {
        write_u64(buf, OFF_PARENT, parent);
    }

    pub fn prev_leaf(buf: &[u8]) -> u64 {
        read_u64(buf, OFF_LEAF_PREV)
    }

    pub fn set_prev_leaf(buf: &mut [u8], prev: u64) {
        write_u64(buf, OFF_LEAF_PREV, prev);
    }

    pub fn next_leaf(buf: &[u8]) -> u64 {
        read_u64(buf, OFF_LEAF_NEXT)
    }

    pub fn set_next_leaf(buf: &mut [u8], next: u64) {
        write_u64(buf, OFF_LEAF_NEXT, next);
    }

    pub fn get_key(buf: &[u8], slot_idx: usize) -> &[u8] {
        let slot_off = LEAF_HEADER_LEN + slot_idx * LEAF_SLOT_LEN;
        let key_off = read_u16(buf, slot_off) as usize;
        let key_len = read_u16(buf, slot_off + 2) as usize;
        &buf[key_off..key_off + key_len]
    }

    pub fn get_value(buf: &[u8], slot_idx: usize) -> &[u8] {
        let slot_off = LEAF_HEADER_LEN + slot_idx * LEAF_SLOT_LEN;
        let val_off = read_u16(buf, slot_off + 4) as usize;
        let val_len = read_u16(buf, slot_off + 6) as usize;
        &buf[val_off..val_off + val_len]
    }

    pub fn get_entry(buf: &[u8], slot_idx: usize) -> LeafEntry {
        LeafEntry {
            key: Self::get_key(buf, slot_idx).to_vec(),
            value: Self::get_value(buf, slot_idx).to_vec(),
        }
    }

    pub fn read_all_entries(buf: &[u8]) -> Vec<LeafEntry> {
        let count = Self::num_keys(buf) as usize;
        let mut entries = Vec::with_capacity(count);
        for i in 0..count {
            entries.push(Self::get_entry(buf, i));
        }
        entries
    }

    pub fn binary_search(buf: &[u8], key: &[u8]) -> Result<usize, usize> {
        let count = Self::num_keys(buf) as usize;
        let mut low = 0;
        let mut high = count;
        while low < high {
            let mid = low + (high - low) / 2;
            let mid_key = Self::get_key(buf, mid);
            match mid_key.cmp(key) {
                Ordering::Less => low = mid + 1,
                Ordering::Greater => high = mid,
                Ordering::Equal => return Ok(mid),
            }
        }
        Err(low)
    }

    /// Check if inserting an entry of given key and value size will fit in the leaf page.
    pub fn can_fit(buf: &[u8], key_len: usize, val_len: usize) -> bool {
        let count = Self::num_keys(buf) as usize;
        let total_slots_size = LEAF_HEADER_LEN + (count + 1) * LEAF_SLOT_LEN;
        let current_payloads: usize = (0..count)
            .map(|i| {
                let slot_off = LEAF_HEADER_LEN + i * LEAF_SLOT_LEN;
                let klen = read_u16(buf, slot_off + 2) as usize;
                let vlen = read_u16(buf, slot_off + 6) as usize;
                klen + vlen
            })
            .sum();
        let needed = total_slots_size + current_payloads + key_len + val_len;
        needed <= buf.len()
    }

    /// Rebuild all entries into the leaf page compactly.
    pub fn rebuild(buf: &mut [u8], entries: &[LeafEntry], parent: u64, prev: u64, next: u64) {
        Self::init(buf, parent, prev, next);
        let ps = buf.len();
        let count = entries.len();
        write_u16(buf, OFF_NUM_KEYS, count as u16);
        let free_start = LEAF_HEADER_LEN + count * LEAF_SLOT_LEN;
        write_u16(buf, OFF_FREE_START, free_start as u16);

        let mut curr_end = ps;
        for (i, entry) in entries.iter().enumerate() {
            let klen = entry.key.len();
            let vlen = entry.value.len();

            curr_end -= vlen;
            let val_off = curr_end;
            buf[val_off..val_off + vlen].copy_from_slice(&entry.value);

            curr_end -= klen;
            let key_off = curr_end;
            buf[key_off..key_off + klen].copy_from_slice(&entry.key);

            let slot_off = LEAF_HEADER_LEN + i * LEAF_SLOT_LEN;
            write_u16(buf, slot_off, key_off as u16);
            write_u16(buf, slot_off + 2, klen as u16);
            write_u16(buf, slot_off + 4, val_off as u16);
            write_u16(buf, slot_off + 6, vlen as u16);
        }
        write_u16(buf, OFF_FREE_END, curr_end as u16);
    }

    /// Insert or overwrite an entry in the leaf page. Returns Ok(()) or Err(NodeFull) if full.
    pub fn insert_or_update(buf: &mut [u8], key: &[u8], value: &[u8]) -> Result<(), NodeFull> {
        let mut entries = Self::read_all_entries(buf);
        match entries.binary_search_by(|e| e.key.as_slice().cmp(key)) {
            Ok(idx) => {
                entries[idx].value = value.to_vec();
            }
            Err(idx) => {
                entries.insert(
                    idx,
                    LeafEntry {
                        key: key.to_vec(),
                        value: value.to_vec(),
                    },
                );
            }
        }

        let needed_payload: usize = entries.iter().map(|e| e.key.len() + e.value.len()).sum();
        let needed_total = LEAF_HEADER_LEN + entries.len() * LEAF_SLOT_LEN + needed_payload;
        if needed_total > buf.len() {
            return Err(NodeFull);
        }

        let parent = Self::parent(buf);
        let prev = Self::prev_leaf(buf);
        let next = Self::next_leaf(buf);
        Self::rebuild(buf, &entries, parent, prev, next);
        Ok(())
    }

    /// Delete an entry by key. Returns true if removed.
    pub fn delete_key(buf: &mut [u8], key: &[u8]) -> bool {
        let mut entries = Self::read_all_entries(buf);
        let idx = match entries.binary_search_by(|e| e.key.as_slice().cmp(key)) {
            Ok(i) => i,
            Err(_) => return false,
        };
        entries.remove(idx);
        let parent = Self::parent(buf);
        let prev = Self::prev_leaf(buf);
        let next = Self::next_leaf(buf);
        Self::rebuild(buf, &entries, parent, prev, next);
        true
    }

    /// Calculate total bytes used by header, slots, and payloads.
    pub fn used_bytes(buf: &[u8]) -> usize {
        let count = Self::num_keys(buf) as usize;
        let slots = LEAF_HEADER_LEN + count * LEAF_SLOT_LEN;
        let payloads: usize = (0..count)
            .map(|i| {
                let slot_off = LEAF_HEADER_LEN + i * LEAF_SLOT_LEN;
                let klen = read_u16(buf, slot_off + 2) as usize;
                let vlen = read_u16(buf, slot_off + 6) as usize;
                klen + vlen
            })
            .sum();
        slots + payloads
    }
}

pub struct InternalNode;

impl InternalNode {
    pub fn init(buf: &mut [u8], parent: u64, first_child: u64) {
        let ps = buf.len() as u16;
        buf.fill(0);
        buf[OFF_TYPE] = PAGE_TYPE_INTERNAL;
        buf[OFF_FLAGS] = 0;
        write_u16(buf, OFF_NUM_KEYS, 0);
        write_u16(buf, OFF_FREE_START, INTERNAL_HEADER_LEN as u16);
        write_u16(buf, OFF_FREE_END, ps);
        write_u64(buf, OFF_PARENT, parent);
        write_u64(buf, OFF_INT_FIRST_CHILD, first_child);
    }

    pub fn validate(buf: &[u8]) -> Result<(), BTreeError> {
        if buf.len() < INTERNAL_HEADER_LEN {
            return Err(BTreeError::Corrupt("internal page buffer too small".into()));
        }
        let ptype = buf[OFF_TYPE];
        if ptype != PAGE_TYPE_INTERNAL {
            return Err(BTreeError::PageTypeMismatch {
                expected: PAGE_TYPE_INTERNAL,
                found: ptype,
            });
        }
        let num_keys = read_u16(buf, OFF_NUM_KEYS) as usize;
        let free_start = read_u16(buf, OFF_FREE_START) as usize;
        let free_end = read_u16(buf, OFF_FREE_END) as usize;
        let expected_free_start = INTERNAL_HEADER_LEN + num_keys * INTERNAL_SLOT_LEN;

        if free_start != expected_free_start || free_start > free_end || free_end > buf.len() {
            return Err(BTreeError::Corrupt(
                "internal page header offsets invalid".into(),
            ));
        }
        Ok(())
    }

    pub fn num_keys(buf: &[u8]) -> u16 {
        read_u16(buf, OFF_NUM_KEYS)
    }

    pub fn parent(buf: &[u8]) -> u64 {
        read_u64(buf, OFF_PARENT)
    }

    pub fn set_parent(buf: &mut [u8], parent: u64) {
        write_u64(buf, OFF_PARENT, parent);
    }

    pub fn first_child(buf: &[u8]) -> u64 {
        read_u64(buf, OFF_INT_FIRST_CHILD)
    }

    pub fn set_first_child(buf: &mut [u8], child: u64) {
        write_u64(buf, OFF_INT_FIRST_CHILD, child);
    }

    pub fn get_key(buf: &[u8], slot_idx: usize) -> &[u8] {
        let slot_off = INTERNAL_HEADER_LEN + slot_idx * INTERNAL_SLOT_LEN;
        let key_off = read_u16(buf, slot_off) as usize;
        let key_len = read_u16(buf, slot_off + 2) as usize;
        &buf[key_off..key_off + key_len]
    }

    pub fn get_child_page_id(buf: &[u8], slot_idx: usize) -> u64 {
        let slot_off = INTERNAL_HEADER_LEN + slot_idx * INTERNAL_SLOT_LEN;
        read_u64(buf, slot_off + 4)
    }

    pub fn get_entry(buf: &[u8], slot_idx: usize) -> InternalEntry {
        InternalEntry {
            key: Self::get_key(buf, slot_idx).to_vec(),
            child_page_id: Self::get_child_page_id(buf, slot_idx),
        }
    }

    pub fn read_all_entries(buf: &[u8]) -> Vec<InternalEntry> {
        let count = Self::num_keys(buf) as usize;
        let mut entries = Vec::with_capacity(count);
        for i in 0..count {
            entries.push(Self::get_entry(buf, i));
        }
        entries
    }

    /// All child page IDs managed by this internal node (first_child + all slot child_page_ids).
    pub fn all_children(buf: &[u8]) -> Vec<u64> {
        let count = Self::num_keys(buf) as usize;
        let mut children = Vec::with_capacity(count + 1);
        children.push(Self::first_child(buf));
        for i in 0..count {
            children.push(Self::get_child_page_id(buf, i));
        }
        children
    }

    /// Find the target child page ID for a search key.
    pub fn find_child(buf: &[u8], key: &[u8]) -> (u64, usize) {
        let count = Self::num_keys(buf) as usize;
        let mut low = 0;
        let mut high = count;
        while low < high {
            let mid = low + (high - low) / 2;
            let mid_key = Self::get_key(buf, mid);
            match mid_key.cmp(key) {
                Ordering::Less => low = mid + 1,
                Ordering::Greater => high = mid,
                Ordering::Equal => {
                    // Exact match goes to right child (slot mid)
                    return (Self::get_child_page_id(buf, mid), mid + 1);
                }
            }
        }
        if low == 0 {
            (Self::first_child(buf), 0)
        } else {
            (Self::get_child_page_id(buf, low - 1), low)
        }
    }

    /// Rebuild all entries into the internal page compactly.
    pub fn rebuild(buf: &mut [u8], entries: &[InternalEntry], parent: u64, first_child: u64) {
        Self::init(buf, parent, first_child);
        let ps = buf.len();
        let count = entries.len();
        write_u16(buf, OFF_NUM_KEYS, count as u16);
        let free_start = INTERNAL_HEADER_LEN + count * INTERNAL_SLOT_LEN;
        write_u16(buf, OFF_FREE_START, free_start as u16);

        let mut curr_end = ps;
        for (i, entry) in entries.iter().enumerate() {
            let klen = entry.key.len();
            curr_end -= klen;
            let key_off = curr_end;
            buf[key_off..key_off + klen].copy_from_slice(&entry.key);

            let slot_off = INTERNAL_HEADER_LEN + i * INTERNAL_SLOT_LEN;
            write_u16(buf, slot_off, key_off as u16);
            write_u16(buf, slot_off + 2, klen as u16);
            write_u64(buf, slot_off + 4, entry.child_page_id);
        }
        write_u16(buf, OFF_FREE_END, curr_end as u16);
    }

    /// Insert an entry (key and right child) in sorted order. Returns Ok(()) or Err(NodeFull) if full.
    pub fn insert_entry(buf: &mut [u8], key: &[u8], child_page_id: u64) -> Result<(), NodeFull> {
        let mut entries = Self::read_all_entries(buf);
        let idx = match entries.binary_search_by(|e| e.key.as_slice().cmp(key)) {
            Ok(i) => i,
            Err(i) => i,
        };
        entries.insert(
            idx,
            InternalEntry {
                key: key.to_vec(),
                child_page_id,
            },
        );

        let needed_payload: usize = entries.iter().map(|e| e.key.len()).sum();
        let needed_total = INTERNAL_HEADER_LEN + entries.len() * INTERNAL_SLOT_LEN + needed_payload;
        if needed_total > buf.len() {
            return Err(NodeFull);
        }

        let parent = Self::parent(buf);
        let first_child = Self::first_child(buf);
        Self::rebuild(buf, &entries, parent, first_child);
        Ok(())
    }

    /// Calculate total bytes used by header, slots, and payloads.
    pub fn used_bytes(buf: &[u8]) -> usize {
        let count = Self::num_keys(buf) as usize;
        let slots = INTERNAL_HEADER_LEN + count * INTERNAL_SLOT_LEN;
        let payloads: usize = (0..count)
            .map(|i| {
                let slot_off = INTERNAL_HEADER_LEN + i * INTERNAL_SLOT_LEN;
                read_u16(buf, slot_off + 2) as usize
            })
            .sum();
        slots + payloads
    }
}

// ── B+Tree Statistics ─────────────────────────────────────────────────────────

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BTreeStats {
    pub depth: usize,
    pub leaf_pages: usize,
    pub internal_pages: usize,
    pub total_keys: usize,
    pub min_key: Option<Vec<u8>>,
    pub max_key: Option<Vec<u8>>,
}

// ── BTree Main Structure ──────────────────────────────────────────────────────

/// Persistent On-Disk B+Tree Index.
#[derive(Clone)]
pub struct BTree {
    bpm: BufferPoolManager,
    root_page_id: Arc<AtomicU64>,
}

impl BTree {
    /// Create a new B+Tree backed by the [`BufferPoolManager`].
    /// Allocates an initial root leaf page.
    pub fn create(bpm: BufferPoolManager) -> Result<Self, BTreeError> {
        let (root_id, mut root_guard) = bpm.new_page()?;
        LeafNode::init(&mut root_guard, 0, 0, 0);
        drop(root_guard);
        Ok(Self {
            bpm,
            root_page_id: Arc::new(AtomicU64::new(root_id)),
        })
    }

    /// Open an existing B+Tree rooted at `root_page_id`.
    pub fn open(bpm: BufferPoolManager, root_page_id: u64) -> Result<Self, BTreeError> {
        if root_page_id == 0 || root_page_id >= bpm.page_count() {
            return Err(BTreeError::PageOutOfRange(root_page_id));
        }
        let guard = bpm.fetch_page(root_page_id)?;
        let ptype = guard[OFF_TYPE];
        if ptype != PAGE_TYPE_LEAF && ptype != PAGE_TYPE_INTERNAL {
            return Err(BTreeError::Corrupt(format!(
                "invalid btree root page type: 0x{ptype:02x}"
            )));
        }
        drop(guard);
        Ok(Self {
            bpm,
            root_page_id: Arc::new(AtomicU64::new(root_page_id)),
        })
    }

    /// Current root page ID of the tree.
    pub fn root_page_id(&self) -> u64 {
        self.root_page_id.load(AtomicOrdering::Acquire)
    }

    /// Access the underlying [`BufferPoolManager`].
    pub fn buffer_pool(&self) -> &BufferPoolManager {
        &self.bpm
    }

    // ── Search & Point Queries ────────────────────────────────────────────────

    /// Find the value associated with `key`. Returns `None` if not found.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, BTreeError> {
        if key.is_empty() {
            return Err(BTreeError::EmptyKey);
        }
        if key.len() > MAX_BTREE_KEY_BYTES {
            return Err(BTreeError::KeyTooLarge(key.len()));
        }

        let leaf_id = self.find_leaf_page_id(key)?;
        let guard = self.bpm.fetch_page(leaf_id)?;
        LeafNode::validate(&guard)?;

        match LeafNode::binary_search(&guard, key) {
            Ok(idx) => Ok(Some(LeafNode::get_value(&guard, idx).to_vec())),
            Err(_) => Ok(None),
        }
    }

    /// Check whether `key` exists in the B+Tree.
    pub fn contains_key(&self, key: &[u8]) -> Result<bool, BTreeError> {
        self.get(key).map(|opt| opt.is_some())
    }

    /// Total number of keys currently stored across all leaves.
    pub fn len(&self) -> Result<usize, BTreeError> {
        let first_leaf_id = self.find_first_leaf_page_id()?;
        let mut curr_id = first_leaf_id;
        let mut total = 0;

        while curr_id != 0 {
            let guard = self.bpm.fetch_page(curr_id)?;
            LeafNode::validate(&guard)?;
            total += LeafNode::num_keys(&guard) as usize;
            curr_id = LeafNode::next_leaf(&guard);
        }
        Ok(total)
    }

    /// Whether the tree contains 0 keys.
    pub fn is_empty(&self) -> Result<bool, BTreeError> {
        self.len().map(|l| l == 0)
    }

    /// Fetch the smallest key and its value in the tree.
    pub fn first(&self) -> Result<Option<KeyValuePair>, BTreeError> {
        let first_leaf_id = self.find_first_leaf_page_id()?;
        let guard = self.bpm.fetch_page(first_leaf_id)?;
        LeafNode::validate(&guard)?;
        if LeafNode::num_keys(&guard) == 0 {
            Ok(None)
        } else {
            let entry = LeafNode::get_entry(&guard, 0);
            Ok(Some((entry.key, entry.value)))
        }
    }

    /// Fetch the largest key and its value in the tree.
    pub fn last(&self) -> Result<Option<KeyValuePair>, BTreeError> {
        let last_leaf_id = self.find_last_leaf_page_id()?;
        let guard = self.bpm.fetch_page(last_leaf_id)?;
        LeafNode::validate(&guard)?;
        let count = LeafNode::num_keys(&guard);
        if count == 0 {
            Ok(None)
        } else {
            let entry = LeafNode::get_entry(&guard, (count - 1) as usize);
            Ok(Some((entry.key, entry.value)))
        }
    }

    // ── Insert Operations ─────────────────────────────────────────────────────

    /// Insert or update `(key, value)` into the B+Tree.
    pub fn insert(&self, key: &[u8], value: &[u8]) -> Result<(), BTreeError> {
        if key.is_empty() {
            return Err(BTreeError::EmptyKey);
        }
        if key.len() > MAX_BTREE_KEY_BYTES {
            return Err(BTreeError::KeyTooLarge(key.len()));
        }
        if value.len() > MAX_BTREE_VALUE_BYTES {
            return Err(BTreeError::ValueTooLarge(value.len()));
        }

        let leaf_id = self.find_leaf_page_id(key)?;
        let mut leaf_guard = self.bpm.fetch_page_mut(leaf_id)?;
        LeafNode::validate(&leaf_guard)?;

        if LeafNode::insert_or_update(&mut leaf_guard, key, value).is_ok() {
            return Ok(());
        }

        // Leaf is full: split leaf!
        self.split_leaf(leaf_id, leaf_guard, key, value)
    }

    fn split_leaf(
        &self,
        leaf_id: u64,
        mut leaf_guard: PageMutGuard,
        new_key: &[u8],
        new_value: &[u8],
    ) -> Result<(), BTreeError> {
        let mut entries = LeafNode::read_all_entries(&leaf_guard);
        match entries.binary_search_by(|e| e.key.as_slice().cmp(new_key)) {
            Ok(idx) => entries[idx].value = new_value.to_vec(),
            Err(idx) => entries.insert(
                idx,
                LeafEntry {
                    key: new_key.to_vec(),
                    value: new_value.to_vec(),
                },
            ),
        }

        let total = entries.len();
        let mid = total / 2;
        let left_entries = &entries[..mid];
        let right_entries = &entries[mid..];
        let promote_key = right_entries[0].key.clone();

        let parent_id = LeafNode::parent(&leaf_guard);
        let old_next = LeafNode::next_leaf(&leaf_guard);
        let prev = LeafNode::prev_leaf(&leaf_guard);

        // Allocate new right leaf page
        let (right_id, mut right_guard) = self.bpm.new_page()?;
        LeafNode::rebuild(
            &mut right_guard,
            right_entries,
            parent_id,
            leaf_id,
            old_next,
        );
        LeafNode::rebuild(&mut leaf_guard, left_entries, parent_id, prev, right_id);

        // Drop leaf guards before updating adjacent nodes and parent
        drop(leaf_guard);
        drop(right_guard);

        // If old_next exists, update its prev_leaf pointer
        if old_next != 0 {
            let mut next_guard = self.bpm.fetch_page_mut(old_next)?;
            LeafNode::validate(&next_guard)?;
            LeafNode::set_prev_leaf(&mut next_guard, right_id);
        }

        // Promote key to parent
        self.insert_into_parent(leaf_id, &promote_key, right_id)
    }

    fn insert_into_parent(
        &self,
        left_child_id: u64,
        key: &[u8],
        right_child_id: u64,
    ) -> Result<(), BTreeError> {
        let current_root = self.root_page_id();
        let parent_id = {
            let guard = self.bpm.fetch_page(left_child_id)?;
            if guard[OFF_TYPE] == PAGE_TYPE_LEAF {
                LeafNode::parent(&guard)
            } else {
                InternalNode::parent(&guard)
            }
        };

        if left_child_id == current_root || parent_id == 0 {
            // Create a new internal root
            let (new_root_id, mut new_root_guard) = self.bpm.new_page()?;
            InternalNode::init(&mut new_root_guard, 0, left_child_id);
            InternalNode::insert_entry(&mut new_root_guard, key, right_child_id)
                .map_err(|_| BTreeError::Corrupt("failed inserting into new root".into()))?;
            drop(new_root_guard);

            // Update parent pointers in both children
            self.set_parent_pointer(left_child_id, new_root_id)?;
            self.set_parent_pointer(right_child_id, new_root_id)?;

            self.root_page_id
                .store(new_root_id, AtomicOrdering::Release);
            return Ok(());
        }

        let mut parent_guard = self.bpm.fetch_page_mut(parent_id)?;
        InternalNode::validate(&parent_guard)?;

        if InternalNode::insert_entry(&mut parent_guard, key, right_child_id).is_ok() {
            drop(parent_guard);
            self.set_parent_pointer(right_child_id, parent_id)?;
            return Ok(());
        }

        // Internal parent is full: split internal node!
        self.split_internal(parent_id, parent_guard, key, right_child_id)
    }

    fn split_internal(
        &self,
        node_id: u64,
        mut node_guard: PageMutGuard,
        new_key: &[u8],
        new_right_child: u64,
    ) -> Result<(), BTreeError> {
        let mut entries = InternalNode::read_all_entries(&node_guard);
        let first_child = InternalNode::first_child(&node_guard);
        let parent_id = InternalNode::parent(&node_guard);

        let insert_idx = match entries.binary_search_by(|e| e.key.as_slice().cmp(new_key)) {
            Ok(i) => i,
            Err(i) => i,
        };
        entries.insert(
            insert_idx,
            InternalEntry {
                key: new_key.to_vec(),
                child_page_id: new_right_child,
            },
        );

        let total = entries.len();
        let mid = total / 2;
        let promote_entry = entries[mid].clone();

        let left_entries = &entries[..mid];
        let right_entries = &entries[mid + 1..];
        let right_first_child = promote_entry.child_page_id;

        // Allocate new right internal page
        let (right_id, mut right_guard) = self.bpm.new_page()?;
        InternalNode::rebuild(
            &mut right_guard,
            right_entries,
            parent_id,
            right_first_child,
        );
        InternalNode::rebuild(&mut node_guard, left_entries, parent_id, first_child);

        drop(node_guard);
        drop(right_guard);

        // Update parent pointers for all children moved to the right internal node
        self.set_parent_pointer(right_first_child, right_id)?;
        for entry in right_entries {
            self.set_parent_pointer(entry.child_page_id, right_id)?;
        }

        // Promote median key to parent
        self.insert_into_parent(node_id, &promote_entry.key, right_id)
    }

    fn set_parent_pointer(&self, child_id: u64, parent_id: u64) -> Result<(), BTreeError> {
        let mut guard = self.bpm.fetch_page_mut(child_id)?;
        match guard[OFF_TYPE] {
            PAGE_TYPE_LEAF => LeafNode::set_parent(&mut guard, parent_id),
            PAGE_TYPE_INTERNAL => InternalNode::set_parent(&mut guard, parent_id),
            other => {
                return Err(BTreeError::Corrupt(format!(
                    "unknown page type: 0x{other:02x}"
                )));
            }
        }
        Ok(())
    }

    // ── Delete & Rebalancing ──────────────────────────────────────────────────

    /// Delete `key` from the tree. Returns `true` if removed, `false` if not found.
    pub fn delete(&self, key: &[u8]) -> Result<bool, BTreeError> {
        if key.is_empty() {
            return Err(BTreeError::EmptyKey);
        }
        if key.len() > MAX_BTREE_KEY_BYTES {
            return Err(BTreeError::KeyTooLarge(key.len()));
        }

        let leaf_id = self.find_leaf_page_id(key)?;
        let mut leaf_guard = self.bpm.fetch_page_mut(leaf_id)?;
        LeafNode::validate(&leaf_guard)?;

        let removed = LeafNode::delete_key(&mut leaf_guard, key);
        if !removed {
            return Ok(false);
        }

        let root_id = self.root_page_id();
        if leaf_id == root_id {
            // Leaf root requires no further rebalancing
            return Ok(true);
        }

        let leaf_used = LeafNode::used_bytes(&leaf_guard);
        let page_size = leaf_guard.len();
        let underflow_threshold = page_size / 3;

        if leaf_used >= underflow_threshold && LeafNode::num_keys(&leaf_guard) > 0 {
            return Ok(true);
        }

        drop(leaf_guard);
        self.rebalance_leaf(leaf_id)?;
        Ok(true)
    }

    fn rebalance_leaf(&self, leaf_id: u64) -> Result<(), BTreeError> {
        let parent_id = {
            let guard = self.bpm.fetch_page(leaf_id)?;
            LeafNode::parent(&guard)
        };
        if parent_id == 0 {
            return Ok(());
        }

        let parent_guard = self.bpm.fetch_page(parent_id)?;
        InternalNode::validate(&parent_guard)?;
        let children = InternalNode::all_children(&parent_guard);
        let child_idx = children.iter().position(|&c| c == leaf_id).ok_or_else(|| {
            BTreeError::Corrupt(format!("leaf {leaf_id} not found in parent {parent_id}"))
        })?;
        drop(parent_guard);

        // Try left sibling first
        if child_idx > 0 {
            let left_sibling_id = children[child_idx - 1];
            if self.try_redistribute_leaf_left(
                left_sibling_id,
                leaf_id,
                parent_id,
                child_idx - 1,
            )? {
                return Ok(());
            }
            // Merge leaf into left sibling
            self.merge_leaf(left_sibling_id, leaf_id, parent_id, child_idx - 1)?;
            return Ok(());
        }

        // Otherwise try right sibling
        if child_idx + 1 < children.len() {
            let right_sibling_id = children[child_idx + 1];
            if self.try_redistribute_leaf_right(leaf_id, right_sibling_id, parent_id, child_idx)? {
                return Ok(());
            }
            // Merge right sibling into leaf
            self.merge_leaf(leaf_id, right_sibling_id, parent_id, child_idx)?;
            return Ok(());
        }

        Ok(())
    }

    fn try_redistribute_leaf_left(
        &self,
        left_id: u64,
        right_id: u64,
        parent_id: u64,
        parent_slot_idx: usize,
    ) -> Result<bool, BTreeError> {
        let mut left_guard = self.bpm.fetch_page_mut(left_id)?;
        let mut right_guard = self.bpm.fetch_page_mut(right_id)?;
        let mut left_entries = LeafNode::read_all_entries(&left_guard);
        let mut right_entries = LeafNode::read_all_entries(&right_guard);

        if left_entries.len() <= 1 {
            return Ok(false);
        }

        let borrowed = left_entries.pop().unwrap();
        right_entries.insert(0, borrowed);

        let page_size = left_guard.len();
        let right_payload: usize = right_entries
            .iter()
            .map(|e| e.key.len() + e.value.len())
            .sum();
        let right_needed = LEAF_HEADER_LEN + right_entries.len() * LEAF_SLOT_LEN + right_payload;
        if right_needed > page_size {
            return Ok(false);
        }

        let left_prev = LeafNode::prev_leaf(&left_guard);
        let right_next = LeafNode::next_leaf(&right_guard);

        LeafNode::rebuild(
            &mut left_guard,
            &left_entries,
            parent_id,
            left_prev,
            right_id,
        );
        LeafNode::rebuild(
            &mut right_guard,
            &right_entries,
            parent_id,
            left_id,
            right_next,
        );
        let new_separator = right_entries[0].key.clone();
        drop(left_guard);
        drop(right_guard);

        // Update separator in parent
        let mut parent_guard = self.bpm.fetch_page_mut(parent_id)?;
        let mut parent_entries = InternalNode::read_all_entries(&parent_guard);
        parent_entries[parent_slot_idx].key = new_separator;
        let p_parent = InternalNode::parent(&parent_guard);
        let p_first = InternalNode::first_child(&parent_guard);
        InternalNode::rebuild(&mut parent_guard, &parent_entries, p_parent, p_first);

        Ok(true)
    }

    fn try_redistribute_leaf_right(
        &self,
        left_id: u64,
        right_id: u64,
        parent_id: u64,
        parent_slot_idx: usize,
    ) -> Result<bool, BTreeError> {
        let mut left_guard = self.bpm.fetch_page_mut(left_id)?;
        let mut right_guard = self.bpm.fetch_page_mut(right_id)?;
        let mut left_entries = LeafNode::read_all_entries(&left_guard);
        let mut right_entries = LeafNode::read_all_entries(&right_guard);

        if right_entries.len() <= 1 {
            return Ok(false);
        }

        let borrowed = right_entries.remove(0);
        left_entries.push(borrowed);

        let page_size = left_guard.len();
        let left_payload: usize = left_entries
            .iter()
            .map(|e| e.key.len() + e.value.len())
            .sum();
        let left_needed = LEAF_HEADER_LEN + left_entries.len() * LEAF_SLOT_LEN + left_payload;
        if left_needed > page_size {
            return Ok(false);
        }

        let left_prev = LeafNode::prev_leaf(&left_guard);
        let right_next = LeafNode::next_leaf(&right_guard);

        LeafNode::rebuild(
            &mut left_guard,
            &left_entries,
            parent_id,
            left_prev,
            right_id,
        );
        LeafNode::rebuild(
            &mut right_guard,
            &right_entries,
            parent_id,
            left_id,
            right_next,
        );
        let new_separator = right_entries[0].key.clone();
        drop(left_guard);
        drop(right_guard);

        // Update separator in parent
        let mut parent_guard = self.bpm.fetch_page_mut(parent_id)?;
        let mut parent_entries = InternalNode::read_all_entries(&parent_guard);
        parent_entries[parent_slot_idx].key = new_separator;
        let p_parent = InternalNode::parent(&parent_guard);
        let p_first = InternalNode::first_child(&parent_guard);
        InternalNode::rebuild(&mut parent_guard, &parent_entries, p_parent, p_first);

        Ok(true)
    }

    fn merge_leaf(
        &self,
        left_id: u64,
        right_id: u64,
        parent_id: u64,
        parent_slot_idx: usize,
    ) -> Result<(), BTreeError> {
        let mut left_guard = self.bpm.fetch_page_mut(left_id)?;
        let right_guard = self.bpm.fetch_page(right_id)?;
        let mut left_entries = LeafNode::read_all_entries(&left_guard);
        let right_entries = LeafNode::read_all_entries(&right_guard);
        let right_next = LeafNode::next_leaf(&right_guard);
        let left_prev = LeafNode::prev_leaf(&left_guard);

        left_entries.extend(right_entries);
        LeafNode::rebuild(
            &mut left_guard,
            &left_entries,
            parent_id,
            left_prev,
            right_next,
        );
        drop(left_guard);
        drop(right_guard);

        // Fix linked list next pointer
        if right_next != 0 {
            let mut next_guard = self.bpm.fetch_page_mut(right_next)?;
            LeafNode::set_prev_leaf(&mut next_guard, left_id);
        }

        // Delete right page from buffer pool and free on disk
        self.bpm.delete_page(right_id)?;

        // Remove entry from parent
        self.remove_parent_entry(parent_id, parent_slot_idx)
    }

    fn remove_parent_entry(&self, parent_id: u64, slot_idx: usize) -> Result<(), BTreeError> {
        let mut parent_guard = self.bpm.fetch_page_mut(parent_id)?;
        InternalNode::validate(&parent_guard)?;
        let mut entries = InternalNode::read_all_entries(&parent_guard);
        entries.remove(slot_idx);

        let root_id = self.root_page_id();
        if parent_id == root_id {
            if entries.is_empty() {
                // Root has 0 keys: new root becomes its first_child!
                let new_root_id = InternalNode::first_child(&parent_guard);
                drop(parent_guard);

                self.set_parent_pointer(new_root_id, 0)?;
                self.bpm.delete_page(root_id)?;
                self.root_page_id
                    .store(new_root_id, AtomicOrdering::Release);
            } else {
                let p = InternalNode::parent(&parent_guard);
                let first = InternalNode::first_child(&parent_guard);
                InternalNode::rebuild(&mut parent_guard, &entries, p, first);
            }
            return Ok(());
        }

        let p = InternalNode::parent(&parent_guard);
        let first = InternalNode::first_child(&parent_guard);
        InternalNode::rebuild(&mut parent_guard, &entries, p, first);

        let used = InternalNode::used_bytes(&parent_guard);
        let page_size = parent_guard.len();
        let threshold = page_size / 3;

        if used < threshold || entries.is_empty() {
            drop(parent_guard);
            self.rebalance_internal(parent_id)?;
        }

        Ok(())
    }

    fn rebalance_internal(&self, node_id: u64) -> Result<(), BTreeError> {
        let parent_id = {
            let guard = self.bpm.fetch_page(node_id)?;
            InternalNode::parent(&guard)
        };
        if parent_id == 0 {
            return Ok(());
        }

        let parent_guard = self.bpm.fetch_page(parent_id)?;
        InternalNode::validate(&parent_guard)?;
        let children = InternalNode::all_children(&parent_guard);
        let child_idx = children.iter().position(|&c| c == node_id).ok_or_else(|| {
            BTreeError::Corrupt(format!(
                "internal node {node_id} not found in parent {parent_id}"
            ))
        })?;
        drop(parent_guard);

        if child_idx > 0 {
            let left_sibling_id = children[child_idx - 1];
            if self.try_redistribute_internal_left(
                left_sibling_id,
                node_id,
                parent_id,
                child_idx - 1,
            )? {
                return Ok(());
            }
            self.merge_internal(left_sibling_id, node_id, parent_id, child_idx - 1)?;
            return Ok(());
        }

        if child_idx + 1 < children.len() {
            let right_sibling_id = children[child_idx + 1];
            if self.try_redistribute_internal_right(
                node_id,
                right_sibling_id,
                parent_id,
                child_idx,
            )? {
                return Ok(());
            }
            self.merge_internal(node_id, right_sibling_id, parent_id, child_idx)?;
            return Ok(());
        }

        Ok(())
    }

    fn try_redistribute_internal_left(
        &self,
        left_id: u64,
        right_id: u64,
        parent_id: u64,
        parent_slot_idx: usize,
    ) -> Result<bool, BTreeError> {
        let mut left_guard = self.bpm.fetch_page_mut(left_id)?;
        let mut right_guard = self.bpm.fetch_page_mut(right_id)?;
        let mut left_entries = InternalNode::read_all_entries(&left_guard);
        let mut right_entries = InternalNode::read_all_entries(&right_guard);

        if left_entries.is_empty() {
            return Ok(false);
        }

        let mut parent_guard = self.bpm.fetch_page_mut(parent_id)?;
        let mut parent_entries = InternalNode::read_all_entries(&parent_guard);

        let parent_key = parent_entries[parent_slot_idx].key.clone();
        let borrowed = left_entries.pop().unwrap();

        // Right's new first child was right's old first child; borrowed entry's child becomes new right first child
        let old_right_first = InternalNode::first_child(&right_guard);
        let new_right_first = borrowed.child_page_id;

        right_entries.insert(
            0,
            InternalEntry {
                key: parent_key,
                child_page_id: old_right_first,
            },
        );

        parent_entries[parent_slot_idx].key = borrowed.key;

        let left_first = InternalNode::first_child(&left_guard);
        InternalNode::rebuild(&mut left_guard, &left_entries, parent_id, left_first);
        InternalNode::rebuild(&mut right_guard, &right_entries, parent_id, new_right_first);

        let p = InternalNode::parent(&parent_guard);
        let first = InternalNode::first_child(&parent_guard);
        InternalNode::rebuild(&mut parent_guard, &parent_entries, p, first);

        drop(left_guard);
        drop(right_guard);
        drop(parent_guard);

        self.set_parent_pointer(new_right_first, right_id)?;
        Ok(true)
    }

    fn try_redistribute_internal_right(
        &self,
        left_id: u64,
        right_id: u64,
        parent_id: u64,
        parent_slot_idx: usize,
    ) -> Result<bool, BTreeError> {
        let mut left_guard = self.bpm.fetch_page_mut(left_id)?;
        let mut right_guard = self.bpm.fetch_page_mut(right_id)?;
        let mut left_entries = InternalNode::read_all_entries(&left_guard);
        let mut right_entries = InternalNode::read_all_entries(&right_guard);

        if right_entries.is_empty() {
            return Ok(false);
        }

        let mut parent_guard = self.bpm.fetch_page_mut(parent_id)?;
        let mut parent_entries = InternalNode::read_all_entries(&parent_guard);

        let parent_key = parent_entries[parent_slot_idx].key.clone();
        let right_first_child = InternalNode::first_child(&right_guard);
        let borrowed = right_entries.remove(0);

        left_entries.push(InternalEntry {
            key: parent_key,
            child_page_id: right_first_child,
        });

        parent_entries[parent_slot_idx].key = borrowed.key;
        let new_right_first = borrowed.child_page_id;

        let left_first = InternalNode::first_child(&left_guard);
        InternalNode::rebuild(&mut left_guard, &left_entries, parent_id, left_first);
        InternalNode::rebuild(&mut right_guard, &right_entries, parent_id, new_right_first);

        let p = InternalNode::parent(&parent_guard);
        let first = InternalNode::first_child(&parent_guard);
        InternalNode::rebuild(&mut parent_guard, &parent_entries, p, first);

        drop(left_guard);
        drop(right_guard);
        drop(parent_guard);

        self.set_parent_pointer(right_first_child, left_id)?;
        Ok(true)
    }

    fn merge_internal(
        &self,
        left_id: u64,
        right_id: u64,
        parent_id: u64,
        parent_slot_idx: usize,
    ) -> Result<(), BTreeError> {
        let mut left_guard = self.bpm.fetch_page_mut(left_id)?;
        let right_guard = self.bpm.fetch_page(right_id)?;
        let parent_guard = self.bpm.fetch_page(parent_id)?;

        let parent_entries = InternalNode::read_all_entries(&parent_guard);
        let separator_key = parent_entries[parent_slot_idx].key.clone();
        drop(parent_guard);

        let mut left_entries = InternalNode::read_all_entries(&left_guard);
        let right_entries = InternalNode::read_all_entries(&right_guard);
        let right_first_child = InternalNode::first_child(&right_guard);

        left_entries.push(InternalEntry {
            key: separator_key,
            child_page_id: right_first_child,
        });
        left_entries.extend(right_entries.clone());

        let left_first = InternalNode::first_child(&left_guard);
        InternalNode::rebuild(&mut left_guard, &left_entries, parent_id, left_first);
        drop(left_guard);
        drop(right_guard);

        // Update parent pointers for all children moved to left_id
        self.set_parent_pointer(right_first_child, left_id)?;
        for entry in &right_entries {
            self.set_parent_pointer(entry.child_page_id, left_id)?;
        }

        self.bpm.delete_page(right_id)?;
        self.remove_parent_entry(parent_id, parent_slot_idx)
    }

    // ── Range Scans & Iterators ───────────────────────────────────────────────

    /// Retrieve up to `limit` entries starting at `start_key` (inclusive).
    pub fn scan(&self, start_key: &[u8], limit: usize) -> Result<Vec<KeyValuePair>, BTreeError> {
        let mut results = Vec::with_capacity(limit.min(1024));
        if limit == 0 {
            return Ok(results);
        }

        let leaf_id = self.find_leaf_page_id(start_key)?;
        let mut curr_id = leaf_id;
        let mut is_first_page = true;

        while curr_id != 0 && results.len() < limit {
            let guard = self.bpm.fetch_page(curr_id)?;
            LeafNode::validate(&guard)?;
            let count = LeafNode::num_keys(&guard) as usize;

            let start_slot = if is_first_page {
                is_first_page = false;
                match LeafNode::binary_search(&guard, start_key) {
                    Ok(i) => i,
                    Err(i) => i,
                }
            } else {
                0
            };

            for slot in start_slot..count {
                let entry = LeafNode::get_entry(&guard, slot);
                results.push((entry.key, entry.value));
                if results.len() == limit {
                    break;
                }
            }

            curr_id = LeafNode::next_leaf(&guard);
        }

        Ok(results)
    }

    /// Create an iterator over a range of keys.
    pub fn range<R>(&self, range: R) -> Result<BTreeRangeIter, BTreeError>
    where
        R: RangeBounds<Vec<u8>>,
    {
        let start_bound = match range.start_bound() {
            Bound::Included(k) => Bound::Included(k.clone()),
            Bound::Excluded(k) => Bound::Excluded(k.clone()),
            Bound::Unbounded => Bound::Unbounded,
        };
        let end_bound = match range.end_bound() {
            Bound::Included(k) => Bound::Included(k.clone()),
            Bound::Excluded(k) => Bound::Excluded(k.clone()),
            Bound::Unbounded => Bound::Unbounded,
        };

        let first_leaf_id = match &start_bound {
            Bound::Included(k) | Bound::Excluded(k) => self.find_leaf_page_id(k)?,
            Bound::Unbounded => self.find_first_leaf_page_id()?,
        };

        let (curr_page_id, curr_slot) = {
            let guard = self.bpm.fetch_page(first_leaf_id)?;
            LeafNode::validate(&guard)?;
            let slot = match &start_bound {
                Bound::Included(k) => match LeafNode::binary_search(&guard, k) {
                    Ok(i) => i,
                    Err(i) => i,
                },
                Bound::Excluded(k) => match LeafNode::binary_search(&guard, k) {
                    Ok(i) => i + 1,
                    Err(i) => i,
                },
                Bound::Unbounded => 0,
            };
            (first_leaf_id, slot)
        };

        Ok(BTreeRangeIter {
            btree: self.clone(),
            curr_page_id,
            curr_slot,
            end_bound,
        })
    }

    // ── Integrity & Invariant Validation ──────────────────────────────────────

    /// Thoroughly validate all structural invariants of the B+Tree.
    pub fn verify_integrity(&self) -> Result<BTreeStats, BTreeError> {
        let root_id = self.root_page_id();
        let mut stats = BTreeStats::default();

        let (leaf_depth, total_keys) = self.verify_subtree(root_id, 0, None, None, &mut stats)?;
        stats.depth = leaf_depth;
        stats.total_keys = total_keys;

        // Verify Leaf Doubly Linked List consistency
        let first_leaf_id = self.find_first_leaf_page_id()?;
        let mut curr_id = first_leaf_id;
        let mut prev_id = 0;
        let mut sequential_keys = 0;
        let mut last_key: Option<Vec<u8>> = None;

        while curr_id != 0 {
            let guard = self.bpm.fetch_page(curr_id)?;
            LeafNode::validate(&guard)?;

            let p = LeafNode::prev_leaf(&guard);
            if p != prev_id {
                return Err(BTreeError::Corrupt(format!(
                    "leaf {curr_id} prev_leaf pointer ({p}) does not match expected ({prev_id})"
                )));
            }

            let count = LeafNode::num_keys(&guard) as usize;
            for i in 0..count {
                let k = LeafNode::get_key(&guard, i);
                if let Some(ref prev_k) = last_key {
                    if k <= prev_k.as_slice() {
                        return Err(BTreeError::Corrupt(format!(
                            "leaf keys not strictly monotonically increasing at leaf {curr_id}, slot {i}"
                        )));
                    }
                } else {
                    stats.min_key = Some(k.to_vec());
                }
                last_key = Some(k.to_vec());
                sequential_keys += 1;
            }

            prev_id = curr_id;
            curr_id = LeafNode::next_leaf(&guard);
        }

        stats.max_key = last_key;
        if sequential_keys != stats.total_keys {
            return Err(BTreeError::Corrupt(format!(
                "sequential leaf key count ({sequential_keys}) does not match tree key count ({})",
                stats.total_keys
            )));
        }

        Ok(stats)
    }

    fn verify_subtree(
        &self,
        node_id: u64,
        depth: usize,
        min_key: Option<&[u8]>,
        max_key: Option<&[u8]>,
        stats: &mut BTreeStats,
    ) -> Result<(usize, usize), BTreeError> {
        let guard = self.bpm.fetch_page(node_id)?;
        let ptype = guard[OFF_TYPE];

        match ptype {
            PAGE_TYPE_LEAF => {
                LeafNode::validate(&guard)?;
                stats.leaf_pages += 1;
                let count = LeafNode::num_keys(&guard) as usize;

                for i in 0..count {
                    let k = LeafNode::get_key(&guard, i);
                    if let Some(min) = min_key
                        && k < min
                    {
                        return Err(BTreeError::Corrupt(format!(
                            "leaf key violates lower separator bound at leaf {node_id}"
                        )));
                    }
                    if let Some(max) = max_key
                        && k >= max
                    {
                        return Err(BTreeError::Corrupt(format!(
                            "leaf key violates upper separator bound at leaf {node_id}"
                        )));
                    }
                }
                Ok((depth, count))
            }
            PAGE_TYPE_INTERNAL => {
                InternalNode::validate(&guard)?;
                stats.internal_pages += 1;
                let children = InternalNode::all_children(&guard);
                let entries = InternalNode::read_all_entries(&guard);
                drop(guard);

                let mut expected_depth: Option<usize> = None;
                let mut total_keys = 0;

                for i in 0..children.len() {
                    let child_id = children[i];
                    let child_min = if i == 0 {
                        min_key
                    } else {
                        Some(entries[i - 1].key.as_slice())
                    };
                    let child_max = if i < entries.len() {
                        Some(entries[i].key.as_slice())
                    } else {
                        max_key
                    };

                    let (child_depth, child_keys) =
                        self.verify_subtree(child_id, depth + 1, child_min, child_max, stats)?;
                    total_keys += child_keys;

                    if let Some(exp) = expected_depth {
                        if child_depth != exp {
                            return Err(BTreeError::Corrupt(format!(
                                "unbalanced tree depth: expected {exp}, got {child_depth} at child {child_id}"
                            )));
                        }
                    } else {
                        expected_depth = Some(child_depth);
                    }
                }

                Ok((expected_depth.unwrap_or(depth), total_keys))
            }
            other => Err(BTreeError::Corrupt(format!(
                "unknown page type 0x{other:02x} at page {node_id}"
            ))),
        }
    }

    // ── Internal Helpers ──────────────────────────────────────────────────────

    fn find_leaf_page_id(&self, key: &[u8]) -> Result<u64, BTreeError> {
        let mut curr_id = self.root_page_id();
        loop {
            let guard = self.bpm.fetch_page(curr_id)?;
            let ptype = guard[OFF_TYPE];
            if ptype == PAGE_TYPE_LEAF {
                return Ok(curr_id);
            }
            if ptype == PAGE_TYPE_INTERNAL {
                InternalNode::validate(&guard)?;
                let (next_id, _) = InternalNode::find_child(&guard, key);
                curr_id = next_id;
            } else {
                return Err(BTreeError::Corrupt(format!(
                    "unexpected page type 0x{ptype:02x} while searching for key"
                )));
            }
        }
    }

    fn find_first_leaf_page_id(&self) -> Result<u64, BTreeError> {
        let mut curr_id = self.root_page_id();
        loop {
            let guard = self.bpm.fetch_page(curr_id)?;
            let ptype = guard[OFF_TYPE];
            if ptype == PAGE_TYPE_LEAF {
                return Ok(curr_id);
            }
            if ptype == PAGE_TYPE_INTERNAL {
                InternalNode::validate(&guard)?;
                curr_id = InternalNode::first_child(&guard);
            } else {
                return Err(BTreeError::Corrupt(format!(
                    "unexpected page type 0x{ptype:02x} finding first leaf"
                )));
            }
        }
    }

    fn find_last_leaf_page_id(&self) -> Result<u64, BTreeError> {
        let mut curr_id = self.root_page_id();
        loop {
            let guard = self.bpm.fetch_page(curr_id)?;
            let ptype = guard[OFF_TYPE];
            if ptype == PAGE_TYPE_LEAF {
                return Ok(curr_id);
            }
            if ptype == PAGE_TYPE_INTERNAL {
                InternalNode::validate(&guard)?;
                let count = InternalNode::num_keys(&guard);
                if count == 0 {
                    curr_id = InternalNode::first_child(&guard);
                } else {
                    curr_id = InternalNode::get_child_page_id(&guard, (count - 1) as usize);
                }
            } else {
                return Err(BTreeError::Corrupt(format!(
                    "unexpected page type 0x{ptype:02x} finding last leaf"
                )));
            }
        }
    }
}

// ── Range Iterator ────────────────────────────────────────────────────────────

/// Memory-bounded bidirectional / range iterator over B+Tree leaf entries.
pub struct BTreeRangeIter {
    btree: BTree,
    curr_page_id: u64,
    curr_slot: usize,
    end_bound: Bound<Vec<u8>>,
}

impl Iterator for BTreeRangeIter {
    type Item = Result<KeyValuePair, BTreeError>;

    fn next(&mut self) -> Option<Self::Item> {
        while self.curr_page_id != 0 {
            let guard = match self.btree.bpm.fetch_page(self.curr_page_id) {
                Ok(g) => g,
                Err(e) => return Some(Err(BTreeError::BufferPool(e))),
            };
            if let Err(err) = LeafNode::validate(&guard) {
                return Some(Err(err));
            }
            let count = LeafNode::num_keys(&guard) as usize;

            if self.curr_slot < count {
                let entry = LeafNode::get_entry(&guard, self.curr_slot);
                self.curr_slot += 1;

                // Check end bound
                match &self.end_bound {
                    Bound::Included(end_k) => {
                        if entry.key > *end_k {
                            self.curr_page_id = 0;
                            return None;
                        }
                    }
                    Bound::Excluded(end_k) => {
                        if entry.key >= *end_k {
                            self.curr_page_id = 0;
                            return None;
                        }
                    }
                    Bound::Unbounded => {}
                }

                return Some(Ok((entry.key, entry.value)));
            }

            // Advance to next leaf page
            self.curr_page_id = LeafNode::next_leaf(&guard);
            self.curr_slot = 0;
        }
        None
    }
}
