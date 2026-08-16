//! Memory-bounded Buffer Pool Manager with CLOCK and LRU-K eviction,
//! WAL-pinned dirty page eviction, and pin/unpin counting.
//!
//! # Architecture
//!
//! ```text
//!  ┌─────────────────────────────────────────────────────────┐
//!  │                   BufferPoolManager                     │
//!  │  ┌───────────────┐  ┌─────────────┐  ┌───────────────┐  │
//!  │  │  Page Table   │  │  Replacer   │  │  Free List    │  │
//!  │  │ PageId -> Fid │  │ Clock/LRU-K │  │ [Fid, ...]    │  │
//!  │  └───────┬───────┘  └──────┬──────┘  └───────┬───────┘  │
//!  │         │                  │                 │          │
//!  │  ┌──────▼──────────────────▼─────────────────▼───────┐  │
//!  │  │ Fixed Frame Pool [Frame 0, Frame 1, ... Frame N]   │  │
//!  │  │ Each frame: RwLock<Data>, pin_count, dirty, lsn   │  │
//!  │  └─────────────────────────┬─────────────────────────┘  │
//!  └────────────────────────────┼────────────────────────────┘
//!                               │ I/O on miss / dirty evict
//!                        ┌──────▼──────┐
//!                        │    Pager    │
//!                        └─────────────┘
//! ```
//!
//! # WAL-Pinned Eviction Invariant
//!
//! Before any dirty page is written to disk (during eviction or manual flush),
//! the Buffer Pool Manager guarantees that `wal_flushed_lsn >= page_lsn`. If
//! the page LSN exceeds the flushed WAL position, a registered WAL sync hook
//! is invoked to synchronize the WAL before the page buffer is flushed to disk.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};

pub use crate::pager::{PAGE_4K, PAGE_8K, Pager, PagerError};

/// Default buffer pool memory budget: 64 MiB.
pub const DEFAULT_MEMORY_BUDGET: usize = 64 * 1024 * 1024;

/// Frame identifier in the buffer pool frame table (0..pool_size).
pub type FrameId = usize;

/// Eviction policy algorithm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EvictionPolicy {
    /// Second-chance CLOCK algorithm.
    Clock,
    /// LRU-K algorithm tracking backward K-th access distance.
    LruK(usize),
}

/// WAL sync callback handler.
pub type WalSyncHook = Arc<dyn Fn(u32) -> Result<(), BufferPoolError> + Send + Sync>;

/// Configuration options for the buffer pool manager.
#[derive(Clone)]
pub struct BufferPoolOptions {
    /// Number of page frames in the pool.
    pub pool_size: usize,
    /// Eviction policy algorithm.
    pub eviction_policy: EvictionPolicy,
    /// Optional WAL synchronization callback.
    pub wal_sync_hook: Option<WalSyncHook>,
}

impl BufferPoolOptions {
    /// Create options from memory budget in bytes and page size.
    pub fn from_budget(budget_bytes: usize, page_size: u32) -> Self {
        let ps = page_size as usize;
        let pool_size = (budget_bytes / ps).max(1);
        Self {
            pool_size,
            eviction_policy: EvictionPolicy::Clock,
            wal_sync_hook: None,
        }
    }

    /// Set the number of frames directly.
    pub fn with_frames(mut self, frames: usize) -> Self {
        self.pool_size = frames.max(1);
        self
    }

    /// Set the eviction policy.
    pub fn with_eviction_policy(mut self, policy: EvictionPolicy) -> Self {
        self.eviction_policy = policy;
        self
    }

    /// Set the WAL sync hook.
    pub fn with_wal_sync_hook<F>(mut self, hook: F) -> Self
    where
        F: Fn(u32) -> Result<(), BufferPoolError> + Send + Sync + 'static,
    {
        self.wal_sync_hook = Some(Arc::new(hook));
        self
    }
}

impl Default for BufferPoolOptions {
    fn default() -> Self {
        Self::from_budget(DEFAULT_MEMORY_BUDGET, PAGE_4K)
    }
}

// ── Replacer Implementations ──────────────────────────────────────────────────

