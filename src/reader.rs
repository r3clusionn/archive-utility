//! Reading archives: verifying, listing and extracting, from a stream or with random access.
//!
//! A [`Walker`] consumes the records of a stream in order, checks everything that can be checked
//! (every checksum, the order and completeness of chunks, sizes, the per-file digest) and hands the
//! verified bytes of each file to a [`Sink`]. Chunks are decompressed in parallel; they are applied
//! in order. The same walker serves extraction (a sink that writes files), verification (a sink that
//! discards) and `cat` (a sink that writes to a stream).

use std::collections::{HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use workpool::{JoinHandle, ThreadPool};

use crate::codec::decode_chunk;
use crate::entry::*;
use crate::format::*;
use crate::input::{DamageKind, Input, Record, Step};
use crate::pathsafe::{join_under, sanitize};

/// Something wrong with the archive or with what was asked of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    /// Byte offset in the archive where it was noticed.
    pub offset: u64,
    pub path: Option<String>,
    pub message: String,
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.path {
            Some(p) => write!(f, "{p}: {} (at archive offset {})", self.message, self.offset),
            None => write!(f, "{} (at archive offset {})", self.message, self.offset),
        }
    }
}

#[derive(Default, Debug)]
pub struct Report {
    pub files_ok: u64,
    pub files_bad: u64,
    pub dirs: u64,
    pub symlinks: u64,
    /// Uncompressed bytes of the files that verified.
    pub bytes: u64,
    pub problems: Vec<Problem>,
    /// The entries as found by reading every record.
    pub scanned: Vec<Entry>,
    /// The entries of the index record, if one was read.
    pub index: Option<Vec<Entry>>,
    pub footer: Option<Footer>,
    pub header: Option<Header>,
    /// Bytes skipped while looking for the next intact record after damage.
    pub skipped_bytes: u64,
}

impl Report {
    pub fn ok(&self) -> bool {
        self.problems.is_empty()
    }
}

/// Where verified file contents go.
pub trait Sink {
    /// A file starts. `Ok(false)` skips it (it is not wanted); `Err` skips it and reports why.
    fn begin_file(&mut self, e: &Entry) -> std::result::Result<bool, String>;
    fn write(&mut self, data: &[u8]) -> std::result::Result<(), String>;
    /// The file ended; `ok` says whether everything about it verified.
    fn end_file(&mut self, e: &Entry, ok: bool) -> std::result::Result<(), String>;
    /// A directory entry. `Ok(false)` means it was not wanted.
    fn dir(&mut self, e: &Entry) -> std::result::Result<(), String>;
    fn symlink(&mut self, e: &Entry) -> std::result::Result<(), String>;
    /// The walk is over: wait for pending work and report what went wrong while it ran.
    fn finish(&mut self) -> Vec<String> {
        Vec::new()
    }
}

/// Discards everything: used to verify.
pub struct NullSink;

impl Sink for NullSink {
    fn begin_file(&mut self, _: &Entry) -> std::result::Result<bool, String> {
        Ok(true)
    }
    fn write(&mut self, _: &[u8]) -> std::result::Result<(), String> {
        Ok(())
    }
    fn end_file(&mut self, _: &Entry, _: bool) -> std::result::Result<(), String> {
        Ok(())
    }
    fn dir(&mut self, _: &Entry) -> std::result::Result<(), String> {
        Ok(())
    }
    fn symlink(&mut self, _: &Entry) -> std::result::Result<(), String> {
        Ok(())
    }
}

struct Pending {
    index: u64,
    offset: u64,
    digest: [u8; 32],
    stored: u64,
    job: JoinHandle<std::result::Result<Vec<u8>, String>>,
}

struct Current {
    entry: Entry,
    /// The sink wants the data.
    wanted: bool,
    next_index: u64,
    next_offset: u64,
    stored: u64,
    chain: blake3::Hasher,
    bad: bool,
    begin_offset: u64,
}

pub struct Walker<'a, S: Sink> {
    pool: &'a ThreadPool,
    sink: &'a mut S,
    window: usize,
    max_raw: u64,
    cur: Option<Current>,
    pending: VecDeque<Pending>,
    pub report: Report,
}

