use ferrite_core::pager::{PAGE_4K, PAGE_8K, Pager};
use ferrite_core::slotted_page::{
    MAX_RECORD_BYTES, SlottedError, SlottedPage, delete_record, get_record, put_record,
};

fn temp_path(name: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("ferrite-slotted-{name}-{}", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

fn payload(byte: u8, len: usize) -> Vec<u8> {
    vec![byte; len]
}

#[test]
fn insert_and_get_100_byte_payload() {
    let mut page = SlottedPage::new(PAGE_4K);
    let data = payload(0xA5, 100);
    let slot = page.insert(&data).unwrap();
    assert_eq!(page.get(slot).unwrap(), data.as_slice());
}

#[test]
fn slot_directory_grows_forward_payloads_grow_backward() {
    let mut page = SlottedPage::new(PAGE_4K);
    let first = payload(0x11, 16);
    let second = payload(0x22, 24);
    let a = page.insert(&first).unwrap();
    let b = page.insert(&second).unwrap();

    let bytes = page.as_bytes();
    let slot_count = u16::from_le_bytes(bytes[0..2].try_into().unwrap());
    let free_end = u16::from_le_bytes(bytes[2..4].try_into().unwrap()) as usize;
    assert_eq!(slot_count, 2);
    assert_eq!(a, 0);
    assert_eq!(b, 1);

    let slot_a_off = u16::from_le_bytes(bytes[8..10].try_into().unwrap()) as usize;
    let slot_b_off = u16::from_le_bytes(bytes[14..16].try_into().unwrap()) as usize;
    assert!(slot_a_off > 8 + 2 * 6, "payloads sit after the directory");
    assert!(
        slot_b_off < slot_a_off,
        "later payloads pack toward the start"
    );
    assert_eq!(slot_b_off, free_end);
    assert_eq!(&bytes[slot_a_off..slot_a_off + 16], first.as_slice());
    assert_eq!(&bytes[slot_b_off..slot_b_off + 24], second.as_slice());
    assert_eq!(slot_a_off + 16, PAGE_4K as usize);
}

#[test]
fn delete_tombstones_slot_and_reuses_id() {
    let mut page = SlottedPage::new(PAGE_4K);
    let first = payload(1, 32);
    let second = payload(2, 32);
    let a = page.insert(&first).unwrap();
    let b = page.insert(&second).unwrap();
    page.delete(a).unwrap();
    assert!(matches!(page.get(a), Err(SlottedError::Deleted)));
    assert_eq!(page.get(b).unwrap(), second.as_slice());

    let reused = page.insert(&payload(3, 32)).unwrap();
    assert_eq!(reused, a, "tombstone slot ids are reused");
    assert_eq!(page.slot_count(), 2);
    assert_eq!(page.get(reused).unwrap(), payload(3, 32));
}

#[test]
fn compact_on_write_consolidates_fragmented_free_space() {
    let mut page = SlottedPage::new(PAGE_4K);
    let chunk = 900usize;
    let mut live = Vec::new();
    for i in 0..4 {
        live.push(page.insert(&payload(i as u8 + 1, chunk)).unwrap());
    }
    page.delete(live[1]).unwrap();
    page.delete(live[2]).unwrap();

    let before = page.contiguous_free();
    assert!(
        before < chunk,
        "deletes must leave a hole rather than a contiguous gap: {before}"
    );
    assert!(page.reclaimable_free() >= chunk);

    let compacted = page.insert(&payload(0xEE, chunk)).unwrap();
    assert!(
        page.contiguous_free() > before,
        "insert must compact holes so later writes see consolidated free space"
    );
    assert!(compacted == live[1] || compacted == live[2]);
    assert_eq!(page.get(live[0]).unwrap(), payload(1, chunk));
    assert_eq!(page.get(live[3]).unwrap(), payload(4, chunk));
    assert_eq!(page.get(compacted).unwrap(), payload(0xEE, chunk));
    let leftover = if compacted == live[1] {
        live[2]
    } else {
        live[1]
    };
    assert!(matches!(page.get(leftover), Err(SlottedError::Deleted)));
}

#[test]
fn stores_100b_and_64kib_payloads_across_slotted_pages() {
    let path = temp_path("span");
    let mut pager = Pager::create(&path, PAGE_4K).unwrap();

    let small = payload(0x10, 100);
    let large = payload(0x64, MAX_RECORD_BYTES);
    let small_id = put_record(&mut pager, &small).unwrap();
    let large_id = put_record(&mut pager, &large).unwrap();

    assert_eq!(get_record(&mut pager, small_id).unwrap(), small);
    assert_eq!(get_record(&mut pager, large_id).unwrap(), large);
    assert!(
        pager.page_count() > 3,
        "64 KiB must spill across overflow pages"
    );

    delete_record(&mut pager, large_id).unwrap();
    assert!(matches!(
        get_record(&mut pager, large_id),
        Err(SlottedError::Deleted)
    ));
    assert_eq!(get_record(&mut pager, small_id).unwrap(), small);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn near_page_size_payloads_spill_to_overflow_instead_of_nospace() {
    let path = temp_path("near-page");
    let mut pager = Pager::create(&path, PAGE_4K).unwrap();
    // A fresh 4 KiB page has 4088 contiguous free bytes. An inline record
    // also needs a 1-byte kind tag and a 6-byte slot, so 4082..=4087 must
    // overflow rather than fail with NoSpace.
    for len in [4082usize, 4087] {
        let data = payload(0x3C, len);
        let id = put_record(&mut pager, &data).expect("near-page payload must be stored");
        assert_eq!(get_record(&mut pager, id).unwrap(), data);
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn oversized_payload_is_rejected() {
    let path = temp_path("too-big");
    let mut pager = Pager::create(&path, PAGE_8K).unwrap();
    let err = put_record(&mut pager, &payload(1, MAX_RECORD_BYTES + 1)).unwrap_err();
    assert!(matches!(err, SlottedError::PayloadTooLarge));
    std::fs::remove_file(path).unwrap();
}

#[test]
fn parse_rejects_directory_that_overlaps_payloads() {
    let mut bytes = SlottedPage::new(PAGE_4K).as_bytes().to_vec();
    bytes[0..2].copy_from_slice(&200u16.to_le_bytes());
    bytes[2..4].copy_from_slice(&16u16.to_le_bytes());
    let err = SlottedPage::parse(&bytes).unwrap_err();
    assert!(matches!(err, SlottedError::Corrupt(_)));
}