trait Replacer: Send + Sync {
    fn victim(&mut self) -> Option<FrameId>;
    fn pin(&mut self, frame_id: FrameId);
    fn unpin(&mut self, frame_id: FrameId);
    fn record_access(&mut self, frame_id: FrameId);
    fn remove(&mut self, frame_id: FrameId);
}

#[derive(Debug)]
pub struct ClockReplacer {
    capacity: usize,
    hand: usize,
    referenced: Vec<bool>,
    pinned: Vec<bool>,
}

impl ClockReplacer {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            hand: 0,
            referenced: vec![false; capacity],
            pinned: vec![true; capacity],
        }
    }
}

impl Replacer for ClockReplacer {
    fn victim(&mut self) -> Option<FrameId> {
        let unpinned_count = self.pinned.iter().filter(|&&p| !p).count();
        if unpinned_count == 0 {
            return None;
        }

        let max_scans = self.capacity.checked_mul(2).unwrap_or(self.capacity);
        for _ in 0..max_scans {
            let candidate = self.hand;
            self.hand = (self.hand + 1) % self.capacity;

            if !self.pinned[candidate] {
                if self.referenced[candidate] {
                    self.referenced[candidate] = false;
                } else {
                    self.pinned[candidate] = true;
                    return Some(candidate);
                }
            }
        }
        None
    }

    fn pin(&mut self, frame_id: FrameId) {
        if frame_id < self.capacity {
            self.pinned[frame_id] = true;
        }
    }

    fn unpin(&mut self, frame_id: FrameId) {
        if frame_id < self.capacity {
            self.pinned[frame_id] = false;
        }
    }

    fn record_access(&mut self, frame_id: FrameId) {
        if frame_id < self.capacity {
            self.referenced[frame_id] = true;
        }
    }

    fn remove(&mut self, frame_id: FrameId) {
        if frame_id < self.capacity {
            self.pinned[frame_id] = true;
            self.referenced[frame_id] = false;
        }
    }
}

#[derive(Debug)]
pub struct LruKReplacer {
    capacity: usize,
    k: usize,
    current_timestamp: u64,
    history: Vec<VecDeque<u64>>,
    pinned: Vec<bool>,
}

impl LruKReplacer {
    pub fn new(capacity: usize, k: usize) -> Self {
        let k = k.max(1);
        Self {
            capacity,
            k,
            current_timestamp: 0,
            history: vec![VecDeque::with_capacity(k); capacity],
            pinned: vec![true; capacity],
        }
    }
}

impl Replacer for LruKReplacer {
    fn victim(&mut self) -> Option<FrameId> {
        let mut max_distance: Option<u64> = None;
        let mut victim_id: Option<FrameId> = None;
        let mut earliest_inf_timestamp: Option<u64> = None;
        let mut victim_inf_id: Option<FrameId> = None;

        for id in 0..self.capacity {
            if self.pinned[id] {
                continue;
            }

            let hist = &self.history[id];
            if hist.len() < self.k {
                // Backward distance is +inf; break tie with earliest first access time (FIFO).
                let first_access = hist.front().copied().unwrap_or(0);
                if earliest_inf_timestamp.is_none()
                    || first_access < earliest_inf_timestamp.unwrap()
                {
                    earliest_inf_timestamp = Some(first_access);
                    victim_inf_id = Some(id);
                }
            } else if victim_inf_id.is_none() {
                // Backward K-distance is current_timestamp - k_th_access.
                let k_th_access = *hist.front().unwrap();
                let distance = self.current_timestamp.saturating_sub(k_th_access);
                if max_distance.is_none() || distance > max_distance.unwrap() {
                    max_distance = Some(distance);
                    victim_id = Some(id);
                }
            }
        }

        let selected = victim_inf_id.or(victim_id);
        if let Some(id) = selected {
            self.pinned[id] = true;
        }
        selected
    }

    fn pin(&mut self, frame_id: FrameId) {
        if frame_id < self.capacity {
            self.pinned[frame_id] = true;
        }
    }

    fn unpin(&mut self, frame_id: FrameId) {
        if frame_id < self.capacity {
            self.pinned[frame_id] = false;
        }
    }

