//! Fixed-page binary storage: 4 KiB or 8 KiB pages, header metadata, free-list.
//!
//! # Disk layout
//!
//! ```text
//! offset 0          : Header Page (PAGE_SIZE bytes)
//!   [0..8]   magic      b"FERRITE\0"
//!   [8..12]  version    u32 LE  (1)
//!   [12..16] page_size  u32 LE  (4096 | 8192)
//!   [16..24] page_count u64 LE  (total pages including header)
//!   [24..32] free_head  u64 LE  (first free-list page; 0 = empty)
//!   [32..36] wal_seq    u32 LE  (last committed WAL sequence)
//!   [36..]   padding    zeros
//! offset PAGE_SIZE  : Page 1 …  (data / free-list pages)
//! ```
//!
//! A **free-list page** stores the next free page index at bytes `[0..8]` (u64 LE).
//! Page index 0 is always the header page.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

/// 4 KiB page size constant.
pub const PAGE_4K: u32 = 4096;
/// 8 KiB page size constant.
pub const PAGE_8K: u32 = 8192;

const MAGIC: &[u8; 8] = b"FERRITE\0";
const FORMAT_VERSION: u32 = 1;

// Header field offsets (byte positions within the header page).
const OFF_MAGIC: usize = 0;
const OFF_VERSION: usize = 8;
const OFF_PAGE_SIZE: usize = 12;
const OFF_PAGE_COUNT: usize = 16;
const OFF_FREE_HEAD: usize = 24;
const OFF_WAL_SEQ: usize = 32;

/// Manages a `database.fdb` file using fixed-size page units.
#[derive(Debug)]
pub struct Pager {
    file: File,
    page_size: u32,
    // Cached header fields (kept in sync with disk after every mutating op).
    page_count: u64,
    free_head: u64,
    wal_seq: u32,
}

impl Pager {
    /// Create a new pager file. Fails if the file already exists.
    ///
    /// `page_size` must be [`PAGE_4K`] or [`PAGE_8K`].
    pub fn create(path: impl AsRef<Path>, page_size: u32) -> Result<Self, PagerError> {
        if page_size != PAGE_4K && page_size != PAGE_8K {
            return Err(PagerError::InvalidPageSize(page_size));
        }
        let file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(path)?;

        // Header page: page_count starts at 1 (header occupies slot 0).
        let mut pager = Self {
            file,
            page_size,
            page_count: 1,
            free_head: 0,
            wal_seq: 0,
        };
        pager.write_header()?;
        pager.file.sync_all()?;
        Ok(pager)
    }

    /// Open an existing pager file, validate the header.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, PagerError> {
        let mut file = OpenOptions::new().read(true).write(true).open(path)?;
        // Read enough bytes for the fixed header fields (first 36 bytes + some slack).
        let mut buf = vec![0u8; PAGE_8K as usize]; // largest possible page size
        let n = {
            file.seek(SeekFrom::Start(0))?;
            file.read(&mut buf)?
        };
        if n < 36 {
            return Err(PagerError::Corrupt("header too short"));
        }
        if &buf[OFF_MAGIC..OFF_MAGIC + 8] != MAGIC {
            return Err(PagerError::Corrupt("invalid magic bytes"));
        }
        let version = u32::from_le_bytes(buf[OFF_VERSION..OFF_VERSION + 4].try_into().unwrap());
        if version != FORMAT_VERSION {
            return Err(PagerError::Corrupt("unsupported pager format version"));
        }
        let page_size =
            u32::from_le_bytes(buf[OFF_PAGE_SIZE..OFF_PAGE_SIZE + 4].try_into().unwrap());
        if page_size != PAGE_4K && page_size != PAGE_8K {
            return Err(PagerError::InvalidPageSize(page_size));
        }
        let page_count =
            u64::from_le_bytes(buf[OFF_PAGE_COUNT..OFF_PAGE_COUNT + 8].try_into().unwrap());
        let free_head =
            u64::from_le_bytes(buf[OFF_FREE_HEAD..OFF_FREE_HEAD + 8].try_into().unwrap());
        let wal_seq =
            u32::from_le_bytes(buf[OFF_WAL_SEQ..OFF_WAL_SEQ + 4].try_into().unwrap());

