//! Reading records from a byte stream, with detection of damage and a way to find the next
//! intact record after it.

use std::io::{self, Read};

use crate::format::*;

/// A record read from the stream, its checksums not yet applied to the payload.
#[derive(Debug)]
pub struct Record {
    pub offset: u64,
    pub head: RecHeader,
    pub meta: Vec<u8>,
    /// Stored bytes of a chunk (empty for other records).
    pub payload: Vec<u8>,
    /// BLAKE3 of the uncompressed chunk and CRC-32 of the stored bytes (chunks only).
    pub digest: [u8; 32],
    pub payload_crc: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DamageKind {
    /// The stream ended in the middle of a record.
    Truncated,
    /// No valid record starts here; the framing is lost until `resync` finds the next one.
    Header,
    /// The record's meta data failed its checksum; its extent is known, so reading continues after it.
    Meta,
    /// Bytes follow the footer.
    Trailing,
    /// The footer is damaged.
    Footer,
}

#[derive(Debug)]
pub enum Step {
    Eof,
    Record(Record),
    Footer(Footer),
    Damage { offset: u64, kind: DamageKind, msg: String },
}

const INITIAL: usize = 256 * 1024;

pub struct Input<R: Read> {
    r: R,
    buf: Vec<u8>,
    pos: usize,
    end: usize,
    eof: bool,
    /// Offset in the archive of `buf[0]`.
    base: u64,
    after_footer: bool,
}

impl<R: Read> Input<R> {
    /// `offset` is where in the archive the first byte read from `r` lives.
    pub fn new(r: R, offset: u64) -> Self {
        Input { r, buf: vec![0; INITIAL], pos: 0, end: 0, eof: false, base: offset, after_footer: false }
    }

    pub fn offset(&self) -> u64 {
        self.base + self.pos as u64
    }

    fn available(&self) -> &[u8] {
        &self.buf[self.pos..self.end]
    }

    fn consume(&mut self, n: usize) {
        self.pos += n;
    }

    /// Makes at least `want` bytes available unless the stream ends first.
    fn fill(&mut self, want: usize) -> io::Result<()> {
        while self.end - self.pos < want && !self.eof {
            if self.pos > 0 {
                self.buf.copy_within(self.pos..self.end, 0);
                self.base += self.pos as u64;
                self.end -= self.pos;
                self.pos = 0;
            }
            if self.buf.len() < want.max(1) {
                self.buf.resize(want, 0);
            }
            if self.end == self.buf.len() {
                let grow = self.buf.len();
                self.buf.resize(grow * 2, 0);
            }
            let n = self.r.read(&mut self.buf[self.end..])?;
            if n == 0 {
                self.eof = true;
            } else {
                self.end += n;
            }
        }
        Ok(())
    }

    /// Reads exactly `n` bytes, or returns `None` if the stream ends first. The vector grows as
    /// data arrives, so a huge length in damaged data does not allocate that much up front.
    fn read_vec(&mut self, n: u64) -> io::Result<Option<Vec<u8>>> {
        let mut out = Vec::with_capacity(n.min(1 << 20) as usize);
        let from_buf = (self.end - self.pos).min(n as usize);
        out.extend_from_slice(&self.buf[self.pos..self.pos + from_buf]);
        self.pos += from_buf;
        let rest = n - from_buf as u64;
        if rest > 0 {
            let got = (&mut self.r).take(rest).read_to_end(&mut out)? as u64;
            self.base += self.pos as u64 + got;
            self.pos = 0;
            self.end = 0;
            if got < rest {
                self.eof = true;
                return Ok(None);
            }
        }
        Ok(Some(out))
    }

    /// Throws away `n` bytes.
    fn skip(&mut self, n: u64) -> io::Result<bool> {
        Ok(self.read_vec(n)?.is_some())
    }