impl<'a, S: Sink> Walker<'a, S> {
    pub fn new(pool: &'a ThreadPool, sink: &'a mut S, header: &Header) -> Self {
        let report = Report { header: Some(header.clone()), ..Report::default() };
        Walker { pool, sink, window: pool.threads() * 2 + 2, max_raw: header.chunk_size as u64, cur: None, pending: VecDeque::new(), report }
    }

    fn problem(&mut self, offset: u64, path: Option<&str>, msg: impl Into<String>) {
        self.report.problems.push(Problem { offset, path: path.map(str::to_string), message: msg.into() });
    }

    fn mark_bad(&mut self, offset: u64, msg: impl Into<String>) {
        let path = self.cur.as_ref().map(|c| c.entry.path.clone());
        if let Some(c) = &mut self.cur {
            c.bad = true;
        }
        self.problem(offset, path.as_deref(), msg);
    }

    /// Applies finished chunks from the front of the queue until at most `keep` remain.
    fn flush(&mut self, keep: usize) {
        while self.pending.len() > keep {
            let p = self.pending.pop_front().expect("non-empty");
            let result = p.job.join();
            let Some(cur) = self.cur.as_mut() else { continue };
            match result {
                Ok(Ok(raw)) => {
                    cur.chain.update(&p.digest);
                    cur.stored += p.stored;
                    let wanted = cur.wanted && !cur.bad;
                    let (path, begin) = (cur.entry.path.clone(), cur.begin_offset);
                    if wanted {
                        if let Err(e) = self.sink.write(&raw) {
                            self.mark_bad(begin, format!("cannot write: {e}"));
                        }
                    }
                    let _ = (path, p.index, p.offset);
                    self.report.bytes += raw.len() as u64;
                }
                Ok(Err(msg)) => {
                    let begin = cur.begin_offset;
                    self.mark_bad(begin, format!("chunk {}: {msg}", p.index));
                }
                Err(e) => {
                    let begin = cur.begin_offset;
                    self.mark_bad(begin, format!("chunk {}: decoding failed: {e}", p.index));
                }
            }
        }
    }

    fn close_file(&mut self, end: Option<(EndMeta, u64)>) {
        self.flush(0);
        let Some(cur) = self.cur.take() else { return };
        let mut e = cur.entry.clone();
        let mut bad = cur.bad;
        let begin = cur.begin_offset;
        let mut messages: Vec<String> = Vec::new();
        match end {
            Some((end, _)) => {
                let digest = *cur.chain.finalize().as_bytes();
                if end.file_id != e.id {
                    messages.push("the end record belongs to another file".into());
                }
                if end.chunks != cur.next_index {
                    messages.push(format!("{} chunks were read but the file claims {}", cur.next_index, end.chunks));
                }
                if end.size != cur.next_offset {
                    messages.push(format!("{} bytes were read but the file claims {}", cur.next_offset, end.size));
                }
                if e.size != cur.next_offset {
                    messages.push(format!("the file changed size while it was archived ({} declared, {} stored)", e.size, cur.next_offset));
                }
                if end.digest != digest {
                    messages.push("the file's digest does not match its chunks".into());
                }
                if end.stored != cur.stored {
                    messages.push("the stored size does not match".into());
                }
                e.chunks = end.chunks;
                e.stored = end.stored;
                e.digest = end.digest;
                e.size = end.size;
            }
            None => messages.push("the file is incomplete: the archive has no end record for it".into()),
        }
        if !messages.is_empty() {
            bad = true;
            for m in messages {
                self.problem(begin, Some(&e.path), m);
            }
        }
        if cur.wanted {
            if let Err(m) = self.sink.end_file(&e, !bad) {
                bad = true;
                self.problem(begin, Some(&e.path), format!("cannot finish: {m}"));
            }
        }
        e.offset = begin;
        if bad {
            self.report.files_bad += 1;
        } else {
            self.report.files_ok += 1;
        }
        self.report.scanned.push(e);
    }

    /// Takes one step of the stream. Returns false when the stream is finished.
    pub fn step(&mut self, step: Step) -> bool {
        match step {
            Step::Eof => {
                if self.cur.is_some() {
                    let off = self.cur.as_ref().map_or(0, |c| c.begin_offset);
                    let _ = off;
                    self.close_file(None);
                }
                return false;
            }
            Step::Footer(f) => {
                self.close_file_if_open();
                self.report.footer = Some(f);
            }
            Step::Damage { offset, kind, msg } => {
                self.problem(offset, self.cur.as_ref().map(|c| c.entry.path.clone()).as_deref(), msg);
                if kind == DamageKind::Meta {
                    // A record is lost: whatever file is open can no longer be complete.
                    if let Some(c) = &mut self.cur {
                        c.bad = true;
                    }
                }
            }
            Step::Record(rec) => self.record(rec),
        }
        true
    }

