//! Archiving, verifying and extracting a file far larger than the memory the tool is allowed to
//! use. Ignored by default because it writes several gigabytes:
//!
//!     ARX_LARGE_GIB=8 ARX_LARGE_DIR=D:\scratch cargo test --release --test large -- --ignored --nocapture
//!
//! The file alternates 1 MiB blocks of text (compressible) and random bytes (not), so both the
//! compressed and the stored path are exercised.

mod common;

use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::time::Instant;

use arx::format::Codec;
use arx::reader::{scan, Existing, FsSink, NullSink};
use arx::writer::ArchiveWriter;
use common::*;

mod heap {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

    pub struct Counting;
    static CURRENT: AtomicUsize = AtomicUsize::new(0);
    static PEAK: AtomicUsize = AtomicUsize::new(0);

    // SAFETY: forwards every call to the system allocator and only adds bookkeeping.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, l: Layout) -> *mut u8 {
            let p = System.alloc(l);
            if !p.is_null() {
                let now = CURRENT.fetch_add(l.size(), Relaxed) + l.size();
                PEAK.fetch_max(now, Relaxed);
            }
            p
        }
        unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
            CURRENT.fetch_sub(l.size(), Relaxed);
            System.dealloc(p, l)
        }
        unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
            let q = System.realloc(p, l, new);
            if !q.is_null() {
                if new >= l.size() {
                    let now = CURRENT.fetch_add(new - l.size(), Relaxed) + (new - l.size());
                    PEAK.fetch_max(now, Relaxed);
                } else {
                    CURRENT.fetch_sub(l.size() - new, Relaxed);
                }
            }
            q
        }
    }

    pub fn reset_peak() {
        PEAK.store(CURRENT.load(Relaxed), Relaxed);
    }

    pub fn peak_mib() -> usize {
        PEAK.load(Relaxed).div_ceil(1 << 20)
    }
}

#[global_allocator]
static ALLOC: heap::Counting = heap::Counting;

fn file_digest(p: &std::path::Path) -> blake3::Hash {
    let mut r = BufReader::with_capacity(1 << 20, File::open(p).unwrap());
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = r.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    h.finalize()
}

#[test]
#[ignore]
fn a_file_far_larger_than_memory_is_archived_verified_and_extracted_in_bounded_memory() {
    let gib: u64 = std::env::var("ARX_LARGE_GIB").ok().and_then(|v| v.parse().ok()).unwrap_or(2);
    let dir = match std::env::var("ARX_LARGE_DIR") {
        Ok(d) => tempfile::tempdir_in(d).unwrap(),
        Err(_) => tempfile::tempdir().unwrap(),
    };
    let src = dir.path().join("big.dat");
    let blocks = gib * 1024;

    let t = Instant::now();
    {
        let mut w = BufWriter::with_capacity(1 << 22, File::create(&src).unwrap());
        let mut rng = Rng(0x5EED);
        for i in 0..blocks {
            if i % 2 == 0 {
                w.write_all(&text(1 << 20, i)).unwrap();
            } else {
                w.write_all(&rng.bytes(1 << 20)).unwrap();
            }
        }
    }
    println!("generated a {gib} GiB file in {:.1} s", t.elapsed().as_secs_f64());
    let size = fs::metadata(&src).unwrap().len();
    let want = file_digest(&src);

    for codec in [Codec::Lz4, Codec::Deflate] {
        let arc = dir.path().join(format!("big-{}.arx", codec.name()));
        heap::reset_peak();
        let t = Instant::now();
        let out = BufWriter::with_capacity(1 << 22, File::create(&arc).unwrap());
        let mut w = ArchiveWriter::new(out, config(codec, 1 << 20, 0)).unwrap();
        w.add_path(&src, "big.dat").unwrap();
        let (mut out, stats) = w.finish().unwrap();
        out.flush().unwrap();
        let secs = t.elapsed().as_secs_f64();
        println!("{:<8} create:  {:>7.2} s  {:>6.0} MB/s  {:.1}% of input   peak heap {} MiB", codec.name(), secs, size as f64 / secs / 1e6, stats.archive_bytes as f64 * 100.0 / size as f64, heap::peak_mib());
        let create_heap = heap::peak_mib();

        heap::reset_peak();
        let t = Instant::now();
        let report = scan(File::open(&arc).unwrap(), 0, &mut NullSink, false).unwrap();
        let secs = t.elapsed().as_secs_f64();
        assert!(report.ok(), "{:?}", report.problems);
        println!("{:<8} verify:  {:>7.2} s  {:>6.0} MB/s                    peak heap {} MiB", codec.name(), secs, size as f64 / secs / 1e6, heap::peak_mib());
        let verify_heap = heap::peak_mib();

        heap::reset_peak();
        let out_dir = dir.path().join(format!("out-{}", codec.name()));
        fs::create_dir(&out_dir).unwrap();
        let t = Instant::now();
        let mut sink = FsSink::new(&out_dir, Existing::Fail, &[]);
        let report = scan(File::open(&arc).unwrap(), 0, &mut sink, false).unwrap();
        let secs = t.elapsed().as_secs_f64();
        assert!(report.ok(), "{:?}", report.problems);
        println!("{:<8} extract: {:>7.2} s  {:>6.0} MB/s                    peak heap {} MiB", codec.name(), secs, size as f64 / secs / 1e6, heap::peak_mib());
        let extract_heap = heap::peak_mib();
        assert_eq!(file_digest(&out_dir.join("big.dat")), want, "the extracted file differs from the original");
        println!("{:<8} the extracted file's BLAKE3 matches the original", codec.name());
        assert!(create_heap < 400 && verify_heap < 400 && extract_heap < 400, "memory use must not grow with the file: {create_heap}/{verify_heap}/{extract_heap} MiB");
        fs::remove_dir_all(&out_dir).unwrap();
        fs::remove_file(&arc).unwrap();
    }
}
