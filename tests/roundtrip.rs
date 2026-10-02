mod common;

use std::fs;
use std::io::{self, Cursor, Read};

use arx::entry::EntryKind;
use arx::format::{Codec, HEADER_LEN};
use arx::reader::{scan, Archive, Existing, FsSink, NullSink};
use arx::writer::ArchiveWriter;
use common::*;

const CHUNK: usize = 4096;

fn extract_stream(bytes: &[u8], dest: &std::path::Path, threads: usize) -> arx::reader::Report {
    let mut sink = FsSink::new(dest, Existing::Fail, &[]);
    scan(Cursor::new(bytes.to_vec()), threads, &mut sink, false).unwrap()
}

#[test]
fn every_codec_chunk_size_and_thread_count_round_trips_a_tree() {
    for codec in [Codec::None, Codec::Lz4, Codec::Deflate] {
        for (chunk, threads) in [(CHUNK, 1), (CHUNK, 8), (1 << 16, 3), (512, 4)] {
            let src = tmp();
            let tree = src.path().join("proj");
            fs::create_dir(&tree).unwrap();
            make_tree(&tree, chunk.max(CHUNK));
            let bytes = archive_tree(&tree, "proj", config(codec, chunk, threads));

            let out = tmp();
            let report = extract_stream(&bytes, out.path(), threads);
            assert!(report.ok(), "{codec:?} chunk {chunk} threads {threads}: {:?}", report.problems);
            assert_eq!(snapshot(&tree), snapshot(&out.path().join("proj")), "{codec:?} chunk {chunk}");
            assert!(report.files_bad == 0 && report.files_ok > 50);
            // The index was cross-checked against the records read, and agrees.
            assert!(report.index.is_some() && report.footer.is_some());
        }
    }
}

#[test]
fn modification_times_survive() {
    let src = tmp();
    let tree = src.path().join("t");
    fs::create_dir(&tree).unwrap();
    make_tree(&tree, CHUNK);
    let bytes = archive_tree(&tree, "t", config(Codec::Lz4, CHUNK, 2));
    let out = tmp();
    assert!(extract_stream(&bytes, out.path(), 2).ok());
    for rel in ["one.bin", "five-chunks.txt", "dir/sub/deep/file.txt", ".hidden/config"] {
        let a = mtime_of(&tree.join(rel));
        let b = mtime_of(&out.path().join("t").join(rel));
        assert_eq!(a, b, "{rel}");
    }
    // Directories too (set after their contents were written).
    assert_eq!(mtime_of(&tree.join("dir")), mtime_of(&out.path().join("t/dir")));
}

#[test]
fn the_archive_is_identical_whatever_the_thread_count() {
    let src = tmp();
    let tree = src.path().join("t");
    fs::create_dir(&tree).unwrap();
    make_tree(&tree, CHUNK);
    let a = archive_tree(&tree, "t", config(Codec::Deflate, CHUNK, 1));
    let b = archive_tree(&tree, "t", config(Codec::Deflate, CHUNK, 8));
    // Only the creation time in the header may differ.
    assert_eq!(a.len(), b.len());
    assert_eq!(a[HEADER_LEN..], b[HEADER_LEN..]);
}

#[test]
fn random_access_extraction_and_listing_use_the_index() {
    let src = tmp();
    let tree = src.path().join("t");
    fs::create_dir(&tree).unwrap();
    let files = make_tree(&tree, CHUNK);
    let bytes = archive_tree(&tree, "t", config(Codec::Lz4, CHUNK, 4));
    let mut a = Archive::open(Cursor::new(bytes), 4).unwrap();
    assert_eq!(a.entries.iter().filter(|e| e.kind == EntryKind::File).count(), files.len());
    assert!(a.entries.iter().any(|e| e.path == "t/empty-dir/nested-empty" && e.kind == EntryKind::Dir));
    let five = a.entries.iter().find(|e| e.path == "t/five-chunks.txt").unwrap();
    assert_eq!((five.size, five.chunks), ((CHUNK * 5 + 17) as u64, 6));

    // One file, one directory, a file that does not exist.
    let out = tmp();
    let mut sink = FsSink::new(out.path(), Existing::Fail, &[]);
    let report = a.extract_selected(&mut sink, &["t/five-chunks.txt".to_string(), "t/dir/sub".to_string()]).unwrap();
    assert!(report.ok(), "{:?}", report.problems);
    let got = snapshot(out.path());
    let paths: Vec<_> = got.iter().map(|(p, _)| p.as_str()).collect();
    assert!(paths.contains(&"t/five-chunks.txt") && paths.contains(&"t/dir/sub/deep/file.txt") && !paths.contains(&"t/dir/日本語.dat"), "{paths:?}");
    assert!(!paths.contains(&"t/one.bin"), "unselected files stay out: {paths:?}");
    let want = files.iter().find(|(p, _)| p == "five-chunks.txt").unwrap().1.clone();
    assert_eq!(fs::read(out.path().join("t/five-chunks.txt")).unwrap(), want);
}

