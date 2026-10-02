//! What the records carry: file entries and how they are serialized.

use crate::format::{bad, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    File = 1,
    Dir = 2,
    Symlink = 3,
}

impl EntryKind {
    pub fn from_u8(b: u8) -> Option<EntryKind> {
        Some(match b {
            1 => EntryKind::File,
            2 => EntryKind::Dir,
            3 => EntryKind::Symlink,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub kind: EntryKind,
    pub id: u64,
    /// Relative, `/`-separated, UTF-8.
    pub path: String,
    /// Uncompressed size (files only).
    pub size: u64,
    pub mtime_secs: i64,
    pub mtime_nanos: u32,
    /// Unix permission bits (`0o444` stands for read-only on Windows).
    pub mode: u32,
    /// Where the entry's first record starts in the archive.
    pub offset: u64,
    pub chunks: u64,
    /// Bytes of compressed payload (files only).
    pub stored: u64,
    /// BLAKE3 over the chunk digests in order (files only).
    pub digest: [u8; 32],
    /// Link target (symlinks only).
    pub target: String,
}

impl Entry {
    pub fn new(kind: EntryKind, id: u64, path: String) -> Entry {
        Entry { kind, id, path, size: 0, mtime_secs: 0, mtime_nanos: 0, mode: 0o644, offset: 0, chunks: 0, stored: 0, digest: [0; 32], target: String::new() }
    }
}

/// Appends little-endian values to a buffer.
#[derive(Default)]
pub struct Enc(pub Vec<u8>);

impl Enc {
    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn i64(&mut self, v: i64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.0.extend_from_slice(v);
        self
    }
    pub fn str(&mut self, s: &str) -> &mut Self {
        self.u32(s.len() as u32);
        self.bytes(s.as_bytes())
    }
}

/// Reads little-endian values from a slice, failing instead of panicking when it runs out.
pub struct Dec<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Dec<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Dec { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.buf.len() - self.pos < n {
            return bad("a record's meta data is shorter than its fields");
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    pub fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    pub fn digest(&mut self) -> Result<[u8; 32]> {
        Ok(self.take(32)?.try_into().unwrap())
    }
    pub fn str(&mut self) -> Result<String> {
        let n = self.u32()? as usize;
        let s = self.take(n)?;
        String::from_utf8(s.to_vec()).or_else(|_| bad("a path is not valid UTF-8"))
    }
    pub fn finish(&self) -> Result<()> {
        if self.pos != self.buf.len() {
            return bad("a record's meta data is longer than its fields");
        }
        Ok(())
    }
}

pub fn encode_begin(e: &Entry) -> Vec<u8> {
    let mut m = Enc::default();
    m.u64(e.id).str(&e.path).u64(e.size).i64(e.mtime_secs).u32(e.mtime_nanos).u32(e.mode);
    m.0
}

pub fn decode_begin(meta: &[u8]) -> Result<Entry> {
    let mut d = Dec::new(meta);
    let mut e = Entry::new(EntryKind::File, d.u64()?, d.str()?);
    e.size = d.u64()?;
    e.mtime_secs = d.i64()?;
    e.mtime_nanos = d.u32()?;
    e.mode = d.u32()?;
    d.finish()?;
    Ok(e)
}

pub fn encode_dir(e: &Entry) -> Vec<u8> {
    let mut m = Enc::default();
    m.u64(e.id).str(&e.path).i64(e.mtime_secs).u32(e.mtime_nanos).u32(e.mode);
    m.0
}

pub fn decode_dir(meta: &[u8]) -> Result<Entry> {
    let mut d = Dec::new(meta);
    let mut e = Entry::new(EntryKind::Dir, d.u64()?, d.str()?);
    e.mtime_secs = d.i64()?;
    e.mtime_nanos = d.u32()?;
    e.mode = d.u32()?;
    d.finish()?;
    Ok(e)
}

pub fn encode_symlink(e: &Entry) -> Vec<u8> {
    let mut m = Enc::default();
    m.u64(e.id).str(&e.path).str(&e.target).i64(e.mtime_secs).u32(e.mtime_nanos).u32(e.mode);
    m.0
}

pub fn decode_symlink(meta: &[u8]) -> Result<Entry> {
    let mut d = Dec::new(meta);
    let mut e = Entry::new(EntryKind::Symlink, d.u64()?, d.str()?);
    e.target = d.str()?;
    e.mtime_secs = d.i64()?;
    e.mtime_nanos = d.u32()?;
    e.mode = d.u32()?;
    d.finish()?;
    Ok(e)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkMeta {
    pub file_id: u64,
    pub index: u64,
    /// Byte offset of this chunk within the file.
    pub offset: u64,
}

pub fn encode_chunk_meta(c: &ChunkMeta) -> Vec<u8> {
    let mut m = Enc::default();
    m.u64(c.file_id).u64(c.index).u64(c.offset);
    m.0
}

pub fn decode_chunk_meta(meta: &[u8]) -> Result<ChunkMeta> {
    let mut d = Dec::new(meta);
    let c = ChunkMeta { file_id: d.u64()?, index: d.u64()?, offset: d.u64()? };
    d.finish()?;
    Ok(c)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EndMeta {
    pub file_id: u64,
    pub chunks: u64,
    pub stored: u64,
    /// The size the file turned out to have: the sum of its chunks.
    pub size: u64,
    pub digest: [u8; 32],
}

pub fn encode_end(e: &EndMeta) -> Vec<u8> {
    let mut m = Enc::default();
    m.u64(e.file_id).u64(e.chunks).u64(e.stored).u64(e.size).bytes(&e.digest);
    m.0
}

pub fn decode_end(meta: &[u8]) -> Result<EndMeta> {
    let mut d = Dec::new(meta);
    let e = EndMeta { file_id: d.u64()?, chunks: d.u64()?, stored: d.u64()?, size: d.u64()?, digest: d.digest()? };
    d.finish()?;
    Ok(e)
}

pub fn encode_index(entries: &[Entry]) -> Vec<u8> {
    let mut m = Enc::default();
    m.u64(entries.len() as u64);
    for e in entries {
        m.u8(e.kind as u8).u64(e.id).str(&e.path).u64(e.size).i64(e.mtime_secs).u32(e.mtime_nanos).u32(e.mode);
        m.u64(e.offset).u64(e.chunks).u64(e.stored).bytes(&e.digest).str(&e.target);
    }
    m.0
}

pub fn decode_index(meta: &[u8]) -> Result<Vec<Entry>> {
    let mut d = Dec::new(meta);
    let n = d.u64()?;
    // Each entry takes at least 100 bytes, so a count the meta cannot hold is damage, not a reason to allocate.
    if n > (meta.len() as u64) / 100 + 1 {
        return bad("the index claims more entries than it can hold");
    }
    let mut out = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let kind = EntryKind::from_u8(d.u8()?).ok_or_else(|| crate::format::FormatError::Bad("the index has an unknown entry kind".into()))?;
        let mut e = Entry::new(kind, d.u64()?, d.str()?);
        e.size = d.u64()?;
        e.mtime_secs = d.i64()?;
        e.mtime_nanos = d.u32()?;
        e.mode = d.u32()?;
        e.offset = d.u64()?;
        e.chunks = d.u64()?;
        e.stored = d.u64()?;
        e.digest = d.digest()?;
        e.target = d.str()?;
        out.push(e);
    }
    d.finish()?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Entry {
        let mut e = Entry::new(EntryKind::File, 7, "dir/файл.txt".to_string());
        e.size = 12345;
        e.mtime_secs = -5;
        e.mtime_nanos = 999;
        e.mode = 0o755;
        e.offset = 99;
        e.chunks = 3;
        e.stored = 777;
        e.digest = [9; 32];
        e
    }

    #[test]
    fn records_round_trip() {
        let e = sample();
        let b = decode_begin(&encode_begin(&e)).unwrap();
        assert_eq!((b.id, &b.path[..], b.size, b.mtime_secs, b.mtime_nanos, b.mode), (7, "dir/файл.txt", 12345, -5, 999, 0o755));
        let mut d = Entry::new(EntryKind::Dir, 1, "d".into());
        d.mode = 0o700;
        assert_eq!(decode_dir(&encode_dir(&d)).unwrap().mode, 0o700);
        let mut s = Entry::new(EntryKind::Symlink, 2, "l".into());
        s.target = "../x".into();
        assert_eq!(decode_symlink(&encode_symlink(&s)).unwrap().target, "../x");
        let c = ChunkMeta { file_id: 1, index: 2, offset: 3 };
        assert_eq!(decode_chunk_meta(&encode_chunk_meta(&c)).unwrap(), c);
        let en = EndMeta { file_id: 1, chunks: 2, stored: 3, size: 5, digest: [4; 32] };
        assert_eq!(decode_end(&encode_end(&en)).unwrap(), en);
        assert_eq!(decode_index(&encode_index(&[e.clone(), d, s])).unwrap().len(), 3);
        assert_eq!(decode_index(&encode_index(std::slice::from_ref(&e))).unwrap()[0], e);
    }

    #[test]
    fn short_or_long_meta_is_an_error_not_a_panic() {
        let meta = encode_begin(&sample());
        for cut in 0..meta.len() {
            assert!(decode_begin(&meta[..cut]).is_err(), "cut at {cut}");
        }
        let mut long = meta.clone();
        long.push(0);
        assert!(decode_begin(&long).is_err());
        assert!(decode_index(&[0xff; 16]).is_err());
        assert!(decode_index(&[]).is_err());
    }

    #[test]
    fn invalid_utf8_paths_are_rejected() {
        let mut m = Enc::default();
        m.u64(1).u32(2).bytes(&[0xff, 0xfe]).u64(0).i64(0).u32(0).u32(0);
        assert!(decode_begin(&m.0).is_err());
    }
}