    fn close_file_if_open(&mut self) {
        if self.cur.is_some() {
            self.close_file(None);
        }
    }

    fn record(&mut self, rec: Record) {
        let off = rec.offset;
        match rec.head.kind {
            Kind::FileBegin => {
                self.close_file_if_open();
                let entry = match decode_begin(&rec.meta) {
                    Ok(e) => e,
                    Err(e) => return self.problem(off, None, format!("bad file record: {e}")),
                };
                let (wanted, bad) = match self.sink.begin_file(&entry) {
                    Ok(w) => (w, false),
                    Err(m) => {
                        self.problem(off, Some(&entry.path), m);
                        (false, true)
                    }
                };
                self.cur = Some(Current { entry, wanted, next_index: 0, next_offset: 0, stored: 0, chain: blake3::Hasher::new(), bad, begin_offset: off });
            }
            Kind::Chunk => {
                let meta = match decode_chunk_meta(&rec.meta) {
                    Ok(m) => m,
                    Err(e) => return self.mark_bad_or_orphan(off, format!("bad chunk record: {e}")),
                };
                let Some(cur) = self.cur.as_mut() else {
                    return self.problem(off, None, "a chunk appears outside any file");
                };
                if meta.file_id != cur.entry.id {
                    return self.mark_bad(off, "a chunk belongs to a different file than the one being read");
                }
                if meta.index != cur.next_index || meta.offset != cur.next_offset {
                    let want = cur.next_index;
                    cur.bad = true;
                    // Take the chunk's position as given so one gap is reported once, not for every chunk after it.
                    cur.next_index = meta.index;
                    cur.next_offset = meta.offset;
                    self.mark_bad(off, format!("chunk {} found where chunk {want} was expected: a chunk is missing or out of order", meta.index));
                }
                let cur = self.cur.as_mut().expect("still open");
                let (raw_len, stored) = (rec.head.raw_len, rec.head.data_len);
                cur.next_index += 1;
                cur.next_offset += raw_len;
                let (codec, digest, crc, max_raw) = (rec.head.codec, rec.digest, rec.payload_crc, self.max_raw);
                let payload = rec.payload;
                let job = self.pool.spawn(move || decode_chunk(codec, &payload, raw_len, max_raw, &digest, crc));
                self.pending.push_back(Pending { index: meta.index, offset: meta.offset, digest, stored, job });
                let window = self.window;
                self.flush(window);
            }
            Kind::FileEnd => {
                let end = match decode_end(&rec.meta) {
                    Ok(e) => e,
                    Err(e) => return self.mark_bad_or_orphan(off, format!("bad end record: {e}")),
                };
                if self.cur.is_none() {
                    return self.problem(off, None, "an end record appears outside any file");
                }
                self.close_file(Some((end, off)));
            }
            Kind::Dir => {
                self.close_file_if_open();
                match decode_dir(&rec.meta) {
                    Ok(mut e) => {
                        e.offset = off;
                        if let Err(m) = self.sink.dir(&e) {
                            self.problem(off, Some(&e.path), m);
                        }
                        self.report.dirs += 1;
                        self.report.scanned.push(e);
                    }
                    Err(e) => self.problem(off, None, format!("bad directory record: {e}")),
                }
            }
            Kind::Symlink => {
                self.close_file_if_open();
                match decode_symlink(&rec.meta) {
                    Ok(mut e) => {
                        e.offset = off;
                        if let Err(m) = self.sink.symlink(&e) {
                            self.problem(off, Some(&e.path), m);
                        }
                        self.report.symlinks += 1;
                        self.report.scanned.push(e);
                    }
                    Err(e) => self.problem(off, None, format!("bad symlink record: {e}")),
                }
            }
            Kind::Index => {
                self.close_file_if_open();
                match decode_index(&rec.meta) {
                    Ok(v) => self.report.index = Some(v),
                    Err(e) => self.problem(off, None, format!("the index is damaged: {e}")),
                }
            }
        }
    }

