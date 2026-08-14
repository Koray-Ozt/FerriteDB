//! Slotted-page frame over a fixed-size pager page.
//!
//! ```text
//! [ header 8 B ][ slot directory → ][ free space ][ ← payloads ]
//! ```
//!
//! The slot array grows forward from the header. Variable-length payloads are
//! packed backward from the end of the page. A slot is an indirection
//! `SlotId -> (offset, length, flags)`. Deletes leave tombstones so IDs stay
//! stable; inserts compact the payload region when fragmentation would reject
//! a write that otherwise fits.

use crate::pager::{PAGE_4K, PAGE_8K, Pager, PagerError};
use std::fmt;

/// Maximum payload accepted by [`put_record`].
pub const MAX_RECORD_BYTES: usize = 64 * 1024;

const HEADER_LEN: usize = 8;
const SLOT_LEN: usize = 6;
const FLAG_LIVE: u16 = 0;
const FLAG_TOMBSTONE: u16 = 1;
const FLAG_OVERFLOW: u16 = 2;
const KIND_INLINE: u8 = 0;
const KIND_OVERFLOW: u8 = 1;

/// Stable index into a page's slot directory.
pub type SlotId = u16;

/// Home-page location of a record that may span overflow pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordId {
    pub page: u64,
    pub slot: SlotId,
}

/// In-memory slotted frame for one page.
#[derive(Clone, Debug)]
pub struct SlottedPage {
    data: Vec<u8>,
}

impl SlottedPage {
    /// Empty slotted page of `page_size` bytes (`4096` or `8192`).
    pub fn new(page_size: u32) -> Self {
        assert!(
            page_size == PAGE_4K || page_size == PAGE_8K,
            "slotted pages must be 4096 or 8192 bytes"
        );
        let ps = page_size as usize;
        let mut data = vec![0u8; ps];
        write_u16(&mut data, 0, 0);
        write_u16(&mut data, 2, page_size as u16);
        Self { data }
    }

