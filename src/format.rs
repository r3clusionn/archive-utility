//! The ARX container format, version 1. All integers are little-endian.
//!
//! ```text
//! archive  = header record* index-record footer
//! header   = "ARX\0" major:u16 minor:u16 default_codec:u8 hash:u8 flags:u16
//!            chunk_size:u32 created:u64 reserved:u32 crc32:u32           (32 bytes)
//! record   = rec-header meta meta-crc [payload digest:32 payload-crc:u32]
//! rec-header = "ARXR" kind:u8 codec:u8 reserved:u16 meta_len:u32
//!              data_len:u64 raw_len:u64 crc32:u32                        (32 bytes)
//! footer   = "ARXE" index_offset:u64 index_len:u64 entries:u64
//!            raw_total:u64 crc32:u32                                     (40 bytes)
//! ```
//!
//! Every byte of an archive is covered by a checksum: the header and each record header by their
//! own CRC-32, each record's meta data by a CRC-32, each chunk's stored bytes by a CRC-32 and their
//! uncompressed form by a BLAKE3 digest, and the footer by a CRC-32. A record header is checked before any length in
//! it is trusted. The `ARXR` marker lets a reader find the next record after damage.
//!
//! A file is stored as `FileBegin`, one `Chunk` per `chunk_size` bytes, `FileEnd`. The `FileEnd`
//! digest is BLAKE3 over the chunk digests in order, so a missing, repeated or reordered chunk is
//! detected even though every chunk is valid on its own.

use std::io::{self, Read, Write};

pub const MAGIC: [u8; 4] = *b"ARX\0";
pub const REC_SYNC: [u8; 4] = *b"ARXR";
pub const FOOTER_MAGIC: [u8; 4] = *b"ARXE";
pub const HEADER_LEN: usize = 32;
pub const REC_HEADER_LEN: usize = 32;
pub const FOOTER_LEN: usize = 40;
pub const VERSION_MAJOR: u16 = 1;
pub const VERSION_MINOR: u16 = 0;
pub const HASH_BLAKE3: u8 = 1;

/// Largest meta block and payload a reader will accept, so damaged or hostile lengths cannot make
/// it allocate without bound.
pub const MAX_META: u32 = 1 << 30;
pub const MAX_PAYLOAD: u64 = 1 << 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    FileBegin = 1,
    Chunk = 2,
    FileEnd = 3,
    Dir = 4,
    Symlink = 5,
    Index = 6,
}