    fn record_access(&mut self, frame_id: FrameId) {
        if frame_id < self.capacity {
            self.current_timestamp = self.current_timestamp.wrapping_add(1);
            let hist = &mut self.history[frame_id];
            if hist.len() == self.k {
                hist.pop_front();
            }
            hist.push_back(self.current_timestamp);
        }
    }

    fn remove(&mut self, frame_id: FrameId) {
        if frame_id < self.capacity {
            self.pinned[frame_id] = true;
            self.history[frame_id].clear();
        }
    }
}

// ── Frame Table ───────────────────────────────────────────────────────────────

struct Frame {
    _frame_id: FrameId,
    page_idx: AtomicU64,
    pin_count: AtomicUsize,
    is_dirty: AtomicBool,
    page_lsn: AtomicU32,
    data: RwLock<Vec<u8>>,
}

impl Frame {
    const INVALID_PAGE: u64 = u64::MAX;

    fn new(frame_id: FrameId, page_size: usize) -> Self {
        Self {
            _frame_id: frame_id,
            page_idx: AtomicU64::new(Self::INVALID_PAGE),
            pin_count: AtomicUsize::new(0),
            is_dirty: AtomicBool::new(false),
            page_lsn: AtomicU32::new(0),
            data: RwLock::new(vec![0u8; page_size]),
        }
    }

    fn get_page_idx(&self) -> Option<u64> {
        let idx = self.page_idx.load(Ordering::Acquire);
        if idx == Self::INVALID_PAGE {
            None
        } else {
            Some(idx)
        }
    }

    fn set_page_idx(&self, idx: Option<u64>) {
        let val = idx.unwrap_or(Self::INVALID_PAGE);
        self.page_idx.store(val, Ordering::Release);
    }
}

// ── Metrics / Statistics ──────────────────────────────────────────────────────

/// Buffer pool runtime statistics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BufferPoolStats {
    pub pool_size: usize,
    pub page_size: u32,
    pub allocated_memory_bytes: usize,
    pub active_pages: usize,
    pub pinned_pages: usize,
    pub dirty_pages: usize,
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub dirty_evictions: u64,
    pub wal_syncs: u64,
}

impl BufferPoolStats {
    /// Cache hit ratio between 0.0 and 1.0.
    pub fn hit_ratio(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            0.0
        } else {
            self.hits as f64 / total as f64
        }
    }
}

// ── Inner Manager State ───────────────────────────────────────────────────────

struct BufferPoolInner {
    pager: Pager,
    page_size: u32,
    pool_size: usize,
    frames: Vec<Arc<Frame>>,
    page_table: HashMap<u64, FrameId>,
    free_list: Vec<FrameId>,
    replacer: Box<dyn Replacer>,
    flushed_wal_seq: u32,
    wal_sync_hook: Option<WalSyncHook>,
    hits: u64,
    misses: u64,
    evictions: u64,
    dirty_evictions: u64,
    wal_syncs: u64,
}

impl BufferPoolInner {
    fn find_available_frame(&mut self) -> Result<FrameId, BufferPoolError> {
        if let Some(frame_id) = self.free_list.pop() {
            return Ok(frame_id);
        }

        let victim_id = self.replacer.victim().ok_or(BufferPoolError::NoFreeFrame)?;

        let frame = self.frames[victim_id].clone();
        if let Some(old_page_idx) = frame.get_page_idx() {
            let is_dirty = frame.is_dirty.load(Ordering::Acquire);
            if is_dirty {
                let page_lsn = frame.page_lsn.load(Ordering::Acquire);
                if let Err(err) = self.enforce_wal_invariant(old_page_idx, page_lsn) {
                    self.replacer.unpin(victim_id);
                    return Err(err);
                }

                let data = frame.data.read().unwrap();
                if let Err(err) = self.pager.write_page(old_page_idx, &data) {
                    self.replacer.unpin(victim_id);
                    return Err(BufferPoolError::Pager(err));
                }
                frame.is_dirty.store(false, Ordering::Release);
                self.dirty_evictions = self.dirty_evictions.saturating_add(1);
            }

            self.page_table.remove(&old_page_idx);
            frame.set_page_idx(None);
            self.evictions = self.evictions.saturating_add(1);
        }

        self.replacer.remove(victim_id);
        Ok(victim_id)
    }