#[test]
fn verify_walks_the_whole_archive_in_both_ways() {
    let src = tmp();
    let tree = src.path().join("t");
    fs::create_dir(&tree).unwrap();
    make_tree(&tree, CHUNK);
    let bytes = archive_tree(&tree, "t", config(Codec::Deflate, CHUNK, 4));
    let mut a = Archive::open(Cursor::new(bytes.clone()), 4).unwrap();
    let r = a.verify(false).unwrap();
    assert!(r.ok(), "{:?}", r.problems);
    let mut sink = NullSink;
    let r2 = scan(Cursor::new(bytes), 4, &mut sink, false).unwrap();
    assert!(r2.ok());
    assert_eq!((r.files_ok, r.dirs, r.bytes), (r2.files_ok, r2.dirs, r2.bytes));
}

#[test]
fn a_stream_that_delivers_odd_sized_pieces_extracts_the_same() {
    struct Dribble(Cursor<Vec<u8>>, u64);
    impl Read for Dribble {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            self.1 = self.1.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let n = (1 + (self.1 >> 33) as usize % 700).min(b.len());
            self.0.read(&mut b[..n])
        }
    }
    let src = tmp();
    let tree = src.path().join("t");
    fs::create_dir(&tree).unwrap();
    make_tree(&tree, CHUNK);
    let bytes = archive_tree(&tree, "t", config(Codec::Lz4, CHUNK, 4));
    let out = tmp();
    let mut sink = FsSink::new(out.path(), Existing::Fail, &[]);
    let report = scan(Dribble(Cursor::new(bytes), 7), 4, &mut sink, false).unwrap();
    assert!(report.ok(), "{:?}", report.problems);
    assert_eq!(snapshot(&tree), snapshot(&out.path().join("t")));
}

#[test]
fn files_can_be_added_from_memory_and_directories_and_links_are_listed() {
    let mut w = ArchiveWriter::new(Cursor::new(Vec::new()), config(Codec::Lz4, CHUNK, 2)).unwrap();
    let now = std::time::SystemTime::now();
    w.add_dir("d", now, 0o755).unwrap();
    let data = text(20_000, 9);
    w.add_file("d/mem.txt", data.len() as u64, now, 0o644, Cursor::new(data.clone())).unwrap();
    w.add_symlink("d/link", "mem.txt", now, 0o777).unwrap();
    let (c, stats) = w.finish().unwrap();
    assert_eq!((stats.files, stats.dirs, stats.symlinks), (1, 1, 1));
    assert!(stats.stored_bytes < stats.raw_bytes / 2, "text should compress: {stats:?}");
    let a = Archive::open(Cursor::new(c.into_inner()), 2).unwrap();
    let kinds: Vec<_> = a.entries.iter().map(|e| (e.path.as_str(), e.kind)).collect();
    assert_eq!(kinds, [("d", EntryKind::Dir), ("d/mem.txt", EntryKind::File), ("d/link", EntryKind::Symlink)]);
    assert_eq!(a.entries[2].target, "mem.txt");
}

#[test]
fn a_file_that_shrinks_while_being_archived_is_an_error() {
    let mut w = ArchiveWriter::new(Cursor::new(Vec::new()), config(Codec::None, CHUNK, 1)).unwrap();
    let e = w.add_file("short.bin", 10_000, std::time::SystemTime::now(), 0o644, Cursor::new(vec![1u8; 5_000])).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
    assert!(e.to_string().contains("shorter"), "{e}");
}

