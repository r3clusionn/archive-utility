mod common;

use std::collections::HashMap;
use std::io::Cursor;
use std::time::SystemTime;

use arx::entry::{Entry, EntryKind};
use arx::format::{Codec, Kind, HEADER_LEN};
use arx::input::{Input, Step};
use arx::reader::{scan_with, Archive, Report, Sink};
use arx::writer::ArchiveWriter;
use common::*;
use workpool::ThreadPool;

const CHUNK: usize = 512;

/// Collects verified files in memory.
#[derive(Default)]
struct MemSink {
    files: HashMap<String, Vec<u8>>,
    current: Option<(String, Vec<u8>)>,
}

impl Sink for MemSink {
    fn begin_file(&mut self, e: &Entry) -> Result<bool, String> {
        self.current = Some((e.path.clone(), Vec::new()));
        Ok(true)
    }
    fn write(&mut self, data: &[u8]) -> Result<(), String> {
        self.current.as_mut().unwrap().1.extend_from_slice(data);
        Ok(())
    }
    fn end_file(&mut self, _: &Entry, ok: bool) -> Result<(), String> {
        let (path, data) = self.current.take().unwrap();
        if ok {
            self.files.insert(path, data);
        }
        Ok(())
    }
    fn dir(&mut self, _: &Entry) -> Result<(), String> {
        Ok(())
    }
    fn symlink(&mut self, _: &Entry) -> Result<(), String> {
        Ok(())
    }
}

fn contents() -> Vec<(&'static str, Vec<u8>)> {
    let mut r = Rng(77);
    vec![("a/multi.txt", text(CHUNK * 4 + 100, 1)), ("a/random.bin", r.bytes(CHUNK + 200)), ("empty", vec![]), ("b/small.txt", text(90, 2)), ("b/last.txt", text(CHUNK * 2, 3))]
}

fn small_archive(codec: Codec) -> Vec<u8> {
    let mut w = ArchiveWriter::new(Cursor::new(Vec::new()), config(codec, CHUNK, 1)).unwrap();
    let now = SystemTime::now();
    w.add_dir("a", now, 0o755).unwrap();
    for (p, d) in contents() {
        if p == "b/small.txt" {
            w.add_dir("b", now, 0o755).unwrap();
        }
        w.add_file(p, d.len() as u64, now, 0o644, Cursor::new(d)).unwrap();
    }
    w.finish().unwrap().0.into_inner()
}

fn read(pool: &ThreadPool, bytes: &[u8], recover: bool) -> Option<(Report, MemSink)> {
    let mut sink = MemSink::default();
    scan_with(Cursor::new(bytes.to_vec()), pool, &mut sink, recover).ok().map(|r| (r, sink))
}

/// No file accepted as good may differ from the original.
fn assert_no_wrong_data(sink: &MemSink, ctx: &str) {
    let want: HashMap<_, _> = contents().into_iter().map(|(p, d)| (p.to_string(), d)).collect();
    for (p, d) in &sink.files {
        assert_eq!(want.get(p), Some(d), "{ctx}: file {p} was accepted but is wrong");
    }
}

#[test]
fn an_unmodified_archive_verifies() {
    let pool = ThreadPool::new(2);
    for codec in [Codec::None, Codec::Lz4, Codec::Deflate] {
        let bytes = small_archive(codec);
        let (r, sink) = read(&pool, &bytes, false).unwrap();
        assert!(r.ok(), "{codec:?}: {:?}", r.problems);
        assert_eq!(sink.files.len(), 5);
        assert_no_wrong_data(&sink, "pristine");
    }
}

#[test]
fn every_single_bit_flip_is_detected() {
    let pool = ThreadPool::new(2);
    for codec in [Codec::None, Codec::Lz4, Codec::Deflate] {
        let bytes = small_archive(codec);
        let mut undetected = Vec::new();
        println!("{codec:?}: archive of {} bytes, flipping each of its {} bits", bytes.len(), bytes.len() * 8);
        for bit in 0..bytes.len() * 8 {
            let mut c = bytes.clone();
            c[bit / 8] ^= 1 << (bit % 8);
            match read(&pool, &c, true) {
                None => {} // refused outright (a damaged header)
                Some((r, sink)) => {
                    if r.ok() {
                        undetected.push(bit);
                    }
                    assert_no_wrong_data(&sink, &format!("{codec:?} bit {bit}"));
                }
            }
        }
        assert!(undetected.is_empty(), "{codec:?}: {} of {} flipped bits went unnoticed, e.g. byte {:?}", undetected.len(), bytes.len() * 8, undetected.iter().take(5).map(|b| b / 8).collect::<Vec<_>>());
    }
}