    fn enforce_wal_invariant(
        &mut self,
        page_idx: u64,
        page_lsn: u32,
    ) -> Result<(), BufferPoolError> {
        if page_lsn > self.flushed_wal_seq {
            if let Some(ref hook) = self.wal_sync_hook {
                hook(page_lsn)?;
                self.flushed_wal_seq = self.flushed_wal_seq.max(page_lsn);
                self.wal_syncs = self.wal_syncs.saturating_add(1);
            } else {
                return Err(BufferPoolError::WalNotSynced {
                    page_idx,
                    page_lsn,
                    flushed_lsn: self.flushed_wal_seq,
                });
            }
        }
        Ok(())
    }

    fn flush_frame_internal(&mut self, frame_id: FrameId) -> Result<(), BufferPoolError> {
        let frame = self.frames[frame_id].clone();
        if let Some(page_idx) = frame.get_page_idx()
            && frame.is_dirty.load(Ordering::Acquire)
        {
            let page_lsn = frame.page_lsn.load(Ordering::Acquire);
            self.enforce_wal_invariant(page_idx, page_lsn)?;

            let data = frame.data.read().unwrap();
            self.pager
                .write_page(page_idx, &data)
                .map_err(BufferPoolError::Pager)?;
            frame.is_dirty.store(false, Ordering::Release);
        }
        Ok(())
    }
}

// ── BufferPoolManager ─────────────────────────────────────────────────────────

/// Thread-safe fixed-frame buffer pool manager.
#[derive(Clone)]
pub struct BufferPoolManager {
    inner: Arc<Mutex<BufferPoolInner>>,
}

impl BufferPoolManager {
    /// Create a new buffer pool manager wrapping a [`Pager`] with default 64 MiB budget.
    pub fn new(pager: Pager) -> Self {
        let page_size = pager.page_size();
        let options = BufferPoolOptions::from_budget(DEFAULT_MEMORY_BUDGET, page_size);
        Self::with_options(pager, options)
    }

    /// Create a buffer pool manager with a custom memory budget in bytes.
    pub fn with_budget(pager: Pager, budget_bytes: usize) -> Self {
        let page_size = pager.page_size();
        let options = BufferPoolOptions::from_budget(budget_bytes, page_size);
        Self::with_options(pager, options)
    }

    /// Create a buffer pool manager with explicit options.
    pub fn with_options(pager: Pager, options: BufferPoolOptions) -> Self {
        let page_size = pager.page_size();
        let pool_size = options.pool_size.max(1);
        let ps = page_size as usize;

        let mut frames = Vec::with_capacity(pool_size);
        let mut free_list = Vec::with_capacity(pool_size);
        for id in 0..pool_size {
            frames.push(Arc::new(Frame::new(id, ps)));
            free_list.push(id);
        }
        free_list.reverse();

        let replacer: Box<dyn Replacer> = match options.eviction_policy {
            EvictionPolicy::Clock => Box::new(ClockReplacer::new(pool_size)),
            EvictionPolicy::LruK(k) => Box::new(LruKReplacer::new(pool_size, k)),
        };

        let last_wal_seq = pager.last_wal_seq();
        let inner = BufferPoolInner {
            pager,
            page_size,
            pool_size,
            frames,
            page_table: HashMap::with_capacity(pool_size),
            free_list,
            replacer,
            flushed_wal_seq: last_wal_seq,
            wal_sync_hook: options.wal_sync_hook,
            hits: 0,
            misses: 0,
            evictions: 0,
            dirty_evictions: 0,
            wal_syncs: 0,
        };

        Self {
            inner: Arc::new(Mutex::new(inner)),
        }
    }

    /// Open or create a database file with a fixed buffer pool.
    pub fn open_or_create(
        path: impl AsRef<Path>,
        page_size: u32,
        options: Option<BufferPoolOptions>,
    ) -> Result<Self, BufferPoolError> {
        let path = path.as_ref();
        let pager = if path.exists() {
            Pager::open(path).map_err(BufferPoolError::Pager)?
        } else {
            Pager::create(path, page_size).map_err(BufferPoolError::Pager)?
        };

        let opts = options.unwrap_or_else(|| {
            BufferPoolOptions::from_budget(DEFAULT_MEMORY_BUDGET, pager.page_size())
        });
        Ok(Self::with_options(pager, opts))
    }