    fn mark_bad_or_orphan(&mut self, off: u64, msg: String) {
        if self.cur.is_some() {
            self.mark_bad(off, msg);
        } else {
            self.problem(off, None, msg);
        }
    }

    /// Finishes the walk: closes an open file, applies pending chunks, runs the sink's final step and
    /// compares what was read with the index.
    pub fn finish(&mut self) {
        self.close_file_if_open();
        for m in self.sink.finish() {
            self.problem(0, None, m);
        }
        self.cross_check();
    }

    fn cross_check(&mut self) {
        match (&self.report.index, self.report.footer) {
            (_, None) => self.problem(0, None, "the archive has no valid footer: it is truncated, unfinished or damaged"),
            (None, Some(_)) => self.problem(0, None, "the footer is present but no index record was read"),
            (Some(index), Some(f)) => {
                let index = index.clone();
                let scanned = std::mem::take(&mut self.report.scanned);
                if f.entries != index.len() as u64 {
                    self.problem(f.index_offset, None, format!("the footer says {} entries but the index holds {}", f.entries, index.len()));
                }
                if index.len() != scanned.len() {
                    self.problem(f.index_offset, None, format!("the index lists {} entries but {} were found in the archive", index.len(), scanned.len()));
                } else {
                    for (a, b) in index.iter().zip(&scanned) {
                        if a != b {
                            self.problem(b.offset, Some(&b.path), "the index does not match the archive's own records for this entry");
                        }
                    }
                }
                let raw: u64 = scanned.iter().filter(|e| e.kind == EntryKind::File).map(|e| e.size).sum();
                if f.raw_total != raw && self.report.problems.is_empty() {
                    self.problem(f.index_offset, None, format!("the footer says {} bytes of data but the files add up to {raw}", f.raw_total));
                }
                self.report.scanned = scanned;
            }
        }
    }
}

/// Reads every record of `input` through a walker feeding `sink`. With `recover`, damage that
/// loses the framing is skipped by searching for the next intact record; without it, reading stops there.
pub fn walk<R: Read, S: Sink>(input: &mut Input<R>, header: &Header, pool: &ThreadPool, sink: &mut S, recover: bool) -> io::Result<Report> {
    let mut w = Walker::new(pool, sink, header);
    loop {
        let step = input.read_step()?;
        let framing_lost = matches!(&step, Step::Damage { kind: DamageKind::Header, .. });
        let is_truncated = matches!(&step, Step::Damage { kind: DamageKind::Truncated, .. });
        if !w.step(step) {
            break;
        }
        if is_truncated {
            // Nothing can follow a cut-off record.
            continue;
        }
        if framing_lost {
            if !recover {
                w.problem(input.offset(), None, "stopped at damage that loses the position of the next record (run with --recover to search for it)");
                break;
            }
            match input.resync()? {
                Some(n) => w.report.skipped_bytes += n,
                None => break,
            }
        }
    }
    w.finish();
    Ok(w.report)
}

/// A seekable archive with a readable footer and index.
pub struct Archive<R: Read + Seek> {
    r: R,
    pub header: Header,
    pub footer: Footer,
    pub entries: Vec<Entry>,
    pool: ThreadPool,
}

fn make_pool(threads: usize) -> io::Result<ThreadPool> {
    let n = if threads == 0 { std::thread::available_parallelism().map_or(1, |n| n.get()) } else { threads };
    ThreadPool::builder().threads(n).name("arx-read").build()
}

impl<R: Read + Seek> Archive<R> {
    /// Opens an archive by its footer and index. Fails if either is missing or damaged; use
    /// [`scan`] to read such an archive from the front.
    pub fn open(mut r: R, threads: usize) -> Result<Archive<R>> {
        r.seek(SeekFrom::Start(0))?;
        let header = Header::read(&mut r)?;
        let len = r.seek(SeekFrom::End(0))?;
        if len < (HEADER_LEN + FOOTER_LEN) as u64 {
            return bad("the file is too short to contain an archive footer");
        }
        r.seek(SeekFrom::Start(len - FOOTER_LEN as u64))?;
        let mut fb = [0u8; FOOTER_LEN];
        r.read_exact(&mut fb)?;
        let footer = Footer::decode(&fb)?;
        let idx_end = footer.index_offset.checked_add(footer.index_len);
        if footer.index_offset < HEADER_LEN as u64 || idx_end != Some(len - FOOTER_LEN as u64) {
            return bad("the footer points outside the archive");
        }
        r.seek(SeekFrom::Start(footer.index_offset))?;
        let mut input = Input::new(&mut r, footer.index_offset);
        let rec = match input.read_step()? {
            Step::Record(rec) if rec.head.kind == Kind::Index => rec,
            Step::Damage { msg, .. } => return bad(format!("the index is damaged: {msg}")),
            _ => return bad("no index record where the footer says it is"),
        };
        let entries = decode_index(&rec.meta)?;
        if entries.len() as u64 != footer.entries {
            return bad("the index and the footer disagree about the number of entries");
        }
        let pool = make_pool(threads)?;
        Ok(Archive { r, header, footer, entries, pool })
    }

