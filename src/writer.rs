//! Creating an archive. Files are cut into chunks; the chunks are compressed in parallel on a
//! thread pool and written in order, with a bounded number in flight so memory use does not grow
//! with the size of the input.

use std::collections::VecDeque;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use workpool::{JoinHandle, ThreadPool};

use crate::codec::{encode_chunk, Encoded};
use crate::entry::*;
use crate::format::*;

#[derive(Clone, Debug)]
pub struct WriterConfig {
    pub codec: Codec,
    /// Deflate level, 1 (fast) to 9 (small). Ignored by the other codecs.
    pub level: u32,
    pub chunk_size: usize,
    /// Compression threads; 0 uses every logical CPU.
    pub threads: usize,
}

impl Default for WriterConfig {
    fn default() -> Self {
        WriterConfig { codec: Codec::Lz4, level: 6, chunk_size: 1 << 20, threads: 0 }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub files: u64,
    pub dirs: u64,
    pub symlinks: u64,
    pub raw_bytes: u64,
    pub stored_bytes: u64,
    /// Size of the finished archive.
    pub archive_bytes: u64,
}

enum Item {
    Begin { entry: usize, meta: Vec<u8> },
    Chunk { entry: usize, index: u64, offset: u64, job: JoinHandle<Encoded> },
    End { entry: usize, size: u64 },
    Plain { entry: usize, kind: Kind, meta: Vec<u8> },
}

pub struct ArchiveWriter<W: Write> {
    out: CountingWriter<W>,
    cfg: WriterConfig,
    pool: ThreadPool,
    entries: Vec<Entry>,
    hashers: Vec<Option<blake3::Hasher>>,
    queue: VecDeque<Item>,
    chunks_in_flight: usize,
    window: usize,
    next_id: u64,
    stats: Stats,
    /// Files the writer skipped or could not store faithfully, with the reason.
    pub warnings: Vec<String>,
    /// A path that must not be archived (the output file when it lives inside the tree).
    pub exclude: Option<PathBuf>,
}

fn unix_time(t: SystemTime) -> (i64, u32) {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64, d.subsec_nanos()),
        Err(e) => {
            let d = e.duration();
            // Before the epoch: floor toward negative infinity so the nanoseconds stay positive.
            if d.subsec_nanos() == 0 {
                (-(d.as_secs() as i64), 0)
            } else {
                (-(d.as_secs() as i64) - 1, 1_000_000_000 - d.subsec_nanos())
            }
        }
    }
}

fn mode_of(meta: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o7777
    }
    #[cfg(not(unix))]
    {
        match (meta.is_dir(), meta.permissions().readonly()) {
            (true, _) => 0o755,
            (false, true) => 0o444,
            (false, false) => 0o644,
        }
    }
}

impl<W: Write> ArchiveWriter<W> {
    pub fn new(w: W, cfg: WriterConfig) -> io::Result<Self> {
        let threads = if cfg.threads == 0 { std::thread::available_parallelism().map_or(1, |n| n.get()) } else { cfg.threads };
        let created = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let mut out = CountingWriter::new(w);
        out.write_all(&Header::new(cfg.codec, cfg.chunk_size as u32, created).encode())?;
        let pool = ThreadPool::builder().threads(threads).name("arx").build()?;
        Ok(ArchiveWriter {
            out,
            cfg,
            pool,
            entries: Vec::new(),
            hashers: Vec::new(),
            queue: VecDeque::new(),
            chunks_in_flight: 0,
            window: threads * 2 + 2,
            next_id: 1,
            stats: Stats::default(),
            warnings: Vec::new(),
            exclude: None,
        })
    }

    fn new_entry(&mut self, kind: EntryKind, path: &str) -> usize {
        let e = Entry::new(kind, self.next_id, path.to_string());
        self.next_id += 1;
        self.entries.push(e);
        self.hashers.push(None);
        self.entries.len() - 1
    }