    /// Read borrow of a page by index.
    ///
    /// Increments pin count. Dropping the returned guard decrements pin count.
    pub fn fetch_page(&self, page_idx: u64) -> Result<PageRefGuard, BufferPoolError> {
        let (frame, frame_id) = self.pin_page_internal(page_idx)?;
        // Safety: the Arc<Frame> is held inside PageRefGuard ensuring the RwLock lives for the guard's lifetime.
        let guard = unsafe {
            let data_ptr: *const RwLock<Vec<u8>> = &frame.data;
            (*data_ptr).read().unwrap()
        };
        Ok(PageRefGuard {
            manager: self.clone(),
            _frame: frame,
            frame_id,
            page_idx,
            guard,
        })
    }

    /// Exclusive mutable borrow of a page by index.
    ///
    /// Increments pin count. Modifications mark the page dirty upon guard drop.
    pub fn fetch_page_mut(&self, page_idx: u64) -> Result<PageMutGuard, BufferPoolError> {
        let (frame, frame_id) = self.pin_page_internal(page_idx)?;
        let current_lsn = frame.page_lsn.load(Ordering::Acquire);
        // Safety: the Arc<Frame> is held inside PageMutGuard ensuring the RwLock lives for the guard's lifetime.
        let guard = unsafe {
            let data_ptr: *const RwLock<Vec<u8>> = &frame.data;
            (*data_ptr).write().unwrap()
        };
        Ok(PageMutGuard {
            manager: self.clone(),
            _frame: frame,
            frame_id,
            page_idx,
            is_dirty: false,
            page_lsn: current_lsn,
            guard,
        })
    }

    /// Allocate a new page on disk and pin it in the buffer pool.
    pub fn new_page(&self) -> Result<(u64, PageMutGuard), BufferPoolError> {
        let (page_idx, frame, frame_id) = {
            let mut inner = self.inner.lock().unwrap();

            // 1. First find an available frame in the buffer pool.
            let frame_id = inner.find_available_frame()?;

            // 2. Allocate new page on disk.
            let page_idx = match inner.pager.alloc() {
                Ok(idx) => idx,
                Err(err) => {
                    inner.free_list.push(frame_id);
                    return Err(BufferPoolError::Pager(err));
                }
            };

            let frame = inner.frames[frame_id].clone();

            {
                let mut data = frame.data.write().unwrap();
                data.fill(0);
            }

            frame.set_page_idx(Some(page_idx));
            frame.pin_count.store(1, Ordering::Release);
            frame.is_dirty.store(true, Ordering::Release);
            frame.page_lsn.store(0, Ordering::Release);

            inner.page_table.insert(page_idx, frame_id);
            inner.replacer.record_access(frame_id);
            inner.replacer.pin(frame_id);

            (page_idx, frame, frame_id)
        };

        let guard = unsafe {
            let data_ptr: *const RwLock<Vec<u8>> = &frame.data;
            (*data_ptr).write().unwrap()
        };

        let page_guard = PageMutGuard {
            manager: self.clone(),
            _frame: frame,
            frame_id,
            page_idx,
            is_dirty: true,
            page_lsn: 0,
            guard,
        };

        Ok((page_idx, page_guard))
    }

    /// Decrement pin count for a page manually.
    pub fn unpin_page(
        &self,
        page_idx: u64,
        is_dirty: bool,
        page_lsn: u32,
    ) -> Result<(), BufferPoolError> {
        let mut inner = self.inner.lock().unwrap();
        let frame_id = match inner.page_table.get(&page_idx).copied() {
            Some(id) => id,
            None => return Err(BufferPoolError::PageNotFound(page_idx)),
        };

        let frame = inner.frames[frame_id].clone();
        if is_dirty {
            frame.is_dirty.store(true, Ordering::Release);
            let _ = frame.page_lsn.fetch_max(page_lsn, Ordering::AcqRel);
        }

        let old_pins = frame.pin_count.fetch_sub(1, Ordering::AcqRel);
        if old_pins <= 1 {
            frame.pin_count.store(0, Ordering::Release);
            inner.replacer.unpin(frame_id);
        }
        Ok(())
    }