    /// Reads the next record, footer or sign of damage.
    pub fn read_step(&mut self) -> io::Result<Step> {
        self.fill(FOOTER_LEN)?;
        let offset = self.offset();
        let avail = self.available();
        if avail.is_empty() {
            return Ok(Step::Eof);
        }
        if self.after_footer {
            let n = avail.len();
            self.consume(n);
            return Ok(Step::Damage { offset, kind: DamageKind::Trailing, msg: format!("{n}+ bytes follow the footer") });
        }
        if avail.starts_with(&FOOTER_MAGIC) {
            if avail.len() < FOOTER_LEN {
                self.consume(avail.len());
                return Ok(Step::Damage { offset, kind: DamageKind::Truncated, msg: "the footer is cut short".into() });
            }
            let f: [u8; FOOTER_LEN] = avail[..FOOTER_LEN].try_into().unwrap();
            return Ok(match Footer::decode(&f) {
                Ok(footer) => {
                    self.consume(FOOTER_LEN);
                    self.after_footer = true;
                    Step::Footer(footer)
                }
                Err(e) => {
                    self.consume(FOOTER_LEN);
                    self.after_footer = true;
                    Step::Damage { offset, kind: DamageKind::Footer, msg: e.to_string() }
                }
            });
        }
        if avail.len() < REC_HEADER_LEN {
            let n = avail.len();
            self.consume(n);
            return Ok(Step::Damage { offset, kind: DamageKind::Truncated, msg: format!("the archive ends with {n} stray bytes, less than a record header") });
        }
        let hb: [u8; REC_HEADER_LEN] = avail[..REC_HEADER_LEN].try_into().unwrap();
        let head = match RecHeader::decode(&hb) {
            Ok(h) => h,
            Err(e) => return Ok(Step::Damage { offset, kind: DamageKind::Header, msg: e.to_string() }),
        };
        self.consume(REC_HEADER_LEN);
        let truncated = |what: &str| Step::Damage { offset, kind: DamageKind::Truncated, msg: format!("the archive ends inside a record's {what}") };
        let Some(mut meta) = self.read_vec(head.meta_len as u64 + 4)? else { return Ok(truncated("meta data")) };
        let crc_bytes = meta.split_off(head.meta_len as usize);
        let has_payload = head.kind == Kind::Chunk;
        let trailer_len = if has_payload { 36 } else { 0 };
        let body = head.data_len + trailer_len;
        if crc32(&[&meta]) != u32::from_le_bytes(crc_bytes[..4].try_into().unwrap()) {
            // The header was intact, so the extent is known: step over the payload and carry on.
            if !self.skip(body)? {
                return Ok(truncated("payload"));
            }
            return Ok(Step::Damage { offset, kind: DamageKind::Meta, msg: format!("a {:?} record's meta data failed its checksum", head.kind) });
        }
        if !has_payload {
            if head.data_len > 0 && !self.skip(head.data_len)? {
                return Ok(truncated("payload"));
            }
            return Ok(Step::Record(Record { offset, head, meta, payload: Vec::new(), digest: [0; 32], payload_crc: 0 }));
        }
        let Some(mut payload) = self.read_vec(body)? else { return Ok(truncated("payload")) };
        let tail = payload.split_off(head.data_len as usize);
        let digest: [u8; 32] = tail[..32].try_into().unwrap();
        let payload_crc = u32::from_le_bytes(tail[32..36].try_into().unwrap());
        Ok(Step::Record(Record { offset, head, meta, payload, digest, payload_crc }))
    }