    pub fn add_dir(&mut self, path: &str, mtime: SystemTime, mode: u32) -> io::Result<()> {
        let i = self.new_entry(EntryKind::Dir, path);
        let (secs, nanos) = unix_time(mtime);
        let e = &mut self.entries[i];
        e.mtime_secs = secs;
        e.mtime_nanos = nanos;
        e.mode = mode;
        let meta = encode_dir(e);
        self.stats.dirs += 1;
        self.queue.push_back(Item::Plain { entry: i, kind: Kind::Dir, meta });
        self.drain(self.window)
    }

    pub fn add_symlink(&mut self, path: &str, target: &str, mtime: SystemTime, mode: u32) -> io::Result<()> {
        let i = self.new_entry(EntryKind::Symlink, path);
        let (secs, nanos) = unix_time(mtime);
        let e = &mut self.entries[i];
        e.target = target.to_string();
        e.mtime_secs = secs;
        e.mtime_nanos = nanos;
        e.mode = mode;
        let meta = encode_symlink(e);
        self.stats.symlinks += 1;
        self.queue.push_back(Item::Plain { entry: i, kind: Kind::Symlink, meta });
        self.drain(self.window)
    }

    /// Adds a file whose `size` bytes come from `reader`. The reader must deliver exactly that many.
    pub fn add_file(&mut self, path: &str, size: u64, mtime: SystemTime, mode: u32, mut reader: impl Read) -> io::Result<()> {
        let i = self.new_entry(EntryKind::File, path);
        let (secs, nanos) = unix_time(mtime);
        {
            let e = &mut self.entries[i];
            e.size = size;
            e.mtime_secs = secs;
            e.mtime_nanos = nanos;
            e.mode = mode;
        }
        self.hashers[i] = Some(blake3::Hasher::new());
        let meta = encode_begin(&self.entries[i]);
        self.queue.push_back(Item::Begin { entry: i, meta });
        let mut offset = 0u64;
        let mut index = 0u64;
        while offset < size {
            let want = (size - offset).min(self.cfg.chunk_size as u64) as usize;
            let mut buf = vec![0u8; want];
            reader.read_exact(&mut buf).map_err(|e| {
                if e.kind() == io::ErrorKind::UnexpectedEof {
                    io::Error::new(io::ErrorKind::UnexpectedEof, format!("{path}: the file got shorter while it was being archived"))
                } else {
                    e
                }
            })?;
            let (codec, level) = (self.cfg.codec, self.cfg.level);
            let job = self.pool.spawn(move || encode_chunk(buf, codec, level));
            self.queue.push_back(Item::Chunk { entry: i, index, offset, job });
            self.chunks_in_flight += 1;
            offset += want as u64;
            index += 1;
            self.drain(self.window)?;
        }
        self.queue.push_back(Item::End { entry: i, size });
        self.stats.files += 1;
        self.drain(self.window)
    }

    /// Adds a file, directory or tree from disk under the archive name `name`.
    pub fn add_path(&mut self, fs_path: &Path, name: &str) -> io::Result<()> {
        if let Some(ex) = &self.exclude {
            if fs::canonicalize(fs_path).ok().as_ref() == Some(ex) {
                return Ok(());
            }
        }
        let meta = fs::symlink_metadata(fs_path)?;
        let mtime = meta.modified().unwrap_or(UNIX_EPOCH);
        if meta.is_dir() {
            self.add_dir(name, mtime, mode_of(&meta))?;
            let mut children: Vec<_> = fs::read_dir(fs_path)?.collect::<io::Result<Vec<_>>>()?;
            children.sort_by_key(|c| c.file_name());
            for c in children {
                let child = c.file_name();
                let Some(child) = child.to_str() else {
                    self.warnings.push(format!("{}: skipped, the name is not valid UTF-8", c.path().display()));
                    continue;
                };
                self.add_path(&c.path(), &format!("{name}/{child}"))?;
            }
        } else if meta.is_symlink() {
            #[cfg(unix)]
            {
                let target = fs::read_link(fs_path)?;
                match target.to_str() {
                    Some(t) => self.add_symlink(name, t, mtime, mode_of(&meta))?,
                    None => self.warnings.push(format!("{}: skipped, the link target is not valid UTF-8", fs_path.display())),
                }
            }
            #[cfg(not(unix))]
            self.warnings.push(format!("{}: skipped, symbolic links are not stored on this platform", fs_path.display()));
        } else if meta.is_file() {
            let f = fs::File::open(fs_path)?;
            self.add_file(name, meta.len(), mtime, mode_of(&meta), io::BufReader::with_capacity(1 << 16, f))?;
        } else {
            self.warnings.push(format!("{}: skipped, not a regular file or directory", fs_path.display()));
        }
        Ok(())
    }