    /// Flush a specific dirty page to disk.
    pub fn flush_page(&self, page_idx: u64) -> Result<(), BufferPoolError> {
        let mut inner = self.inner.lock().unwrap();
        let frame_id = match inner.page_table.get(&page_idx).copied() {
            Some(id) => id,
            None => return Ok(()),
        };
        inner.flush_frame_internal(frame_id)
    }

    /// Flush all dirty pages currently in the buffer pool to disk.
    pub fn flush_all(&self) -> Result<(), BufferPoolError> {
        let mut inner = self.inner.lock().unwrap();
        let frame_ids: Vec<FrameId> = inner.page_table.values().copied().collect();
        for frame_id in frame_ids {
            inner.flush_frame_internal(frame_id)?;
        }
        inner.pager.sync_header().map_err(BufferPoolError::Pager)?;
        Ok(())
    }

    /// Delete a page from the buffer pool and free it in the pager.
    pub fn delete_page(&self, page_idx: u64) -> Result<(), BufferPoolError> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(&frame_id) = inner.page_table.get(&page_idx) {
            let frame = inner.frames[frame_id].clone();
            let pins = frame.pin_count.load(Ordering::Acquire);
            if pins > 0 {
                return Err(BufferPoolError::PagePinned(page_idx));
            }

            inner.page_table.remove(&page_idx);
            frame.set_page_idx(None);
            frame.is_dirty.store(false, Ordering::Release);
            frame.page_lsn.store(0, Ordering::Release);
            inner.replacer.remove(frame_id);
            inner.free_list.push(frame_id);
        }

        inner.pager.free(page_idx).map_err(BufferPoolError::Pager)?;
        Ok(())
    }

    /// Update the confirmed flushed WAL sequence number.
    pub fn record_wal_sync(&self, flushed_lsn: u32) {
        let mut inner = self.inner.lock().unwrap();
        inner.flushed_wal_seq = inner.flushed_wal_seq.max(flushed_lsn);
    }

    /// Query current buffer pool runtime statistics.
    pub fn stats(&self) -> BufferPoolStats {
        let inner = self.inner.lock().unwrap();
        let mut pinned_pages = 0;
        let mut dirty_pages = 0;

        for frame in &inner.frames {
            if frame.get_page_idx().is_some() {
                if frame.pin_count.load(Ordering::Acquire) > 0 {
                    pinned_pages += 1;
                }
                if frame.is_dirty.load(Ordering::Acquire) {
                    dirty_pages += 1;
                }
            }
        }

        BufferPoolStats {
            pool_size: inner.pool_size,
            page_size: inner.page_size,
            allocated_memory_bytes: inner.pool_size * (inner.page_size as usize),
            active_pages: inner.page_table.len(),
            pinned_pages,
            dirty_pages,
            hits: inner.hits,
            misses: inner.misses,
            evictions: inner.evictions,
            dirty_evictions: inner.dirty_evictions,
            wal_syncs: inner.wal_syncs,
        }
    }

    /// Total number of pages managed by the underlying pager.
    pub fn page_count(&self) -> u64 {
        let inner = self.inner.lock().unwrap();
        inner.pager.page_count()
    }

    /// Page size in bytes.
    pub fn page_size(&self) -> u32 {
        let inner = self.inner.lock().unwrap();
        inner.page_size
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    fn pin_page_internal(&self, page_idx: u64) -> Result<(Arc<Frame>, FrameId), BufferPoolError> {
        let mut inner = self.inner.lock().unwrap();
        if page_idx >= inner.pager.page_count() {
            return Err(BufferPoolError::Pager(PagerError::PageOutOfRange(page_idx)));
        }

        if let Some(&frame_id) = inner.page_table.get(&page_idx) {
            let frame = inner.frames[frame_id].clone();
            frame.pin_count.fetch_add(1, Ordering::AcqRel);
            inner.replacer.record_access(frame_id);
            inner.replacer.pin(frame_id);
            inner.hits = inner.hits.saturating_add(1);
            return Ok((frame, frame_id));
        }

        // Cache miss: evict or allocate a frame.
        inner.misses = inner.misses.saturating_add(1);
        let frame_id = inner.find_available_frame()?;
        let frame = inner.frames[frame_id].clone();

        // Read page data from pager disk storage.
        let page_ref = inner.pager.read(page_idx).map_err(BufferPoolError::Pager)?;
        {
            let mut data = frame.data.write().unwrap();
            data.copy_from_slice(page_ref.as_bytes());
        }

        frame.set_page_idx(Some(page_idx));
        frame.pin_count.store(1, Ordering::Release);
        frame.is_dirty.store(false, Ordering::Release);
        frame.page_lsn.store(0, Ordering::Release);

        inner.page_table.insert(page_idx, frame_id);
        inner.replacer.record_access(frame_id);
        inner.replacer.pin(frame_id);

        Ok((frame, frame_id))
    }
}

