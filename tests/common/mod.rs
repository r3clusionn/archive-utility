#![allow(dead_code)]

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use arx::format::Codec;
use arx::writer::{ArchiveWriter, WriterConfig};

pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

/// Text that compresses well.
pub fn text(n: usize, seed: u64) -> Vec<u8> {
    let words = ["alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta", "iota", "kappa", "lambda"];
    let mut r = Rng(seed | 1);
    let mut out = Vec::with_capacity(n + 16);
    while out.len() < n {
        out.extend_from_slice(words[r.below(words.len() as u64) as usize].as_bytes());
        out.push(if r.below(12) == 0 { b'\n' } else { b' ' });
    }
    out.truncate(n);
    out
}

pub fn set_mtime(path: &Path, secs: u64, nanos: u32) {
    let f = fs::OpenOptions::new().write(true).open(path).or_else(|_| fs::File::open(path)).unwrap();
    f.set_modified(UNIX_EPOCH + Duration::new(secs, nanos)).unwrap();
}

/// A tree with files of awkward sizes, nesting, empty files and directories, and Unicode names.
pub fn make_tree(root: &Path, chunk: usize) -> Vec<(String, Vec<u8>)> {
    let mut r = Rng(0xC0FFEE);
    let mut files: Vec<(String, Vec<u8>)> = vec![
        ("empty.txt".into(), vec![]),
        ("one.bin".into(), vec![42]),
        ("exact-chunk.bin".into(), r.bytes(chunk)),
        ("chunk-plus-one.bin".into(), r.bytes(chunk + 1)),
        ("chunk-minus-one.txt".into(), text(chunk - 1, 3)),
        ("five-chunks.txt".into(), text(chunk * 5 + 17, 4)),
        ("random.bin".into(), r.bytes(chunk * 3 + 100)),
        ("zeros.bin".into(), vec![0; chunk * 2 + 5]),
        ("dir/sub/deep/file.txt".into(), text(5000, 5)),
        ("dir/sub/другой файл (1).txt".into(), text(300, 6)),
        ("dir/日本語.dat".into(), r.bytes(77)),
        (".hidden/config".into(), b"key=value\n".to_vec()),
    ];
    for i in 0..40 {
        files.push((format!("many/f{i:03}.txt"), text(10 + i * 13, 100 + i as u64)));
    }
    for (rel, data) in &files {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, data).unwrap();
        set_mtime(&p, 1_600_000_000 + data.len() as u64 * 7, (data.len() as u32 % 9) * 100);
    }
    fs::create_dir_all(root.join("empty-dir/nested-empty")).unwrap();
    files
}

/// Every file and directory below `root`, as sorted (relative path, contents or None for a directory).
pub fn snapshot(root: &Path) -> Vec<(String, Option<Vec<u8>>)> {
    let mut out = Vec::new();
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, Option<Vec<u8>>)>) {
        let mut kids: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap()).collect();
        kids.sort_by_key(|k| k.file_name());
        for k in kids {
            let rel = k.path().strip_prefix(base).unwrap().to_string_lossy().replace('\\', "/");
            if k.file_type().unwrap().is_dir() {
                out.push((rel, None));
                walk(base, &k.path(), out);
            } else {
                out.push((rel, Some(fs::read(k.path()).unwrap())));
            }
        }
    }
    walk(root, root, &mut out);
    out
}

pub fn mtime_of(p: &Path) -> SystemTime {
    fs::metadata(p).unwrap().modified().unwrap()
}

pub fn config(codec: Codec, chunk: usize, threads: usize) -> WriterConfig {
    WriterConfig { codec, level: 6, chunk_size: chunk, threads }
}

/// Archives `root`'s contents (stored under `name`) into memory.
pub fn archive_tree(root: &Path, name: &str, cfg: WriterConfig) -> Vec<u8> {
    let mut w = ArchiveWriter::new(Cursor::new(Vec::new()), cfg).unwrap();
    w.add_path(root, name).unwrap();
    let (c, _) = w.finish().unwrap();
    c.into_inner()
}

pub fn tmp() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

pub fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