#[test]
fn every_truncation_is_detected_and_complete_files_are_still_recovered() {
    let pool = ThreadPool::new(2);
    let bytes = small_archive(Codec::Lz4);
    let a = Archive::open(Cursor::new(bytes.clone()), 1).unwrap();
    // End of each file's records: where the next entry starts, or where the index starts.
    let mut ends: Vec<(String, u64)> = Vec::new();
    for (i, e) in a.entries.iter().enumerate() {
        if e.kind == EntryKind::File {
            let end = a.entries.get(i + 1).map_or(a.footer.index_offset, |n| n.offset);
            ends.push((e.path.clone(), end));
        }
    }
    for cut in 0..bytes.len() {
        let Some((r, sink)) = read(&pool, &bytes[..cut], false) else {
            continue; // not even a header
        };
        assert!(!r.ok(), "cut at {cut} of {} was not noticed", bytes.len());
        assert_no_wrong_data(&sink, &format!("cut {cut}"));
        for (path, end) in &ends {
            if *end <= cut as u64 {
                assert!(sink.files.contains_key(path), "cut {cut}: {path} ends at {end} and should have been recovered");
            }
        }
    }
}

#[test]
fn random_bursts_of_damage_never_yield_wrong_data() {
    let pool = ThreadPool::new(2);
    let mut rng = Rng(0xBAD5EED);
    let mut recovered_something = 0;
    for codec in [Codec::None, Codec::Lz4, Codec::Deflate] {
        let bytes = small_archive(codec);
        for trial in 0..300 {
            let mut c = bytes.clone();
            let len = 1 + rng.below(80) as usize;
            let start = rng.below((c.len() - len) as u64 + 1) as usize;
            for b in &mut c[start..start + len] {
                *b ^= 1 + rng.below(255) as u8;
            }
            for recover in [false, true] {
                if let Some((r, sink)) = read(&pool, &c, recover) {
                    assert!(!r.ok(), "{codec:?} trial {trial}: damage at {start}+{len} was not noticed");
                    assert_no_wrong_data(&sink, &format!("{codec:?} trial {trial} recover {recover}"));
                    if recover && !sink.files.is_empty() {
                        recovered_something += 1;
                    }
                }
            }
        }
    }
    assert!(recovered_something > 500, "recovery should usually save most files, saved something in {recovered_something} trials");
}

fn record_offsets(bytes: &[u8]) -> Vec<(u64, Kind, u64)> {
    let mut inp = Input::new(Cursor::new(bytes[HEADER_LEN..].to_vec()), HEADER_LEN as u64);
    let mut out = Vec::new();
    while let Ok(step) = inp.read_step() {
        match step {
            Step::Record(r) => {
                let end = inp.offset();
                out.push((r.offset, r.head.kind, end));
            }
            Step::Footer(_) | Step::Eof => break,
            Step::Damage { .. } => break,
        }
    }
    out
}

#[test]
fn a_damaged_chunk_costs_only_its_own_file_even_in_strict_mode() {
    let pool = ThreadPool::new(2);
    let bytes = small_archive(Codec::None);
    let a = Archive::open(Cursor::new(bytes.clone()), 1).unwrap();
    let victim = a.entries.iter().find(|e| e.path == "a/multi.txt").unwrap();
    // The second chunk record of that file: its payload sits after the record head and meta.
    let recs = record_offsets(&bytes);
    let chunk_recs: Vec<_> = recs.iter().filter(|(o, k, _)| *k == Kind::Chunk && *o > victim.offset).take(2).collect();
    let (off, _, end) = *chunk_recs[1];
    let mut c = bytes.clone();
    c[(off + (end - off) / 2) as usize] ^= 0x55;
    let (r, sink) = read(&pool, &c, false).unwrap();
    assert!(!r.ok());
    assert_eq!((r.files_bad, r.files_ok), (1, 4), "{:?}", r.problems);
    assert!(!sink.files.contains_key("a/multi.txt"));
    assert_eq!(sink.files.len(), 4);
    assert!(r.problems.iter().any(|p| p.path.as_deref() == Some("a/multi.txt") && p.message.contains("chunk 1")), "{:?}", r.problems);
}

#[test]
fn a_lost_record_header_stops_strict_reading_and_recovery_resumes_after_it() {
    let pool = ThreadPool::new(2);
    let bytes = small_archive(Codec::Lz4);
    let a = Archive::open(Cursor::new(bytes.clone()), 1).unwrap();
    let second = a.entries.iter().find(|e| e.path == "a/random.bin").unwrap();
    let mut c = bytes.clone();
    c[second.offset as usize] ^= 0xFF; // the marker of the second file's first record

    let (strict, sink) = read(&pool, &c, false).unwrap();
    assert!(!strict.ok());
    assert!(strict.problems.iter().any(|p| p.message.contains("--recover")), "{:?}", strict.problems);
    assert!(sink.files.contains_key("a/multi.txt") && !sink.files.contains_key("b/last.txt"));

    let (rec, sink) = read(&pool, &c, true).unwrap();
    assert!(!rec.ok());
    assert!(rec.skipped_bytes > 0);
    // Everything except the file whose first record was destroyed comes back, and correctly.
    assert!(sink.files.contains_key("a/multi.txt") && sink.files.contains_key("b/last.txt") && sink.files.contains_key("b/small.txt"), "{:?}", sink.files.keys().collect::<Vec<_>>());
    assert!(!sink.files.contains_key("a/random.bin"));
    assert_no_wrong_data(&sink, "recovered");
}