// ── RAII Guards ───────────────────────────────────────────────────────────────

/// RAII shared read guard for a page in the buffer pool.
pub struct PageRefGuard {
    manager: BufferPoolManager,
    _frame: Arc<Frame>,
    frame_id: FrameId,
    page_idx: u64,
    guard: RwLockReadGuard<'static, Vec<u8>>,
}

impl PageRefGuard {
    pub fn page_idx(&self) -> u64 {
        self.page_idx
    }

    pub fn frame_id(&self) -> FrameId {
        self.frame_id
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.guard.as_slice()
    }
}

impl Deref for PageRefGuard {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.guard.as_slice()
    }
}

impl Drop for PageRefGuard {
    fn drop(&mut self) {
        let _ = self.manager.unpin_page(self.page_idx, false, 0);
    }
}

/// RAII exclusive mutable guard for a page in the buffer pool.
pub struct PageMutGuard {
    manager: BufferPoolManager,
    _frame: Arc<Frame>,
    frame_id: FrameId,
    page_idx: u64,
    is_dirty: bool,
    page_lsn: u32,
    guard: RwLockWriteGuard<'static, Vec<u8>>,
}

impl PageMutGuard {
    pub fn page_idx(&self) -> u64 {
        self.page_idx
    }

    pub fn frame_id(&self) -> FrameId {
        self.frame_id
    }

    /// Mark page as dirty with associated WAL sequence number.
    pub fn mark_dirty(&mut self, lsn: u32) {
        self.is_dirty = true;
        self.page_lsn = self.page_lsn.max(lsn);
    }

    /// Current LSN associated with this page guard.
    pub fn lsn(&self) -> u32 {
        self.page_lsn
    }
}

impl Deref for PageMutGuard {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.guard.as_slice()
    }
}

impl DerefMut for PageMutGuard {
    fn deref_mut(&mut self) -> &mut [u8] {
        self.is_dirty = true;
        self.guard.as_mut_slice()
    }
}

impl Drop for PageMutGuard {
    fn drop(&mut self) {
        let _ = self
            .manager
            .unpin_page(self.page_idx, self.is_dirty, self.page_lsn);
    }
}

// ── Error Handling ────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum BufferPoolError {
    Pager(PagerError),
    NoFreeFrame,
    PagePinned(u64),
    PageNotFound(u64),
    WalNotSynced {
        page_idx: u64,
        page_lsn: u32,
        flushed_lsn: u32,
    },
    WalSyncFailed(String),
}

impl fmt::Display for BufferPoolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pager(e) => write!(f, "buffer pool pager error: {e}"),
            Self::NoFreeFrame => f.write_str("all buffer pool frames are currently pinned"),
            Self::PagePinned(idx) => write!(f, "cannot delete or free pinned page: {idx}"),
            Self::PageNotFound(idx) => write!(f, "page not found in buffer pool: {idx}"),
            Self::WalNotSynced {
                page_idx,
                page_lsn,
                flushed_lsn,
            } => write!(
                f,
                "WAL sync invariant violation on page {page_idx}: page LSN {page_lsn} > flushed WAL LSN {flushed_lsn}"
            ),
            Self::WalSyncFailed(err) => write!(f, "WAL synchronization hook failed: {err}"),
        }
    }
}

impl std::error::Error for BufferPoolError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Pager(e) => Some(e),
            _ => None,
        }
    }
}

impl From<PagerError> for BufferPoolError {
    fn from(err: PagerError) -> Self {
        Self::Pager(err)
    }
}