#[test]
fn existing_files_are_not_overwritten_unless_asked() {
    let src = tmp();
    let tree = src.path().join("t");
    fs::create_dir(&tree).unwrap();
    fs::write(tree.join("a.txt"), b"from the archive").unwrap();
    let bytes = archive_tree(&tree, "t", config(Codec::Lz4, CHUNK, 1));

    let out = tmp();
    fs::create_dir_all(out.path().join("t")).unwrap();
    fs::write(out.path().join("t/a.txt"), b"already here").unwrap();
    let mut sink = FsSink::new(out.path(), Existing::Fail, &[]);
    let r = scan(Cursor::new(bytes.clone()), 1, &mut sink, false).unwrap();
    assert!(!r.ok());
    assert!(r.problems[0].message.contains("already exists"), "{:?}", r.problems);
    assert_eq!(fs::read(out.path().join("t/a.txt")).unwrap(), b"already here");

    let mut sink = FsSink::new(out.path(), Existing::Skip, &[]);
    assert!(scan(Cursor::new(bytes.clone()), 1, &mut sink, false).unwrap().ok());
    assert_eq!(fs::read(out.path().join("t/a.txt")).unwrap(), b"already here");

    let mut sink = FsSink::new(out.path(), Existing::Overwrite, &[]);
    assert!(scan(Cursor::new(bytes), 1, &mut sink, false).unwrap().ok());
    assert_eq!(fs::read(out.path().join("t/a.txt")).unwrap(), b"from the archive");
}

#[test]
fn a_read_only_file_is_restored_read_only_and_can_be_overwritten() {
    let src = tmp();
    let tree = src.path().join("t");
    fs::create_dir(&tree).unwrap();
    let p = tree.join("ro.txt");
    fs::write(&p, b"locked").unwrap();
    let mut perms = fs::metadata(&p).unwrap().permissions();
    perms.set_readonly(true);
    fs::set_permissions(&p, perms).unwrap();
    let bytes = archive_tree(&tree, "t", config(Codec::Lz4, CHUNK, 1));
    // make the source deletable again for the temp dir cleanup
    let out = tmp();
    assert!(extract_stream(&bytes, out.path(), 1).ok());
    assert!(fs::metadata(out.path().join("t/ro.txt")).unwrap().permissions().readonly());
    let mut sink = FsSink::new(out.path(), Existing::Overwrite, &[]);
    assert!(scan(Cursor::new(bytes), 1, &mut sink, false).unwrap().ok(), "overwriting a read-only file works");
    for f in [&p, &out.path().join("t/ro.txt")] {
        let mut perms = fs::metadata(f).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        fs::set_permissions(f, perms).unwrap();
    }
}

#[test]
fn overwriting_replaces_a_symlink_instead_of_writing_through_it() {
    let src = tmp();
    let tree = src.path().join("t");
    fs::create_dir(&tree).unwrap();
    fs::write(tree.join("victim.txt"), b"from the archive").unwrap();
    let bytes = archive_tree(&tree, "t", config(Codec::Lz4, CHUNK, 1));

    let out = tmp();
    let outside = tmp();
    let precious = outside.path().join("precious.txt");
    fs::write(&precious, b"keep me").unwrap();
    fs::create_dir_all(out.path().join("t")).unwrap();
    let link = out.path().join("t/victim.txt");
    #[cfg(windows)]
    let made = std::os::windows::fs::symlink_file(&precious, &link);
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(&precious, &link);
    if made.is_err() {
        eprintln!("skipped: this account may not create symbolic links");
        return;
    }
    let mut sink = FsSink::new(out.path(), Existing::Overwrite, &[]);
    assert!(scan(Cursor::new(bytes), 1, &mut sink, false).unwrap().ok());
    assert_eq!(fs::read(&precious).unwrap(), b"keep me", "the file the link pointed at must be untouched");
    assert!(!fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "the link was replaced by a regular file");
    assert_eq!(fs::read(&link).unwrap(), b"from the archive");
}
