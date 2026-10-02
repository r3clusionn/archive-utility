//! Archives made by hand to be wrong in exactly one way while every checksum on every record is
//! right. Each test pins one defence that no accidental corruption would exercise alone.

mod common;

use std::collections::HashMap;
use std::io::Cursor;

use arx::entry::*;
use arx::format::*;
use arx::reader::{scan_with, Archive, NullSink, Report};
use common::forge::Forge;
use common::*;
use workpool::ThreadPool;

fn verify(bytes: &[u8]) -> Report {
    let pool = ThreadPool::new(2);
    scan_with(Cursor::new(bytes.to_vec()), &pool, &mut NullSink, false).unwrap()
}

fn messages(r: &Report) -> String {
    r.problems.iter().map(|p| p.message.clone()).collect::<Vec<_>>().join(" | ")
}

#[test]
fn a_well_formed_forged_archive_verifies() {
    let mut f = Forge::new(1024);
    f.file(1, "a.txt", b"hello");
    f.file(2, "b/c.txt", b"world!");
    let r = verify(&f.finish());
    assert!(r.ok(), "{:?}", r.problems);
    assert_eq!(r.files_ok, 2);
}

#[test]
fn a_chunk_whose_data_does_not_match_its_digest_is_caught_even_when_the_chain_agrees() {
    // A writer bug: the digest claimed for the chunk is wrong, and the file digest was chained over
    // the claimed value, so only comparing the chunk's data with its digest can notice.
    let data = b"the real chunk contents";
    let wrong = *blake3::hash(b"something else").as_bytes();
    let mut f = Forge::new(1024);
    let i = f.begin(1, "f.txt", data.len() as u64);
    f.chunk_raw(1, 0, 0, data, wrong);
    f.end(i, 1, data.len() as u64, data.len() as u64, &[wrong]);
    let r = verify(&f.finish());
    assert!(!r.ok());
    assert!(messages(&r).contains("checksum does not match its data"), "{}", messages(&r));
    assert_eq!(r.files_bad, 1);
}

#[test]
fn chunks_out_of_order_are_caught_even_when_the_chain_agrees() {
    // The two chunks are stored in the wrong order and the chain was computed over the order they
    // appear in, so only the chunk numbers give it away.
    let (d0, d1) = (b"first half....".to_vec(), b"second half...".to_vec());
    let (h0, h1) = (*blake3::hash(&d0).as_bytes(), *blake3::hash(&d1).as_bytes());
    let mut f = Forge::new(1024);
    let i = f.begin(1, "f.txt", (d0.len() + d1.len()) as u64);
    f.chunk(1, 1, d0.len() as u64, &d1);
    f.chunk(1, 0, 0, &d0);
    f.end(i, 2, (d0.len() + d1.len()) as u64, (d0.len() + d1.len()) as u64, &[h1, h0]);
    let r = verify(&f.finish());
    assert!(!r.ok());
    assert!(messages(&r).contains("missing or out of order"), "{}", messages(&r));
}

#[test]
fn a_file_digest_that_does_not_match_its_chunks_is_caught() {
    // Every chunk is perfect and in order, but the end record carries the digest of other data, as
    // happens when chunks from two versions of a file are spliced together.
    let data = b"version two of the data";
    let mut f = Forge::new(1024);
    let i = f.begin(1, "f.txt", data.len() as u64);
    f.chunk(1, 0, 0, data);
    f.end(i, 1, data.len() as u64, data.len() as u64, &[*blake3::hash(b"version one of the data").as_bytes()]);
    let r = verify(&f.finish());
    assert!(!r.ok());
    assert!(messages(&r).contains("digest does not match its chunks"), "{}", messages(&r));
}

#[test]
fn chunks_spliced_in_from_another_version_of_the_archive_are_caught() {
    // Two archives of "the same" file with the same size: one chunk of the second is replaced by the
    // first archive's. Every record is intact and well framed.
    let chunk = 256;
    let v1 = text(chunk * 3, 1);
    let v2 = text(chunk * 3, 2);
    let build = |data: &[u8]| {
        let mut w = arx::writer::ArchiveWriter::new(Cursor::new(Vec::new()), config(Codec::None, chunk, 1)).unwrap();
        w.add_file("f.txt", data.len() as u64, std::time::SystemTime::UNIX_EPOCH, 0o644, Cursor::new(data.to_vec())).unwrap();
        w.finish().unwrap().0.into_inner()
    };
    let (a, b) = (build(&v1), build(&v2));
    let chunks_of = |bytes: &[u8]| -> Vec<(usize, usize)> {
        let ar = Archive::open(Cursor::new(bytes.to_vec()), 1).unwrap();
        let start = ar.entries[0].offset as usize;
        let rec = REC_HEADER_LEN + 24 + 4; // head, 24 bytes of chunk meta, its checksum
        let one = rec + chunk + 36;
        let first = start + REC_HEADER_LEN + encode_begin(&ar.entries[0]).len() + 4;
        (0..3).map(|i| (first + i * one, first + (i + 1) * one)).collect()
    };
    let (ca, cb) = (chunks_of(&a), chunks_of(&b));
    let mut spliced = b[..cb[1].0].to_vec();
    spliced.extend_from_slice(&a[ca[1].0..ca[1].1]);
    spliced.extend_from_slice(&b[cb[1].1..]);
    assert_eq!(spliced.len(), b.len());
    let r = verify(&spliced);
    assert!(!r.ok(), "a spliced chunk must not pass");
    assert!(messages(&r).contains("digest does not match its chunks"), "{}", messages(&r));
}