    /// Extracts the entries selected by `filter` (path prefixes; empty selects everything) using the
    /// index to jump to each file.
    pub fn extract_selected<S: Sink>(&mut self, sink: &mut S, filter: &[String]) -> io::Result<Report> {
        let wanted: Vec<Entry> = self.entries.iter().filter(|e| selected(&e.path, filter)).cloned().collect();
        let mut report = Report { header: Some(self.header.clone()), footer: Some(self.footer), index: Some(self.entries.clone()), ..Report::default() };
        let mut w = Walker::new(&self.pool, sink, &self.header);
        for e in &wanted {
            match e.kind {
                EntryKind::Dir => {
                    if let Err(m) = w.sink.dir(e) {
                        w.problem(e.offset, Some(&e.path), m);
                    }
                    w.report.dirs += 1;
                }
                EntryKind::Symlink => {
                    if let Err(m) = w.sink.symlink(e) {
                        w.problem(e.offset, Some(&e.path), m);
                    }
                    w.report.symlinks += 1;
                }
                EntryKind::File => {
                    self.r.seek(SeekFrom::Start(e.offset))?;
                    let mut input = Input::new(io::BufReader::with_capacity(1 << 16, &mut self.r), e.offset);
                    // Read this file's records: begin, chunks, end.
                    loop {
                        let step = input.read_step()?;
                        let done = matches!(&step, Step::Record(r) if r.head.kind == Kind::FileEnd);
                        let stop = matches!(&step, Step::Eof | Step::Footer(_) | Step::Damage { kind: DamageKind::Header | DamageKind::Truncated, .. });
                        w.step(step);
                        if done || stop {
                            break;
                        }
                    }
                    w.close_file_if_open();
                }
            }
        }
        w.finish_selected();
        report.files_ok = w.report.files_ok;
        report.files_bad = w.report.files_bad;
        report.dirs = w.report.dirs;
        report.symlinks = w.report.symlinks;
        report.bytes = w.report.bytes;
        report.problems = std::mem::take(&mut w.report.problems);
        report.scanned = std::mem::take(&mut w.report.scanned);
        Ok(report)
    }

    /// Verifies everything by reading the archive from the front.
    pub fn verify(&mut self, recover: bool) -> io::Result<Report> {
        self.r.seek(SeekFrom::Start(HEADER_LEN as u64))?;
        let mut input = Input::new(io::BufReader::with_capacity(1 << 20, &mut self.r), HEADER_LEN as u64);
        let mut sink = NullSink;
        walk(&mut input, &self.header, &self.pool, &mut sink, recover)
    }
}

impl<S: Sink> Walker<'_, S> {
    /// Like `finish` for a partial extraction: the index is not compared with the records read.
    fn finish_selected(&mut self) {
        self.close_file_if_open();
        for m in self.sink.finish() {
            self.problem(0, None, m);
        }
    }
}

/// Reads an archive from the front without needing the footer (a pipe, or a damaged or unfinished file).
pub fn scan<R: Read, S: Sink>(r: R, threads: usize, sink: &mut S, recover: bool) -> Result<Report> {
    let pool = make_pool(threads)?;
    scan_with(r, &pool, sink, recover)
}

/// Like [`scan`], on an existing thread pool.
pub fn scan_with<R: Read, S: Sink>(mut r: R, pool: &ThreadPool, sink: &mut S, recover: bool) -> Result<Report> {
    let header = Header::read(&mut r)?;
    let mut input = Input::new(io::BufReader::with_capacity(1 << 20, r), HEADER_LEN as u64);
    Ok(walk(&mut input, &header, pool, sink, recover)?)
}