impl Kind {
    pub fn from_u8(b: u8) -> Option<Kind> {
        Some(match b {
            1 => Kind::FileBegin,
            2 => Kind::Chunk,
            3 => Kind::FileEnd,
            4 => Kind::Dir,
            5 => Kind::Symlink,
            6 => Kind::Index,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    None = 0,
    Lz4 = 1,
    Deflate = 2,
}

impl Codec {
    pub fn from_u8(b: u8) -> Option<Codec> {
        Some(match b {
            0 => Codec::None,
            1 => Codec::Lz4,
            2 => Codec::Deflate,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Codec::None => "none",
            Codec::Lz4 => "lz4",
            Codec::Deflate => "deflate",
        }
    }
}

pub fn crc32(parts: &[&[u8]]) -> u32 {
    let mut h = crc32fast::Hasher::new();
    for p in parts {
        h.update(p);
    }
    h.finalize()
}

#[derive(Debug)]
pub enum FormatError {
    Io(io::Error),
    /// Not an ARX archive, or damaged at the point named.
    Bad(String),
    /// A newer format than this build understands.
    Unsupported(String),
}

impl std::fmt::Display for FormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FormatError::Io(e) => write!(f, "{e}"),
            FormatError::Bad(m) => write!(f, "{m}"),
            FormatError::Unsupported(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for FormatError {}

impl From<io::Error> for FormatError {
    fn from(e: io::Error) -> Self {
        FormatError::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, FormatError>;

pub fn bad<T>(msg: impl Into<String>) -> Result<T> {
    Err(FormatError::Bad(msg.into()))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub major: u16,
    pub minor: u16,
    pub default_codec: Codec,
    pub flags: u16,
    pub chunk_size: u32,
    pub created: u64,
}

impl Header {
    pub fn new(default_codec: Codec, chunk_size: u32, created: u64) -> Header {
        Header { major: VERSION_MAJOR, minor: VERSION_MINOR, default_codec, flags: 0, chunk_size, created }
    }

    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut b = [0u8; HEADER_LEN];
        b[0..4].copy_from_slice(&MAGIC);
        b[4..6].copy_from_slice(&self.major.to_le_bytes());
        b[6..8].copy_from_slice(&self.minor.to_le_bytes());
        b[8] = self.default_codec as u8;
        b[9] = HASH_BLAKE3;
        b[10..12].copy_from_slice(&self.flags.to_le_bytes());
        b[12..16].copy_from_slice(&self.chunk_size.to_le_bytes());
        b[16..24].copy_from_slice(&self.created.to_le_bytes());
        // 24..28 reserved, zero
        let crc = crc32(&[&b[..28]]);
        b[28..32].copy_from_slice(&crc.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8; HEADER_LEN]) -> Result<Header> {
        if b[0..4] != MAGIC {
            return bad("not an ARX archive (wrong magic)");
        }
        let want = u32::from_le_bytes(b[28..32].try_into().unwrap());
        if crc32(&[&b[..28]]) != want {
            return bad("the archive header is damaged (checksum mismatch)");
        }
        let major = u16::from_le_bytes([b[4], b[5]]);
        let minor = u16::from_le_bytes([b[6], b[7]]);
        if major > VERSION_MAJOR {
            return Err(FormatError::Unsupported(format!("archive format version {major}.{minor} is newer than this build supports ({VERSION_MAJOR}.x)")));
        }
        if b[9] != HASH_BLAKE3 {
            return Err(FormatError::Unsupported(format!("unknown hash algorithm {}", b[9])));
        }
        let default_codec = Codec::from_u8(b[8]).ok_or_else(|| FormatError::Unsupported(format!("unknown codec {}", b[8])))?;
        let chunk_size = u32::from_le_bytes(b[12..16].try_into().unwrap());
        if chunk_size == 0 {
            return bad("the archive header has a zero chunk size");
        }
        Ok(Header { major, minor, default_codec, flags: u16::from_le_bytes([b[10], b[11]]), chunk_size, created: u64::from_le_bytes(b[16..24].try_into().unwrap()) })
    }

    pub fn read(r: &mut impl Read) -> Result<Header> {
        let mut b = [0u8; HEADER_LEN];
        r.read_exact(&mut b).map_err(|e| if e.kind() == io::ErrorKind::UnexpectedEof { FormatError::Bad("the file is too short to be an ARX archive".into()) } else { e.into() })?;
        Header::decode(&b)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecHeader {
    pub kind: Kind,
    pub codec: u8,
    pub meta_len: u32,
    pub data_len: u64,
    pub raw_len: u64,
}

impl RecHeader {
    pub fn encode(&self) -> [u8; REC_HEADER_LEN] {
        let mut b = [0u8; REC_HEADER_LEN];
        b[0..4].copy_from_slice(&REC_SYNC);
        b[4] = self.kind as u8;
        b[5] = self.codec;
        // 6..8 reserved
        b[8..12].copy_from_slice(&self.meta_len.to_le_bytes());
        b[12..20].copy_from_slice(&self.data_len.to_le_bytes());
        b[20..28].copy_from_slice(&self.raw_len.to_le_bytes());
        let crc = crc32(&[&b[..28]]);
        b[28..32].copy_from_slice(&crc.to_le_bytes());
        b
    }

    /// Checks the marker and the header checksum, then the lengths.
    pub fn decode(b: &[u8; REC_HEADER_LEN]) -> Result<RecHeader> {
        if b[0..4] != REC_SYNC {
            return bad("record marker not found");
        }
        let want = u32::from_le_bytes(b[28..32].try_into().unwrap());
        if crc32(&[&b[..28]]) != want {
            return bad("record header checksum mismatch");
        }
        let kind = Kind::from_u8(b[4]).ok_or_else(|| FormatError::Bad(format!("unknown record kind {}", b[4])))?;
        let meta_len = u32::from_le_bytes(b[8..12].try_into().unwrap());
        let data_len = u64::from_le_bytes(b[12..20].try_into().unwrap());
        if meta_len > MAX_META || data_len > MAX_PAYLOAD {
            return bad("record lengths are out of range");
        }
        Ok(RecHeader { kind, codec: b[5], meta_len, data_len, raw_len: u64::from_le_bytes(b[20..28].try_into().unwrap()) })
    }
}

/// Everything of a record except its payload, ready to be written: header, meta, meta checksum.
pub fn encode_record_head(kind: Kind, codec: u8, meta: &[u8], data_len: u64, raw_len: u64) -> Vec<u8> {
    let h = RecHeader { kind, codec, meta_len: meta.len() as u32, data_len, raw_len };
    let mut out = Vec::with_capacity(REC_HEADER_LEN + meta.len() + 4);
    out.extend_from_slice(&h.encode());
    out.extend_from_slice(meta);
    out.extend_from_slice(&crc32(&[meta]).to_le_bytes());
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Footer {
    pub index_offset: u64,
    pub index_len: u64,
    pub entries: u64,
    pub raw_total: u64,
}

impl Footer {
    pub fn encode(&self) -> [u8; FOOTER_LEN] {
        let mut b = [0u8; FOOTER_LEN];
        b[0..4].copy_from_slice(&FOOTER_MAGIC);
        b[4..12].copy_from_slice(&self.index_offset.to_le_bytes());
        b[12..20].copy_from_slice(&self.index_len.to_le_bytes());
        b[20..28].copy_from_slice(&self.entries.to_le_bytes());
        b[28..36].copy_from_slice(&self.raw_total.to_le_bytes());
        let crc = crc32(&[&b[..36]]);
        b[36..40].copy_from_slice(&crc.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8; FOOTER_LEN]) -> Result<Footer> {
        if b[0..4] != FOOTER_MAGIC {
            return bad("the end-of-archive footer is missing (the archive is truncated or was not finished)");
        }
        if crc32(&[&b[..36]]) != u32::from_le_bytes(b[36..40].try_into().unwrap()) {
            return bad("the end-of-archive footer is damaged (checksum mismatch)");
        }
        Ok(Footer {
            index_offset: u64::from_le_bytes(b[4..12].try_into().unwrap()),
            index_len: u64::from_le_bytes(b[12..20].try_into().unwrap()),
            entries: u64::from_le_bytes(b[20..28].try_into().unwrap()),
            raw_total: u64::from_le_bytes(b[28..36].try_into().unwrap()),
        })
    }
}

/// A `Write` that counts the bytes that went through it, so records know their offsets.
pub struct CountingWriter<W: Write> {
    inner: W,
    pub pos: u64,
}

impl<W: Write> CountingWriter<W> {
    pub fn new(inner: W) -> Self {
        CountingWriter { inner, pos: 0 }
    }

    pub fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.pos += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trip_and_checks() {
        let h = Header::new(Codec::Lz4, 1 << 20, 1_700_000_000);
        let b = h.encode();
        assert_eq!(Header::decode(&b).unwrap(), h);
        // any single flipped bit is detected
        for i in 0..HEADER_LEN * 8 {
            let mut c = b;
            c[i / 8] ^= 1 << (i % 8);
            assert!(Header::decode(&c).is_err(), "bit {i} flipped went unnoticed");
        }
    }

    #[test]
    fn a_newer_major_version_is_refused_with_a_clear_message() {
        let mut h = Header::new(Codec::None, 1024, 0);
        h.major = 2;
        let e = Header::decode(&h.encode()).unwrap_err();
        assert!(matches!(e, FormatError::Unsupported(_)), "{e}");
        assert!(e.to_string().contains("newer"));
    }

    #[test]
    fn record_header_round_trip_and_checks() {
        let h = RecHeader { kind: Kind::Chunk, codec: 2, meta_len: 24, data_len: 1000, raw_len: 4096 };
        let b = h.encode();
        assert_eq!(RecHeader::decode(&b).unwrap(), h);
        for i in 0..REC_HEADER_LEN * 8 {
            let mut c = b;
            c[i / 8] ^= 1 << (i % 8);
            assert!(RecHeader::decode(&c).is_err(), "bit {i} flipped went unnoticed");
        }
    }

    #[test]
    fn footer_round_trip_and_checks() {
        let f = Footer { index_offset: 123, index_len: 456, entries: 7, raw_total: 99 };
        let b = f.encode();
        assert_eq!(Footer::decode(&b).unwrap(), f);
        for i in 0..FOOTER_LEN * 8 {
            let mut c = b;
            c[i / 8] ^= 1 << (i % 8);
            assert!(Footer::decode(&c).is_err(), "bit {i} flipped went unnoticed");
        }
    }

    #[test]
    fn hostile_lengths_are_rejected_before_allocation() {
        let h = RecHeader { kind: Kind::Chunk, codec: 0, meta_len: u32::MAX, data_len: 0, raw_len: 0 };
        assert!(RecHeader::decode(&h.encode()).is_err());
        let h = RecHeader { kind: Kind::Chunk, codec: 0, meta_len: 0, data_len: u64::MAX, raw_len: 0 };
        assert!(RecHeader::decode(&h.encode()).is_err());
    }
}