#[test]
fn a_damaged_index_or_footer_does_not_stop_extraction() {
    let pool = ThreadPool::new(2);
    let bytes = small_archive(Codec::Deflate);
    let a = Archive::open(Cursor::new(bytes.clone()), 1).unwrap();
    for (what, at) in [("index", (a.footer.index_offset + 50) as usize), ("footer", bytes.len() - 20)] {
        let mut c = bytes.clone();
        c[at] ^= 0x01;
        assert!(Archive::open(Cursor::new(c.clone()), 1).is_err(), "{what}: random access refuses a damaged {what}");
        let (r, sink) = read(&pool, &c, false).unwrap();
        assert!(!r.ok(), "{what}");
        assert_eq!(sink.files.len(), 5, "{what}: every file is still read from the front");
        assert_no_wrong_data(&sink, what);
        assert_eq!(r.files_ok, 5);
    }
}

#[test]
fn an_archive_cut_off_before_its_index_still_yields_every_file() {
    let pool = ThreadPool::new(2);
    let bytes = small_archive(Codec::Lz4);
    let a = Archive::open(Cursor::new(bytes.clone()), 1).unwrap();
    let cut = &bytes[..a.footer.index_offset as usize];
    let (r, sink) = read(&pool, cut, false).unwrap();
    assert!(!r.ok());
    assert!(r.problems.iter().any(|p| p.message.contains("no valid footer")), "{:?}", r.problems);
    assert_eq!(sink.files.len(), 5);
    assert_no_wrong_data(&sink, "cut before the index");
}

#[test]
fn missing_repeated_and_swapped_chunks_are_detected() {
    let pool = ThreadPool::new(2);
    let bytes = small_archive(Codec::None);
    let recs = record_offsets(&bytes);
    let a = Archive::open(Cursor::new(bytes.clone()), 1).unwrap();
    let victim = a.entries.iter().find(|e| e.path == "a/multi.txt").unwrap();
    let chunks: Vec<(u64, u64)> = recs.iter().filter(|(o, k, _)| *k == Kind::Chunk && *o > victim.offset).take(3).map(|(o, _, e)| (*o, *e)).collect();
    let part = |i: usize| bytes[chunks[i].0 as usize..chunks[i].1 as usize].to_vec();

    let rebuild = |order: &[usize]| -> Vec<u8> {
        let mut out = bytes[..chunks[0].0 as usize].to_vec();
        for &i in order {
            out.extend(part(i));
        }
        out.extend_from_slice(&bytes[chunks[2].1 as usize..]);
        out
    };
    for (name, order) in [("dropped", vec![0, 2]), ("duplicated", vec![0, 1, 1, 2]), ("swapped", vec![1, 0, 2]), ("repeated first", vec![0, 0, 1, 2])] {
        let c = rebuild(&order);
        let (r, sink) = read(&pool, &c, true).unwrap();
        assert!(!r.ok(), "{name}: not noticed");
        assert!(!sink.files.contains_key("a/multi.txt"), "{name}: the damaged file must not be accepted");
        assert_no_wrong_data(&sink, name);
    }
}

#[test]
fn random_garbage_and_edited_archives_never_panic_or_hang() {
    let pool = ThreadPool::new(2);
    let mut rng = Rng(0xF00D);
    let good = small_archive(Codec::Lz4);
    // Pure noise after a valid header.
    for _ in 0..300 {
        let n = rng.below(3000) as usize;
        let mut c = good[..HEADER_LEN].to_vec();
        c.extend(rng.bytes(n));
        let _ = read(&pool, &c, true);
        let _ = read(&pool, &c, false);
    }
    // A valid archive with segments inserted, removed and repeated.
    for _ in 0..600 {
        let mut c = good.clone();
        for _ in 0..1 + rng.below(4) {
            let at = rng.below(c.len() as u64) as usize;
            match rng.below(3) {
                0 => {
                    let n = 1 + rng.below(100) as usize;
                    let junk = rng.bytes(n);
                    c.splice(at..at, junk).for_each(drop);
                }
                1 => {
                    let n = (1 + rng.below(100) as usize).min(c.len() - at);
                    c.drain(at..at + n);
                }
                _ => {
                    let n = (1 + rng.below(200) as usize).min(c.len() - at);
                    let seg = c[at..at + n].to_vec();
                    c.splice(at..at, seg).for_each(drop);
                }
            }
        }
        if let Some((r, sink)) = read(&pool, &c, true) {
            assert_no_wrong_data(&sink, "edited");
            let _ = r;
        }
    }
}