        Ok(Self {
            file,
            page_size,
            page_count,
            free_head,
            wal_seq,
        })
    }

    // ── Allocation ────────────────────────────────────────────────────────────

    /// Allocate a new page. Returns the page index.
    ///
    /// Reuses a page from the free-list if available; otherwise extends the file.
    pub fn alloc(&mut self) -> Result<u64, PagerError> {
        if self.free_head != 0 {
            let idx = self.free_head;
            // Read the next pointer stored in the first 8 bytes of the free page.
            let next = self.read_u64_at_page_offset(idx, 0)?;
            self.free_head = next;
            // Zero out the recycled page so callers start with a clean slate.
            self.zero_page(idx)?;
            self.write_header()?;
            self.file.sync_all()?;
            Ok(idx)
        } else {
            let idx = self.page_count;
            self.page_count = self.page_count.checked_add(1).ok_or(PagerError::Corrupt(
                "page count overflow",
            ))?;
            // Extend the file with a zero page.
            self.zero_page(idx)?;
            self.write_header()?;
            self.file.sync_all()?;
            Ok(idx)
        }
    }

    /// Return a page to the free-list.
    pub fn free(&mut self, page_idx: u64) -> Result<(), PagerError> {
        if page_idx == 0 {
            return Err(PagerError::Corrupt("cannot free the header page"));
        }
        self.check_range(page_idx)?;
        // Write current free_head as the "next" pointer in the freed page.
        self.write_u64_at_page_offset(page_idx, 0, self.free_head)?;
        self.free_head = page_idx;
        self.write_header()?;
        self.file.sync_all()?;
        Ok(())
    }

    // ── Read / Write ──────────────────────────────────────────────────────────

    /// Return a shared read borrow of a page's bytes.
    pub fn read(&mut self, page_idx: u64) -> Result<PageRef, PagerError> {
        self.check_range(page_idx)?;
        let ps = self.page_size as usize;
        let mut data = vec![0u8; ps];
        self.file
            .seek(SeekFrom::Start(self.page_offset(page_idx)))?;
        self.file.read_exact(&mut data)?;
        Ok(PageRef { data })
    }

    /// Write arbitrary bytes into a page (overwrites the full page).
    ///
    /// `data` must be exactly `page_size()` bytes.
    pub fn write_page(&mut self, page_idx: u64, data: &[u8]) -> Result<(), PagerError> {
        self.check_range(page_idx)?;
        let ps = self.page_size as usize;
        if data.len() != ps {
            return Err(PagerError::Corrupt("write data length != page_size"));
        }
        self.file
            .seek(SeekFrom::Start(self.page_offset(page_idx)))?;
        self.file.write_all(data)?;
        self.file.sync_all()?;
        Ok(())
    }

    /// Acquire an exclusive writable page buffer. Call [`PageMut::flush`] to
    /// persist changes; the buffer is automatically flushed on drop (panics on
    /// I/O error at drop-time — call `flush` explicitly in production paths).
    pub fn write(&mut self, page_idx: u64) -> Result<PageMut<'_>, PagerError> {
        self.check_range(page_idx)?;
        let ps = self.page_size as usize;
        let mut data = vec![0u8; ps];
        self.file
            .seek(SeekFrom::Start(self.page_offset(page_idx)))?;
        self.file.read_exact(&mut data)?;
        Ok(PageMut {
            pager: self,
            page_idx,
            data,
        })
    }

    // ── WAL integration ───────────────────────────────────────────────────────

    /// Record the most recently committed WAL sequence number.
    ///
    /// Writes and syncs the header page so the marker survives a crash.
    pub fn record_wal_commit(&mut self, wal_seq: u32) -> Result<(), PagerError> {
        self.wal_seq = wal_seq;
        self.sync_header()
    }

    /// Flush and sync the header page (page 0).
    pub fn sync_header(&mut self) -> Result<(), PagerError> {
        self.write_header()?;
        self.file.sync_all()?;
        Ok(())
    }

    // ── Accessors ─────────────────────────────────────────────────────────────

    pub fn page_size(&self) -> u32 {
        self.page_size
    }

    pub fn page_count(&self) -> u64 {
        self.page_count
    }

    pub fn last_wal_seq(&self) -> u32 {
        self.wal_seq
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    fn page_offset(&self, page_idx: u64) -> u64 {
        page_idx * self.page_size as u64
    }

    fn check_range(&self, page_idx: u64) -> Result<(), PagerError> {
        if page_idx >= self.page_count {
            Err(PagerError::PageOutOfRange(page_idx))
        } else {
            Ok(())
        }
    }

    fn write_header(&mut self) -> Result<(), PagerError> {
        let ps = self.page_size as usize;
        let mut header = vec![0u8; ps];
        header[OFF_MAGIC..OFF_MAGIC + 8].copy_from_slice(MAGIC);
        header[OFF_VERSION..OFF_VERSION + 4]
            .copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        header[OFF_PAGE_SIZE..OFF_PAGE_SIZE + 4]
            .copy_from_slice(&self.page_size.to_le_bytes());
        header[OFF_PAGE_COUNT..OFF_PAGE_COUNT + 8]
            .copy_from_slice(&self.page_count.to_le_bytes());
        header[OFF_FREE_HEAD..OFF_FREE_HEAD + 8]
            .copy_from_slice(&self.free_head.to_le_bytes());
        header[OFF_WAL_SEQ..OFF_WAL_SEQ + 4]
            .copy_from_slice(&self.wal_seq.to_le_bytes());
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&header)?;
        Ok(())
    }

    fn zero_page(&mut self, page_idx: u64) -> Result<(), PagerError> {
        let ps = self.page_size as usize;
        let zeros = vec![0u8; ps];
        self.file
            .seek(SeekFrom::Start(self.page_offset(page_idx)))?;
        self.file.write_all(&zeros)?;
        Ok(())
    }

    fn read_u64_at_page_offset(&mut self, page_idx: u64, offset: u64) -> Result<u64, PagerError> {
        self.file
            .seek(SeekFrom::Start(self.page_offset(page_idx) + offset))?;
        let mut buf = [0u8; 8];
        self.file.read_exact(&mut buf)?;
        Ok(u64::from_le_bytes(buf))
    }

    fn write_u64_at_page_offset(
        &mut self,
        page_idx: u64,
        offset: u64,
        value: u64,
    ) -> Result<(), PagerError> {
        self.file
            .seek(SeekFrom::Start(self.page_offset(page_idx) + offset))?;
        self.file.write_all(&value.to_le_bytes())?;
        Ok(())
    }
}