#[test]
fn an_index_that_disagrees_with_the_records_is_reported() {
    let mut f = Forge::new(1024);
    f.file(1, "a.txt", b"alpha");
    f.file(2, "b.txt", b"bravo!");
    let mut lie = f.entries.clone();
    lie[1].digest[0] ^= 1; // the index claims another digest for b.txt, with a valid checksum of its own
    let bytes = f.finish_with(Some(&lie));
    assert!(Archive::open(Cursor::new(bytes.clone()), 1).is_ok(), "the index is well formed, so it opens");
    let r = verify(&bytes);
    assert!(!r.ok());
    assert!(messages(&r).contains("index does not match"), "{}", messages(&r));
    assert_eq!(r.files_ok, 2, "the data itself is fine");
}

#[test]
fn an_index_with_a_wrong_entry_count_or_offset_is_reported() {
    let mut f = Forge::new(1024);
    f.file(1, "a.txt", b"alpha");
    let mut missing = f.entries.clone();
    missing.clear();
    assert!(!verify(&f.finish_with(Some(&missing))).ok());

    let mut g = Forge::new(1024);
    g.file(1, "a.txt", b"alpha");
    let mut moved = g.entries.clone();
    moved[0].offset += 7;
    let r = verify(&g.finish_with(Some(&moved)));
    assert!(!r.ok());
}

#[test]
fn a_file_missing_its_end_record_is_reported_as_incomplete() {
    let mut f = Forge::new(1024);
    let i = f.begin(1, "cut.txt", 5);
    f.chunk(1, 0, 0, b"hello");
    let _ = i;
    f.file(2, "ok.txt", b"fine");
    let r = verify(&f.finish());
    assert!(!r.ok());
    assert!(messages(&r).contains("incomplete"), "{}", messages(&r));
    assert_eq!((r.files_bad, r.files_ok), (1, 1));
}

#[test]
fn sizes_that_do_not_add_up_are_reported() {
    // The file claims 10 bytes, its one chunk has 5.
    let mut f = Forge::new(1024);
    let i = f.begin(1, "liar.txt", 10);
    f.chunk(1, 0, 0, b"hello");
    f.end(i, 1, 5, 5, &[*blake3::hash(b"hello").as_bytes()]);
    let r = verify(&f.finish());
    assert!(!r.ok());
    assert!(messages(&r).contains("changed size"), "{}", messages(&r));
}

#[test]
fn a_chunk_claiming_more_data_than_the_archive_chunk_size_is_refused() {
    let big = vec![7u8; 2000];
    let mut f = Forge::new(1024); // header promises chunks of at most 1 KiB
    let i = f.begin(1, "big.bin", 2000);
    f.chunk(1, 0, 0, &big);
    f.end(i, 1, 2000, 2000, &[*blake3::hash(&big).as_bytes()]);
    let r = verify(&f.finish());
    assert!(!r.ok());
    assert!(messages(&r).contains("more than the archive's chunk size"), "{}", messages(&r));
}

#[test]
fn extraction_refuses_unsafe_paths_and_still_extracts_the_rest() {
    let evil = [
        "../escape.txt",
        "a/../../escape.txt",
        "/abs/escape.txt",
        "C:/escape.txt",
        "C:\\escape.txt",
        "..\\escape.txt",
        "ok/../../escape.txt",
        "aux",
        "name:stream",
        "trailing.",
    ];
    let base = tmp();
    let dest = base.path().join("dest");
    // A drive-relative path such as `C:name` lands in the process's working directory, so look there too.
    let cwd_escape = std::env::current_dir().unwrap().join("escape.txt");
    let _ = std::fs::remove_file(&cwd_escape);
    let mut f = Forge::new(1024);
    for (n, p) in evil.iter().enumerate() {
        f.file(n as u64 + 1, p, b"payload that must not land outside");
    }
    f.file(100, "safe/good.txt", b"this one is fine");
    let bytes = f.finish();

    let mut sink = arx::reader::FsSink::new(&dest, arx::reader::Existing::Fail, &[]);
    let pool = ThreadPool::new(2);
    std::fs::create_dir_all(&dest).unwrap();
    let r = scan_with(Cursor::new(bytes), &pool, &mut sink, false).unwrap();
    assert!(!r.ok());
    assert_eq!(r.problems.iter().filter(|p| p.message.contains("refused to extract")).count(), evil.len(), "{:?}", r.problems);
    assert_eq!(std::fs::read(dest.join("safe/good.txt")).unwrap(), b"this one is fine");
    // Nothing escaped: the only things in the temp dir are the destination, and in it only the safe file.
    let top: Vec<_> = std::fs::read_dir(base.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(top, ["dest"], "{top:?}");
    let inside = snapshot(&dest);
    assert_eq!(inside.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(), ["safe", "safe/good.txt"]);
    // and not in the parent of the temp dir either
    assert!(!base.path().parent().unwrap().join("escape.txt").exists());
    assert!(!cwd_escape.exists(), "a file escaped into the working directory");
    let _ = HashMap::<u8, u8>::new();
}