    /// Parse and validate an existing page image.
    pub fn parse(bytes: &[u8]) -> Result<Self, SlottedError> {
        if bytes.len() != PAGE_4K as usize && bytes.len() != PAGE_8K as usize {
            return Err(SlottedError::Corrupt("invalid slotted page size"));
        }
        let page = Self {
            data: bytes.to_vec(),
        };
        page.validate()?;
        Ok(page)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    pub fn page_size(&self) -> usize {
        self.data.len()
    }

    pub fn slot_count(&self) -> u16 {
        read_u16(&self.data, 0)
    }

    /// Contiguous unused bytes between the slot directory and payloads.
    pub fn contiguous_free(&self) -> usize {
        let free_end = self.free_end() as usize;
        let dir_end = HEADER_LEN + self.slot_count() as usize * SLOT_LEN;
        free_end.saturating_sub(dir_end)
    }

    /// Unused payload bytes including holes left by tombstones.
    pub fn reclaimable_free(&self) -> usize {
        let mut live = 0usize;
        for slot in 0..self.slot_count() {
            if self.flags(slot) == FLAG_LIVE || self.flags(slot) == FLAG_OVERFLOW {
                live = live.saturating_add(self.length(slot) as usize);
            }
        }
        self.page_size()
            .saturating_sub(HEADER_LEN)
            .saturating_sub(self.slot_count() as usize * SLOT_LEN)
            .saturating_sub(live)
    }

    pub fn insert(&mut self, payload: &[u8]) -> Result<SlotId, SlottedError> {
        if payload.len() > u16::MAX as usize {
            return Err(SlottedError::PayloadTooLarge);
        }
        if payload.is_empty() {
            return Err(SlottedError::Corrupt("empty payload"));
        }

        let reuse = self.find_tombstone();
        let needs_new_slot = reuse.is_none();
        let extra_dir = if needs_new_slot { SLOT_LEN } else { 0 };

        if self.contiguous_free() < payload.len() + extra_dir {
            if self.reclaimable_free() >= payload.len() + extra_dir {
                self.compact();
            }
            if self.contiguous_free() < payload.len() + extra_dir {
                return Err(SlottedError::NoSpace);
            }
        }

        let slot = match reuse {
            Some(id) => id,
            None => {
                let next = self.slot_count();
                let id = SlotId::try_from(next).map_err(|_| SlottedError::NoSpace)?;
                let count = next.checked_add(1).ok_or(SlottedError::NoSpace)?;
                write_u16(&mut self.data, 0, count);
                id
            }
        };

        let len = payload.len() as u16;
        let new_end = self
            .free_end()
            .checked_sub(len)
            .ok_or(SlottedError::Corrupt("free pointer underflow"))?;
        let start = new_end as usize;
        self.data[start..start + payload.len()].copy_from_slice(payload);
        write_u16(&mut self.data, 2, new_end);
        self.write_slot(slot, new_end, len, FLAG_LIVE);
        Ok(slot)
    }

    pub fn get(&self, slot: SlotId) -> Result<&[u8], SlottedError> {
        self.check_slot(slot)?;
        match self.flags(slot) {
            FLAG_TOMBSTONE => Err(SlottedError::Deleted),
            FLAG_LIVE | FLAG_OVERFLOW => {
                let offset = self.offset(slot) as usize;
                let len = self.length(slot) as usize;
                Ok(&self.data[offset..offset + len])
            }
            _ => Err(SlottedError::Corrupt("unknown slot flag")),
        }
    }

    pub fn delete(&mut self, slot: SlotId) -> Result<(), SlottedError> {
        self.check_slot(slot)?;
        if self.flags(slot) == FLAG_TOMBSTONE {
            return Err(SlottedError::Deleted);
        }
        self.write_slot(slot, 0, 0, FLAG_TOMBSTONE);
        Ok(())
    }

    fn set_flags(&mut self, slot: SlotId, flags: u16) -> Result<(), SlottedError> {
        self.check_slot(slot)?;
        if self.flags(slot) == FLAG_TOMBSTONE {
            return Err(SlottedError::Deleted);
        }
        self.write_slot(slot, self.offset(slot), self.length(slot), flags);
        Ok(())
    }

    /// Pack live payloads against the end of the page. Tombstone slots stay.
    pub fn compact(&mut self) {
        let count = self.slot_count();
        let mut packed = Vec::new();
        let mut cursor = self.page_size();
        for slot in 0..count {
            let flags = self.flags(slot);
            if flags == FLAG_TOMBSTONE {
                continue;
            }
            let offset = self.offset(slot) as usize;
            let len = self.length(slot) as usize;
            cursor = cursor.saturating_sub(len);
            packed.push((
                slot,
                cursor as u16,
                flags,
                self.data[offset..offset + len].to_vec(),
            ));
        }
        write_u16(&mut self.data, 2, cursor as u16);
        let dir_end = HEADER_LEN + count as usize * SLOT_LEN;
        self.data[dir_end..].fill(0);
        for (slot, offset, flags, bytes) in packed {
            let start = offset as usize;
            self.data[start..start + bytes.len()].copy_from_slice(&bytes);
            self.write_slot(slot, offset, bytes.len() as u16, flags);
        }
    }

    fn validate(&self) -> Result<(), SlottedError> {
        let count = self.slot_count() as usize;
        let free_end = self.free_end() as usize;
        let dir_end = HEADER_LEN
            .checked_add(
                count
                    .checked_mul(SLOT_LEN)
                    .ok_or(SlottedError::Corrupt("slot overflow"))?,
            )
            .ok_or(SlottedError::Corrupt("slot overflow"))?;
        if dir_end > self.page_size() || free_end > self.page_size() || dir_end > free_end {
            return Err(SlottedError::Corrupt("directory overlaps payloads"));
        }
        let mut spans = Vec::new();
        for slot in 0..count as u16 {
            match self.flags(slot) {
                FLAG_TOMBSTONE => {
                    if self.offset(slot) != 0 || self.length(slot) != 0 {
                        return Err(SlottedError::Corrupt("tombstone must be empty"));
                    }
                }
                FLAG_LIVE | FLAG_OVERFLOW => {
                    let offset = self.offset(slot) as usize;
                    let len = self.length(slot) as usize;
                    let end = offset
                        .checked_add(len)
                        .ok_or(SlottedError::Corrupt("slot range overflow"))?;
                    if offset < free_end || end > self.page_size() || len == 0 {
                        return Err(SlottedError::Corrupt("slot payload out of range"));
                    }
                    spans.push((offset, end));
                }
                _ => return Err(SlottedError::Corrupt("unknown slot flag")),
            }
        }
        spans.sort_unstable_by_key(|span| span.0);
        for pair in spans.windows(2) {
            if pair[0].1 > pair[1].0 {
                return Err(SlottedError::Corrupt("overlapping payloads"));
            }
        }
        Ok(())
    }

    fn find_tombstone(&self) -> Option<SlotId> {
        (0..self.slot_count()).find(|&slot| self.flags(slot) == FLAG_TOMBSTONE)
    }

    fn check_slot(&self, slot: SlotId) -> Result<(), SlottedError> {
        if slot >= self.slot_count() {
            Err(SlottedError::InvalidSlot)
        } else {
            Ok(())
        }
    }

    fn free_end(&self) -> u16 {
        read_u16(&self.data, 2)
    }

    fn offset(&self, slot: SlotId) -> u16 {
        read_u16(&self.data, slot_offset(slot))
    }

    fn length(&self, slot: SlotId) -> u16 {
        read_u16(&self.data, slot_offset(slot) + 2)
    }

    fn flags(&self, slot: SlotId) -> u16 {
        read_u16(&self.data, slot_offset(slot) + 4)
    }

    fn write_slot(&mut self, slot: SlotId, offset: u16, length: u16, flags: u16) {
        let base = slot_offset(slot);
        write_u16(&mut self.data, base, offset);
        write_u16(&mut self.data, base + 2, length);
        write_u16(&mut self.data, base + 4, flags);
    }
}

/// Store `payload` (up to 64 KiB) in one or more slotted pages.
pub fn put_record(pager: &mut Pager, payload: &[u8]) -> Result<RecordId, SlottedError> {
    if payload.is_empty() || payload.len() > MAX_RECORD_BYTES {
        return Err(SlottedError::PayloadTooLarge);
    }
    let page_size = pager.page_size();
    let mut home = SlottedPage::new(page_size);
    if home.contiguous_free() >= payload.len() {
        let mut inline = Vec::with_capacity(1 + payload.len());
        inline.push(KIND_INLINE);
        inline.extend_from_slice(payload);
        if home.contiguous_free() >= inline.len() {
            let slot = home.insert(&inline)?;
            let page = pager.alloc().map_err(SlottedError::Pager)?;
            pager
                .write_page(page, home.as_bytes())
                .map_err(SlottedError::Pager)?;
            return Ok(RecordId { page, slot });
        }
    }

    let chunk_cap = overflow_chunk_cap(page_size);
    if chunk_cap == 0 {
        return Err(SlottedError::NoSpace);
    }

    let mut allocated = Vec::new();
    let result = (|| {
        let mut chunks = Vec::new();
        for piece in payload.chunks(chunk_cap) {
            let mut page = SlottedPage::new(page_size);
            let slot = page.insert(piece)?;
            let idx = pager.alloc().map_err(SlottedError::Pager)?;
            allocated.push(idx);
            pager
                .write_page(idx, page.as_bytes())
                .map_err(SlottedError::Pager)?;
            chunks.push((idx, slot));
        }

        let mut descriptor = Vec::new();
        descriptor.push(KIND_OVERFLOW);
        descriptor.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        descriptor.extend_from_slice(&(chunks.len() as u16).to_le_bytes());
        for (idx, slot) in &chunks {
            descriptor.extend_from_slice(&idx.to_le_bytes());
            descriptor.extend_from_slice(&slot.to_le_bytes());
        }

        let mut home = SlottedPage::new(page_size);
        let slot = home.insert(&descriptor)?;
        home.set_flags(slot, FLAG_OVERFLOW)?;
        let page = pager.alloc().map_err(SlottedError::Pager)?;
        allocated.push(page);
        pager
            .write_page(page, home.as_bytes())
            .map_err(SlottedError::Pager)?;
        Ok(RecordId { page, slot })
    })();
    if result.is_err() {
        for idx in allocated {
            let _ = pager.free(idx);
        }
    }
    result
}

/// Read a record previously stored by [`put_record`].
pub fn get_record(pager: &mut Pager, id: RecordId) -> Result<Vec<u8>, SlottedError> {
    let page = pager.read(id.page).map_err(SlottedError::Pager)?;
    let slotted = SlottedPage::parse(page.as_bytes())?;
    let raw = slotted.get(id.slot)?;
    match raw.first().copied() {
        Some(KIND_INLINE) => Ok(raw[1..].to_vec()),
        Some(KIND_OVERFLOW) => decode_overflow(pager, raw),
        _ => Err(SlottedError::Corrupt("unknown record kind")),
    }
}

/// Delete a record. Overflow pages are freed; the home slot is tombstoned.
pub fn delete_record(pager: &mut Pager, id: RecordId) -> Result<(), SlottedError> {
    let page = pager.read(id.page).map_err(SlottedError::Pager)?;
    let mut slotted = SlottedPage::parse(page.as_bytes())?;
    let raw = slotted.get(id.slot)?.to_vec();
    if raw.first() == Some(&KIND_OVERFLOW) {
        let chunks = overflow_chunks(&raw)?;
        for (idx, _) in chunks {
            pager.free(idx).map_err(SlottedError::Pager)?;
        }
    }
    slotted.delete(id.slot)?;
    pager
        .write_page(id.page, slotted.as_bytes())
        .map_err(SlottedError::Pager)?;
    Ok(())
}

fn overflow_chunk_cap(page_size: u32) -> usize {
    let dir = HEADER_LEN + SLOT_LEN;
    (page_size as usize).saturating_sub(dir)
}

fn decode_overflow(pager: &mut Pager, raw: &[u8]) -> Result<Vec<u8>, SlottedError> {
    let chunks = overflow_chunks(raw)?;
    let declared = u32::from_le_bytes(
        raw.get(1..5)
            .ok_or(SlottedError::Corrupt("truncated overflow header"))?
            .try_into()
            .unwrap(),
    ) as usize;
    let mut out = Vec::new();
    for (idx, slot) in chunks {
        let page = pager.read(idx).map_err(SlottedError::Pager)?;
        let slotted = SlottedPage::parse(page.as_bytes())?;
        out.extend_from_slice(slotted.get(slot)?);
    }
    if out.len() != declared {
        return Err(SlottedError::Corrupt("overflow length mismatch"));
    }
    Ok(out)
}

fn overflow_chunks(raw: &[u8]) -> Result<Vec<(u64, SlotId)>, SlottedError> {
    if raw.len() < 7 {
        return Err(SlottedError::Corrupt("truncated overflow header"));
    }
    let n = u16::from_le_bytes(raw[5..7].try_into().unwrap()) as usize;
    let expected = 7usize
        .checked_add(
            n.checked_mul(10)
                .ok_or(SlottedError::Corrupt("overflow count"))?,
        )
        .ok_or(SlottedError::Corrupt("overflow count"))?;
    if raw.len() != expected {
        return Err(SlottedError::Corrupt("overflow descriptor size"));
    }
    let mut chunks = Vec::with_capacity(n);
    let mut off = 7;
    for _ in 0..n {
        let page = u64::from_le_bytes(raw[off..off + 8].try_into().unwrap());
        let slot = u16::from_le_bytes(raw[off + 8..off + 10].try_into().unwrap());
        chunks.push((page, slot));
        off += 10;
    }
    Ok(chunks)
}

fn slot_offset(slot: SlotId) -> usize {
    HEADER_LEN + slot as usize * SLOT_LEN
}

fn read_u16(data: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap())
}

fn write_u16(data: &mut [u8], offset: usize, value: u16) {
    data[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

#[derive(Debug)]
pub enum SlottedError {
    NoSpace,
    InvalidSlot,
    Deleted,
    PayloadTooLarge,
    Corrupt(&'static str),
    Pager(PagerError),
}

impl fmt::Display for SlottedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSpace => write!(f, "slotted page has no free space"),
            Self::InvalidSlot => write!(f, "slot id is out of range"),
            Self::Deleted => write!(f, "slot has been deleted"),
            Self::PayloadTooLarge => write!(f, "payload exceeds slotted-page limits"),
            Self::Corrupt(reason) => write!(f, "corrupt slotted page: {reason}"),
            Self::Pager(error) => write!(f, "pager error: {error}"),
        }
    }
}

impl std::error::Error for SlottedError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Pager(error) => Some(error),
            _ => None,
        }
    }
}
