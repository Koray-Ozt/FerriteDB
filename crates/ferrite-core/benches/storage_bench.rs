use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use ferrite_core::pager::{PAGE_4K, PAGE_8K, Pager};
use ferrite_core::slotted_page::{SlottedPage, get_record, put_record};
use std::fs;
use std::path::PathBuf;

fn temp_pager_path(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    path.push(format!("ferrite-bench-pager-{name}-{nonce}.fdb"));
    path
}

fn bench_pager_operations(c: &mut Criterion) {
    let mut group = c.benchmark_group("pager_storage");

    for (name, page_size) in [("page_4k", PAGE_4K), ("page_8k", PAGE_8K)] {
        let path = temp_pager_path(name);
        let mut pager = Pager::create(&path, page_size).unwrap();
        let payload = vec![0xABu8; page_size as usize];

        group.throughput(Throughput::Bytes(page_size as u64 * 100));
        group.bench_function(BenchmarkId::new("alloc_and_write_100_pages", name), |b| {
            b.iter(|| {
                for _ in 0..100 {
                    let idx = pager.alloc().unwrap();
                    pager.write_page(idx, &payload).unwrap();
                }
            });
        });

        drop(pager);
        let _ = fs::remove_file(&path);
    }

    group.finish();
}

fn bench_slotted_page_operations(c: &mut Criterion) {
    let mut group = c.benchmark_group("slotted_page");

    group.throughput(Throughput::Elements(20));
    group.bench_function("insert_and_read_20_records_per_page", |b| {
        let record = vec![0x55u8; 120];
        b.iter(|| {
            let mut page = SlottedPage::new(PAGE_4K);
            let mut slot_ids = Vec::with_capacity(20);
            for _ in 0..20 {
                let slot = page.insert(&record).unwrap();
                slot_ids.push(slot);
            }
            for &slot in &slot_ids {
                let _val = page.get(slot).unwrap();
            }
        });
    });

    // Overflow records spanning slotted pages with pager
    {
        let path = temp_pager_path("overflow");
        let mut pager = Pager::create(&path, PAGE_4K).unwrap();
        let large_payload = vec![0xEFu8; 16 * 1024]; // 16 KiB record

        group.throughput(Throughput::Bytes(large_payload.len() as u64));
        group.bench_function("put_and_get_16kb_overflow_record", |b| {
            b.iter(|| {
                let rec_id = put_record(&mut pager, &large_payload).unwrap();
                let retrieved = get_record(&mut pager, rec_id).unwrap();
                assert_eq!(retrieved.len(), large_payload.len());
            });
        });

        drop(pager);
        let _ = fs::remove_file(&path);
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_pager_operations,
    bench_slotted_page_operations
);
criterion_main!(benches);