pub fn pb(p: &Path, rel: &str) -> PathBuf {
    p.join(rel)
}

/// Builds archives record by record, with full control over every field, so a test can make an
/// archive that is wrong in exactly one way and still has every checksum right.
pub mod forge {
    use arx::entry::*;
    use arx::format::*;
    use std::io::Write;

    pub struct Forge {
        pub out: Vec<u8>,
        pub entries: Vec<Entry>,
        chunk_size: u32,
    }

    impl Forge {
        pub fn new(chunk_size: u32) -> Forge {
            let mut out = Vec::new();
            out.extend_from_slice(&Header::new(Codec::None, chunk_size, 0).encode());
            Forge { out, entries: Vec::new(), chunk_size }
        }

        pub fn chunk_size(&self) -> u32 {
            self.chunk_size
        }

        pub fn pos(&self) -> u64 {
            self.out.len() as u64
        }

        pub fn begin(&mut self, id: u64, path: &str, size: u64) -> usize {
            let mut e = Entry::new(EntryKind::File, id, path.to_string());
            e.size = size;
            e.offset = self.pos();
            self.out.write_all(&encode_record_head(Kind::FileBegin, 0, &encode_begin(&e), 0, 0)).unwrap();
            self.entries.push(e);
            self.entries.len() - 1
        }

        /// A stored (uncompressed) chunk with explicit claims about itself.
        pub fn chunk_raw(&mut self, file_id: u64, index: u64, offset: u64, data: &[u8], claimed_digest: [u8; 32]) {
            let meta = encode_chunk_meta(&ChunkMeta { file_id, index, offset });
            self.out.write_all(&encode_record_head(Kind::Chunk, 0, &meta, data.len() as u64, data.len() as u64)).unwrap();
            self.out.write_all(data).unwrap();
            self.out.write_all(&claimed_digest).unwrap();
            self.out.write_all(&crc32(&[data]).to_le_bytes()).unwrap();
        }

        pub fn chunk(&mut self, file_id: u64, index: u64, offset: u64, data: &[u8]) {
            self.chunk_raw(file_id, index, offset, data, *blake3::hash(data).as_bytes());
        }

        /// Ends a file with an explicit digest; `chain_over` are the digests to chain (normally the chunks').
        pub fn end(&mut self, entry: usize, chunks: u64, size: u64, stored: u64, chain_over: &[[u8; 32]]) {
            let mut h = blake3::Hasher::new();
            for d in chain_over {
                h.update(d);
            }
            let digest = *h.finalize().as_bytes();
            let e = &mut self.entries[entry];
            e.chunks = chunks;
            e.stored = stored;
            e.digest = digest;
            let meta = encode_end(&EndMeta { file_id: e.id, chunks, stored, size, digest });
            self.out.write_all(&encode_record_head(Kind::FileEnd, 0, &meta, 0, 0)).unwrap();
        }

        /// A whole well-formed single-chunk file.
        pub fn file(&mut self, id: u64, path: &str, data: &[u8]) {
            let i = self.begin(id, path, data.len() as u64);
            self.chunk(id, 0, 0, data);
            self.end(i, 1, data.len() as u64, data.len() as u64, &[*blake3::hash(data).as_bytes()]);
        }

        pub fn finish(mut self) -> Vec<u8> {
            self.finish_with(None)
        }

        /// Writes the index (the recorded entries, or `override_entries`) and the footer.
        pub fn finish_with(&mut self, override_entries: Option<&[Entry]>) -> Vec<u8> {
            let entries: Vec<Entry> = override_entries.map_or_else(|| self.entries.clone(), |e| e.to_vec());
            let index_offset = self.pos();
            self.out.write_all(&encode_record_head(Kind::Index, 0, &encode_index(&entries), 0, 0)).unwrap();
            let index_len = self.pos() - index_offset;
            let raw_total = entries.iter().filter(|e| e.kind == EntryKind::File).map(|e| e.size).sum();
            self.out.write_all(&Footer { index_offset, index_len, entries: entries.len() as u64, raw_total }.encode()).unwrap();
            self.out.clone()
        }
    }
}