    /// Writes finished items from the front of the queue: everything that is ready, and enough
    /// chunks (waiting for their compression if need be) to get back under `max_in_flight`.
    fn drain(&mut self, max_in_flight: usize) -> io::Result<()> {
        loop {
            let ready = match self.queue.front() {
                None => break,
                Some(Item::Chunk { job, .. }) => job.is_finished() || self.chunks_in_flight > max_in_flight,
                Some(_) => true,
            };
            if !ready {
                break;
            }
            let item = self.queue.pop_front().expect("front exists");
            self.write_item(item)?;
        }
        Ok(())
    }

    fn write_item(&mut self, item: Item) -> io::Result<()> {
        match item {
            Item::Begin { entry, meta } => {
                self.entries[entry].offset = self.out.pos;
                self.out.write_all(&encode_record_head(Kind::FileBegin, 0, &meta, 0, 0))?;
            }
            Item::Plain { entry, kind, meta } => {
                self.entries[entry].offset = self.out.pos;
                self.out.write_all(&encode_record_head(kind, 0, &meta, 0, 0))?;
            }
            Item::Chunk { entry, index, offset, job } => {
                let enc = job.join().map_err(|e| io::Error::other(format!("compression failed: {e}")))?;
                let meta = encode_chunk_meta(&ChunkMeta { file_id: self.entries[entry].id, index, offset });
                self.out.write_all(&encode_record_head(Kind::Chunk, enc.codec as u8, &meta, enc.payload.len() as u64, enc.raw_len))?;
                self.out.write_all(&enc.payload)?;
                self.out.write_all(&enc.digest)?;
                self.out.write_all(&enc.payload_crc.to_le_bytes())?;
                if let Some(h) = &mut self.hashers[entry] {
                    h.update(&enc.digest);
                }
                let e = &mut self.entries[entry];
                e.chunks += 1;
                e.stored += enc.payload.len() as u64;
                self.stats.raw_bytes += enc.raw_len;
                self.stats.stored_bytes += enc.payload.len() as u64;
                self.chunks_in_flight -= 1;
            }
            Item::End { entry, size } => {
                let digest = *self.hashers[entry].take().expect("a file in progress has a hasher").finalize().as_bytes();
                let e = &mut self.entries[entry];
                e.digest = digest;
                let meta = encode_end(&EndMeta { file_id: e.id, chunks: e.chunks, stored: e.stored, size, digest });
                self.out.write_all(&encode_record_head(Kind::FileEnd, 0, &meta, 0, 0))?;
            }
        }
        Ok(())
    }

    /// Writes the index and footer, flushes, and returns the output and what was stored.
    pub fn finish(mut self) -> io::Result<(W, Stats)> {
        while let Some(item) = self.queue.pop_front() {
            self.write_item(item)?;
        }
        let index_offset = self.out.pos;
        let meta = encode_index(&self.entries);
        self.out.write_all(&encode_record_head(Kind::Index, 0, &meta, 0, 0))?;
        let index_len = self.out.pos - index_offset;
        let footer = Footer { index_offset, index_len, entries: self.entries.len() as u64, raw_total: self.stats.raw_bytes };
        self.out.write_all(&footer.encode())?;
        self.out.flush()?;
        self.stats.archive_bytes = self.out.pos;
        let stats = self.stats;
        Ok((self.out.into_inner(), stats))
    }
}