    /// After a `Header` damage: moves to the next position where a valid record header or footer
    /// starts. Returns how many bytes were skipped, or `None` if the stream ends first.
    pub fn resync(&mut self) -> io::Result<Option<u64>> {
        let mut skipped = 0u64;
        // The byte where reading failed is never a candidate again.
        let mut from = 1usize;
        loop {
            self.fill(FOOTER_LEN)?;
            let eof = self.eof;
            let avail = self.available();
            if avail.is_empty() {
                return Ok(None);
            }
            let mut i = from;
            let mut found = None;
            while i < avail.len() {
                if avail[i] == b'A' {
                    let rest = &avail[i..];
                    // A candidate can only be judged with a whole header or footer after it, unless
                    // the stream has ended (then a short remainder is simply not valid).
                    if rest.len() < FOOTER_LEN && !eof {
                        break;
                    }
                    if rest.starts_with(&REC_SYNC) && rest.len() >= REC_HEADER_LEN {
                        let hb: [u8; REC_HEADER_LEN] = rest[..REC_HEADER_LEN].try_into().unwrap();
                        if RecHeader::decode(&hb).is_ok() {
                            found = Some(i);
                            break;
                        }
                    }
                    if rest.starts_with(&FOOTER_MAGIC) && rest.len() >= FOOTER_LEN {
                        let fb: [u8; FOOTER_LEN] = rest[..FOOTER_LEN].try_into().unwrap();
                        if Footer::decode(&fb).is_ok() {
                            found = Some(i);
                            break;
                        }
                    }
                }
                i += 1;
            }
            if let Some(i) = found {
                self.consume(i);
                return Ok(Some(skipped + i as u64));
            }
            // Nothing valid before `i`; keep the undecided tail (if any) for the next round.
            let stop = i.min(avail.len());
            self.consume(stop);
            skipped += stop as u64;
            from = 0;
            if stop == 0 && !eof {
                // No progress is possible without more data and the buffer cannot grow further.
                self.fill(self.end - self.pos + 1)?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn rec(kind: Kind, meta: &[u8]) -> Vec<u8> {
        encode_record_head(kind, 0, meta, 0, 0)
    }

    fn collect(bytes: &[u8]) -> Vec<String> {
        let mut inp = Input::new(Cursor::new(bytes.to_vec()), 0);
        let mut out = Vec::new();
        loop {
            match inp.read_step().unwrap() {
                Step::Eof => break,
                Step::Record(r) => out.push(format!("rec {:?}@{} meta={}", r.head.kind, r.offset, String::from_utf8_lossy(&r.meta))),
                Step::Footer(_) => out.push("footer".into()),
                Step::Damage { kind, offset, .. } => {
                    out.push(format!("damage {kind:?}@{offset}"));
                    if kind == DamageKind::Header {
                        match inp.resync().unwrap() {
                            Some(n) => out.push(format!("resync +{n}")),
                            None => break,
                        }
                    }
                }
            }
        }
        out
    }

    #[test]
    fn reads_records_and_the_footer() {
        let mut s = rec(Kind::Dir, b"one");
        s.extend(rec(Kind::Dir, b"two"));
        s.extend(Footer { index_offset: 0, index_len: 0, entries: 0, raw_total: 0 }.encode());
        let got = collect(&s);
        assert_eq!(got.len(), 3, "{got:?}");
        assert!(got[0].starts_with("rec Dir@0") && got[1].contains("two") && got[2] == "footer");
    }

    #[test]
    fn garbage_between_records_is_skipped_by_resync() {
        let a = rec(Kind::Dir, b"first");
        let b = rec(Kind::Dir, b"second");
        let mut s = a.clone();
        s.extend(std::iter::repeat_n(0xAB, 1000));
        s.extend(b"ARXR half a marker"); // looks like a marker, is not a record
        s.extend(&b);
        let got = collect(&s);
        assert!(got.iter().any(|g| g.contains("first")), "{got:?}");
        assert!(got.iter().any(|g| g.starts_with("damage Header")), "{got:?}");
        assert!(got.iter().any(|g| g.contains("second")), "the record after the garbage is found: {got:?}");
    }

    #[test]
    fn a_damaged_meta_block_is_skipped_without_losing_the_framing() {
        let mut a = rec(Kind::Dir, b"first");
        let n = a.len();
        a[REC_HEADER_LEN] ^= 1; // flip a bit in the meta
        let mut s = a;
        s.extend(rec(Kind::Dir, b"second"));
        let got = collect(&s);
        assert_eq!(got[0], "damage Meta@0", "{got:?}");
        assert!(got[1].contains("second") && got[1].contains(&format!("@{n}")), "{got:?}");
    }

    #[test]
    fn truncation_is_reported() {
        let s = rec(Kind::Dir, b"first");
        for cut in 1..s.len() {
            let got = collect(&s[..cut]);
            assert!(got.iter().any(|g| g.starts_with("damage Truncated")), "cut {cut}: {got:?}");
        }
    }

    #[test]
    fn data_after_the_footer_is_reported() {
        let mut s = Footer { index_offset: 0, index_len: 0, entries: 0, raw_total: 0 }.encode().to_vec();
        s.extend(b"extra");
        let got = collect(&s);
        assert_eq!(got[0], "footer");
        assert!(got[1].starts_with("damage Trailing"), "{got:?}");
    }

    #[test]
    #[allow(clippy::collapsible_match)] // resync has a side effect, so it stays out of a match guard
    fn works_when_the_reader_returns_one_byte_at_a_time() {
        struct Slow(Cursor<Vec<u8>>);
        impl Read for Slow {
            fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
                let n = b.len().min(1);
                self.0.read(&mut b[..n])
            }
        }
        let mut s = rec(Kind::Dir, b"one");
        s.extend(vec![0u8; 77]);
        s.extend(rec(Kind::Dir, b"two"));
        let mut inp = Input::new(Slow(Cursor::new(s)), 0);
        let mut names = Vec::new();
        loop {
            match inp.read_step().unwrap() {
                Step::Eof => break,
                Step::Record(r) => names.push(String::from_utf8(r.meta).unwrap()),
                Step::Damage { kind: DamageKind::Header, .. } => {
                    if inp.resync().unwrap().is_none() {
                        break;
                    }
                }
                _ => {}
            }
        }
        assert_eq!(names, ["one", "two"]);
    }
}