/// True if `path` is one of the `filter` prefixes or inside one. An empty filter selects everything.
pub fn selected(path: &str, filter: &[String]) -> bool {
    filter.is_empty() || filter.iter().any(|f| {
        let f = f.trim_end_matches('/');
        path == f || (path.len() > f.len() && path.starts_with(f) && path.as_bytes()[f.len()] == b'/')
    })
}

/// What to do when an extracted file already exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Existing {
    /// Report an error and leave the file alone.
    Fail,
    Overwrite,
    Skip,
}

/// Opens a file or directory just far enough to change its times.
fn open_for_times(path: &Path) -> io::Result<File> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
        // Backup semantics is what lets Windows open a directory at all.
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        OpenOptions::new().access_mode(FILE_WRITE_ATTRIBUTES).custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(path)
    }
    #[cfg(not(windows))]
    {
        File::open(path)
    }
}

fn entry_time(e: &Entry) -> std::time::SystemTime {
    if e.mtime_secs >= 0 {
        UNIX_EPOCH + Duration::new(e.mtime_secs as u64, e.mtime_nanos)
    } else {
        UNIX_EPOCH - Duration::new(e.mtime_secs.unsigned_abs(), 0) + Duration::new(0, e.mtime_nanos)
    }
}

fn set_times(path: &Path, e: &Entry) {
    if let Ok(f) = open_for_times(path) {
        let _ = f.set_modified(entry_time(e));
    }
}

fn set_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o7777));
    }
    #[cfg(not(unix))]
    {
        if let Ok(meta) = fs::metadata(path) {
            let mut p = meta.permissions();
            p.set_readonly(mode & 0o200 == 0);
            let _ = fs::set_permissions(path, p);
        }
    }
}

/// Files up to this size are collected in memory and written by a pool thread, so the cost of
/// creating many small files (which is latency, not bandwidth) overlaps instead of adding up.
const SMALL_FILE: u64 = 4 << 20;
/// Small-file writes allowed to be outstanding before the reader waits for the oldest.
const MAX_QUEUED_WRITES: usize = 256;

/// Everything a worker needs to put one small file on disk.
struct SmallWrite {
    path: PathBuf,
    data: Vec<u8>,
    mode: u32,
    entry: Entry,
}

/// State shared between the sink and its write jobs.
struct Shared {
    existing: Existing,
    /// Directories known to exist, so each is created once however many files it holds.
    made: Mutex<HashSet<PathBuf>>,
    errors: Mutex<Vec<String>>,
}

impl Shared {
    fn ensure_dir(&self, dir: &Path) -> io::Result<()> {
        if self.made.lock().unwrap().contains(dir) {
            return Ok(());
        }
        fs::create_dir_all(dir)?;
        // create_dir_all made every missing ancestor too, so none of them needs another call.
        let mut made = self.made.lock().unwrap();
        for a in dir.ancestors() {
            if !made.insert(a.to_path_buf()) {
                break;
            }
        }
        Ok(())
    }

    /// Opens `path` for writing under the existing-file policy. `Ok(None)` means skip the file.
    fn open(&self, path: &Path) -> std::result::Result<Option<File>, String> {
        let create_new = OpenOptions::new().write(true).create_new(true).open(path);
        match create_new {
            Ok(f) => Ok(Some(f)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => match self.existing {
                Existing::Fail => Err("already exists (use --force to overwrite or --skip-existing)".to_string()),
                Existing::Skip => Ok(None),
                Existing::Overwrite => {
                    // A symbolic link in the way is replaced, never followed: opening it for writing
                    // would put the archive's data into whatever the link points at.
                    if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
                        fs::remove_file(path).map_err(|err| format!("cannot replace the symbolic link: {err}"))?;
                        return OpenOptions::new().write(true).create_new(true).open(path).map(Some).map_err(|err| format!("cannot create the file: {err}"));
                    }
                    // A read-only file would refuse to be replaced.
                    if let Ok(meta) = fs::metadata(path) {
                        let mut p = meta.permissions();
                        #[allow(clippy::permissions_set_readonly_false)]
                        p.set_readonly(false);
                        let _ = fs::set_permissions(path, p);
                    }
                    OpenOptions::new().write(true).truncate(true).open(path).map(Some).map_err(|err| format!("cannot overwrite the file: {err}"))
                }
            },
            Err(e) => Err(format!("cannot create the file: {e}")),
        }
    }