// ── PageRef ───────────────────────────────────────────────────────────────────

/// Shared read view of a page's bytes.
#[derive(Debug)]
pub struct PageRef {
    data: Vec<u8>,
}

impl PageRef {
    /// The raw bytes of the page.
    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }
}

impl std::ops::Deref for PageRef {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.data
    }
}

// ── PageMut ───────────────────────────────────────────────────────────────────

/// Exclusive write buffer for a page. Call [`PageMut::flush`] to persist.
pub struct PageMut<'a> {
    pager: &'a mut Pager,
    page_idx: u64,
    data: Vec<u8>,
}

impl<'a> PageMut<'a> {
    /// The mutable byte slice for this page.
    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }

    /// Flush the page buffer to disk and sync.
    pub fn flush(self) -> Result<(), PagerError> {
        let PageMut { pager, page_idx, data } = self;
        pager
            .file
            .seek(SeekFrom::Start(pager.page_offset(page_idx)))?;
        pager.file.write_all(&data)?;
        pager.file.sync_all()?;
        Ok(())
    }
}

impl std::ops::Deref for PageMut<'_> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.data
    }
}

impl std::ops::DerefMut for PageMut<'_> {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }
}

// ── Error ─────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum PagerError {
    Io(io::Error),
    Corrupt(&'static str),
    InvalidPageSize(u32),
    PageOutOfRange(u64),
}

impl fmt::Display for PagerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "pager I/O error: {e}"),
            Self::Corrupt(r) => write!(f, "corrupt pager: {r}"),
            Self::InvalidPageSize(s) => write!(f, "invalid page size: {s} (must be 4096 or 8192)"),
            Self::PageOutOfRange(idx) => write!(f, "page index out of range: {idx}"),
        }
    }
}

impl std::error::Error for PagerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for PagerError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}