    fn write_small(&self, w: SmallWrite) {
        let fail = |m: String| self.errors.lock().unwrap().push(format!("{}: {m}", w.entry.path));
        if let Some(parent) = w.path.parent() {
            if let Err(e) = self.ensure_dir(parent) {
                return fail(format!("cannot create {}: {e}", parent.display()));
            }
        }
        match self.open(&w.path) {
            Ok(Some(mut f)) => {
                if let Err(e) = f.write_all(&w.data) {
                    drop(f);
                    let _ = fs::remove_file(&w.path);
                    return fail(format!("cannot write: {e}"));
                }
                finish_handle(&f, &w.entry, w.mode);
            }
            Ok(None) => {}
            Err(m) => fail(m),
        }
    }
}

/// Sets the time and permissions through the open handle: no second open, no second lookup.
fn finish_handle(f: &File, e: &Entry, mode: u32) {
    let _ = f.set_modified(entry_time(e));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = f.set_permissions(fs::Permissions::from_mode(mode & 0o7777));
    }
    #[cfg(not(unix))]
    {
        // Windows only has a read-only flag, and files are created writable, so most need no call.
        if mode & 0o200 == 0 {
            if let Ok(meta) = f.metadata() {
                let mut p = meta.permissions();
                p.set_readonly(true);
                let _ = f.set_permissions(p);
            }
        }
    }
}

/// Writes entries below a destination directory.
pub struct FsSink {
    dest: PathBuf,
    filter: Vec<String>,
    shared: Arc<Shared>,
    pool: ThreadPool,
    jobs: VecDeque<JoinHandle<()>>,
    /// A file being streamed to disk (large files).
    file: Option<(File, PathBuf)>,
    /// A small file being collected in memory.
    small: Option<SmallWrite>,
    /// Directories whose mode and time are set after everything inside them exists.
    dirs: Vec<(PathBuf, Entry)>,
    /// Directories already checked not to be symbolic links.
    checked: HashSet<PathBuf>,
}

impl FsSink {
    pub fn new(dest: &Path, existing: Existing, filter: &[String]) -> FsSink {
        let threads = std::thread::available_parallelism().map_or(2, |n| n.get());
        FsSink {
            dest: dest.to_path_buf(),
            filter: filter.to_vec(),
            shared: Arc::new(Shared { existing, made: Mutex::new(HashSet::new()), errors: Mutex::new(Vec::new()) }),
            pool: ThreadPool::builder().threads(threads).name("arx-write").build().expect("write threads"),
            jobs: VecDeque::new(),
            file: None,
            small: None,
            dirs: Vec::new(),
            checked: HashSet::new(),
        }
    }

    fn target(&mut self, stored: &str) -> std::result::Result<PathBuf, String> {
        let rel = sanitize(stored).map_err(|e| format!("refused to extract '{}': {e}", stored.escape_debug()))?;
        // Each directory is checked once; a path whose parent was checked already needs no more lookups.
        let parent_ok = rel.parent().is_some_and(|p| !p.as_os_str().is_empty() && self.checked.contains(&self.dest.join(p)));
        let path = if parent_ok { self.dest.join(&rel) } else { join_under(&self.dest, &rel)? };
        if let Some(p) = path.parent() {
            self.checked.insert(p.to_path_buf());
        }
        Ok(path)
    }

    fn wait_for_writes(&mut self, keep: usize) {
        while self.jobs.len() > keep {
            if let Some(j) = self.jobs.pop_front() {
                let _ = j.join();
            }
        }
    }
}

impl Sink for FsSink {
    fn begin_file(&mut self, e: &Entry) -> std::result::Result<bool, String> {
        if !selected(&e.path, &self.filter) {
            return Ok(false);
        }
        let path = self.target(&e.path)?;
        if e.size <= SMALL_FILE {
            self.small = Some(SmallWrite { path, data: Vec::with_capacity(e.size as usize), mode: e.mode, entry: e.clone() });
            return Ok(true);
        }
        if let Some(parent) = path.parent() {
            self.shared.ensure_dir(parent).map_err(|err| format!("cannot create {}: {err}", parent.display()))?;
        }
        match self.shared.open(&path)? {
            Some(f) => {
                self.file = Some((f, path));
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn write(&mut self, data: &[u8]) -> std::result::Result<(), String> {
        if let Some(s) = &mut self.small {
            s.data.extend_from_slice(data);
            return Ok(());
        }
        match &mut self.file {
            Some((f, _)) => f.write_all(data).map_err(|e| e.to_string()),
            None => Ok(()),
        }
    }

    fn end_file(&mut self, e: &Entry, ok: bool) -> std::result::Result<(), String> {
        if let Some(mut w) = self.small.take() {
            if ok {
                w.entry = e.clone();
                let shared = self.shared.clone();
                self.jobs.push_back(self.pool.spawn(move || shared.write_small(w)));
                self.wait_for_writes(MAX_QUEUED_WRITES);
            }
            return Ok(());
        }
        let Some((file, path)) = self.file.take() else { return Ok(()) };
        if ok {
            finish_handle(&file, e, e.mode);
            drop(file);
        } else {
            // A file that failed verification is not left behind looking like a good one.
            drop(file);
            let _ = fs::remove_file(&path);
        }
        Ok(())
    }

    fn dir(&mut self, e: &Entry) -> std::result::Result<(), String> {
        if !selected(&e.path, &self.filter) && !self.filter.iter().any(|f| f.trim_end_matches('/').starts_with(&format!("{}/", e.path))) {
            return Ok(());
        }
        let path = self.target(&e.path)?;
        // Not created yet: the writes of the files inside it create it, in parallel, and whatever
        // is still missing at the end (an empty directory) is created in `finish`.
        self.checked.insert(path.clone());
        self.dirs.push((path, e.clone()));
        Ok(())
    }

    fn symlink(&mut self, e: &Entry) -> std::result::Result<(), String> {
        if !selected(&e.path, &self.filter) {
            return Ok(());
        }
        #[cfg(unix)]
        {
            let path = self.target(&e.path)?;
            if let Some(parent) = path.parent() {
                self.shared.ensure_dir(parent).map_err(|err| format!("cannot create {}: {err}", parent.display()))?;
            }
            if fs::symlink_metadata(&path).is_ok() {
                match self.shared.existing {
                    Existing::Fail => return Err("already exists (use --force to overwrite or --skip-existing)".to_string()),
                    Existing::Skip => return Ok(()),
                    Existing::Overwrite => {
                        let _ = fs::remove_file(&path);
                    }
                }
            }
            std::os::unix::fs::symlink(&e.target, &path).map_err(|err| format!("cannot create the link: {err}"))
        }
        #[cfg(not(unix))]
        {
            let _ = e;
            Err("symbolic links are not restored on this platform".to_string())
        }
    }

    fn finish(&mut self) -> Vec<String> {
        self.wait_for_writes(0);
        // Innermost first, so setting a directory read-only or changing its time does not affect
        // what is still to be done inside it.
        let mut dirs = std::mem::take(&mut self.dirs);
        dirs.sort_by_key(|(p, _)| std::cmp::Reverse(p.components().count()));
        for (p, e) in dirs {
            if let Err(err) = self.shared.ensure_dir(&p) {
                self.shared.errors.lock().unwrap().push(format!("{}: cannot create the directory: {err}", e.path));
                continue;
            }
            set_times(&p, &e);
            set_mode(&p, e.mode);
        }
        std::mem::take(&mut *self.shared.errors.lock().unwrap())
    }
}

/// Writes one file's data to a stream (for `cat`).
pub struct StreamSink<W: Write> {
    pub out: W,
    wanted: String,
}

impl<W: Write> StreamSink<W> {
    pub fn new(out: W, wanted: &str) -> Self {
        StreamSink { out, wanted: wanted.to_string() }
    }
}

impl<W: Write> Sink for StreamSink<W> {
    fn begin_file(&mut self, e: &Entry) -> std::result::Result<bool, String> {
        Ok(e.path == self.wanted)
    }
    fn write(&mut self, data: &[u8]) -> std::result::Result<(), String> {
        self.out.write_all(data).map_err(|e| e.to_string())
    }
    fn end_file(&mut self, _: &Entry, _: bool) -> std::result::Result<(), String> {
        Ok(())
    }
    fn dir(&mut self, _: &Entry) -> std::result::Result<(), String> {
        Ok(())
    }
    fn symlink(&mut self, _: &Entry) -> std::result::Result<(), String> {
        Ok(())
    }
}
