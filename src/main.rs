use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Instant, UNIX_EPOCH};

use arx::entry::{Entry, EntryKind};
use arx::format::{Codec, FormatError};
use arx::reader::{scan, selected, Archive, Existing, FsSink, NullSink, Report, StreamSink};
use arx::writer::{ArchiveWriter, WriterConfig};
use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(name = "arx", version, about = "Archive utility: parallel compression, per-chunk checksums, streaming and corruption recovery")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create an archive from files and directories
    Create {
        /// The archive to write (`-` is standard output)
        archive: String,
        /// Files and directories to add
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Compression
        #[arg(long, value_enum, default_value_t = CodecArg::Lz4)]
        codec: CodecArg,
        /// Deflate level, 1 (fast) to 9 (small)
        #[arg(long, default_value_t = 6, value_parser = clap::value_parser!(u32).range(1..=9))]
        level: u32,
        /// Chunk size in KiB (each chunk is compressed and checked on its own)
        #[arg(long, default_value_t = 1024, value_parser = clap::value_parser!(u32).range(4..=1_048_576))]
        chunk_kib: u32,
        /// Compression threads (default: one per logical CPU)
        #[arg(short, long)]
        jobs: Option<usize>,
        /// Print nothing but errors
        #[arg(short, long)]
        quiet: bool,
    },
    /// List the contents of an archive
    List {
        archive: String,
        /// Only entries at or below these paths
        paths: Vec<String>,
        /// Show permissions, sizes, compression and dates
        #[arg(short, long)]
        long: bool,
    },
    /// Show the archive's header and totals
    Info { archive: PathBuf },
    /// Extract files from an archive
    Extract {
        /// The archive (`-` is standard input, read as a stream)
        archive: String,
        /// Only these paths (and everything below them)
        paths: Vec<String>,
        /// Directory to extract into
        #[arg(short = 'C', long, default_value = ".")]
        dest: PathBuf,
        /// Overwrite files that already exist
        #[arg(long, conflicts_with = "skip_existing")]
        force: bool,
        /// Leave files that already exist alone, without an error
        #[arg(long)]
        skip_existing: bool,
        /// After damage that loses the position of the next record, search for the next intact one and carry on
        #[arg(long)]
        recover: bool,
        #[arg(short, long)]
        jobs: Option<usize>,
        /// Print nothing but errors
        #[arg(short, long)]
        quiet: bool,
    },
    /// Check every checksum in an archive (exit status 1 if anything is wrong)
    Verify {
        archive: String,
        #[arg(short, long)]
        jobs: Option<usize>,
    },
    /// Write one file of the archive to standard output
    Cat { archive: PathBuf, path: String },
}

#[derive(Clone, Copy, ValueEnum)]
enum CodecArg {
    None,
    Lz4,
    Deflate,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("arx: {e}");
            ExitCode::from(2)
        }
    }
}

fn human(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// `YYYY-MM-DD HH:MM` in UTC from Unix seconds (Howard Hinnant's civil-from-days).
fn date(secs: i64) -> String {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", rem / 3600, rem % 3600 / 60)
}

fn open_archive(path: &Path) -> Result<Archive<File>, FormatError> {
    Archive::open(File::open(path)?, 0)
}

fn print_problems(report: &Report) {
    for p in &report.problems {
        eprintln!("arx: {p}");
    }
}

fn run(cli: Cli) -> Result<ExitCode, String> {
    match cli.command {
        Cmd::Create { archive, paths, codec, level, chunk_kib, jobs, quiet } => create(&archive, &paths, codec, level, chunk_kib, jobs.unwrap_or(0), quiet),
        Cmd::List { archive, paths, long } => list(&archive, &paths, long),
        Cmd::Info { archive } => info(&archive),
        Cmd::Extract { archive, paths, dest, force, skip_existing, recover, jobs, quiet } => {
            let existing = if force {
                Existing::Overwrite
            } else if skip_existing {
                Existing::Skip
            } else {
                Existing::Fail
            };
            extract(&archive, &paths, &dest, existing, recover, jobs.unwrap_or(0), quiet)
        }
        Cmd::Verify { archive, jobs } => verify(&archive, jobs.unwrap_or(0)),
        Cmd::Cat { archive, path } => {
            let mut a = open_archive(&archive).map_err(|e| format!("{}: {e}", archive.display()))?;
            let stdout = io::stdout();
            let mut sink = StreamSink::new(BufWriter::new(stdout.lock()), &path);
            if !a.entries.iter().any(|e| e.path == path && e.kind == EntryKind::File) {
                return Err(format!("{path}: no such file in the archive"));
            }
            let report = a.extract_selected(&mut sink, std::slice::from_ref(&path)).map_err(|e| e.to_string())?;
            sink.out.flush().map_err(|e| e.to_string())?;
            print_problems(&report);
            Ok(if report.ok() { ExitCode::SUCCESS } else { ExitCode::from(1) })
        }
    }
}

fn archive_name(p: &Path) -> Result<String, String> {
    let canon = std::fs::canonicalize(p).map_err(|e| format!("{}: {e}", p.display()))?;
    let name = canon.file_name().ok_or_else(|| format!("{}: has no name to store it under", p.display()))?;
    name.to_str().map(str::to_string).ok_or_else(|| format!("{}: the name is not valid UTF-8", p.display()))
}

fn create(archive: &str, paths: &[PathBuf], codec: CodecArg, level: u32, chunk_kib: u32, threads: usize, quiet: bool) -> Result<ExitCode, String> {
    let cfg = WriterConfig {
        codec: match codec {
            CodecArg::None => Codec::None,
            CodecArg::Lz4 => Codec::Lz4,
            CodecArg::Deflate => Codec::Deflate,
        },
        level,
        chunk_size: chunk_kib as usize * 1024,
        threads,
    };
    let mut names = Vec::new();
    for p in paths {
        let n = archive_name(p)?;
        if names.contains(&n) {
            return Err(format!("two inputs would be stored as '{n}'"));
        }
        names.push(n);
    }
    let started = Instant::now();
    let to_file = archive != "-";
    let out: Box<dyn Write> = if to_file {
        Box::new(BufWriter::with_capacity(1 << 20, File::create(archive).map_err(|e| format!("{archive}: {e}"))?))
    } else {
        Box::new(BufWriter::with_capacity(1 << 20, io::stdout().lock()))
    };
    let mut w = ArchiveWriter::new(out, cfg).map_err(|e| e.to_string())?;
    if to_file {
        w.exclude = std::fs::canonicalize(archive).ok();
    }
    let result = (|| -> io::Result<_> {
        for (p, n) in paths.iter().zip(&names) {
            w.add_path(p, n).map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", p.display())))?;
        }
        for warn in std::mem::take(&mut w.warnings) {
            eprintln!("arx: warning: {warn}");
        }
        w.finish()
    })();
    match result {
        Ok((mut out, stats)) => {
            out.flush().map_err(|e| e.to_string())?;
            if !quiet {
                let secs = started.elapsed().as_secs_f64();
                eprintln!(
                    "{} files, {} directories: {} -> {} ({:.1}%) in {:.2} s ({:.0} MB/s of input)",
                    stats.files,
                    stats.dirs,
                    human(stats.raw_bytes),
                    human(stats.archive_bytes),
                    if stats.raw_bytes > 0 { stats.archive_bytes as f64 * 100.0 / stats.raw_bytes as f64 } else { 100.0 },
                    secs,
                    stats.raw_bytes as f64 / secs.max(1e-9) / 1e6
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => {
            if to_file {
                let _ = std::fs::remove_file(archive);
            }
            Err(e.to_string())
        }
    }
}

/// The entries of an archive: from its index if it has one, otherwise by reading it from the front.
fn entries_of(archive: &str) -> Result<(Vec<Entry>, Option<String>), String> {
    if archive != "-" {
        match open_archive(Path::new(archive)) {
            Ok(a) => return Ok((a.entries, None)),
            Err(e) => {
                let f = File::open(archive).map_err(|e| format!("{archive}: {e}"))?;
                let mut sink = NullSink;
                let report = scan(f, 0, &mut sink, true).map_err(|e| format!("{archive}: {e}"))?;
                return Ok((report.scanned, Some(format!("the index could not be used ({e}); listing what could be read from the front of the archive"))));
            }
        }
    }
    let mut sink = NullSink;
    let report = scan(io::stdin().lock(), 0, &mut sink, true).map_err(|e| e.to_string())?;
    Ok((report.scanned, None))
}

fn list(archive: &str, filter: &[String], long: bool) -> Result<ExitCode, String> {
    let (entries, note) = entries_of(archive)?;
    if let Some(n) = note {
        eprintln!("arx: warning: {n}");
    }
    let out = io::stdout();
    let mut out = BufWriter::new(out.lock());
    for e in entries.iter().filter(|e| selected(&e.path, filter)) {
        let suffix = if e.kind == EntryKind::Dir { "/" } else { "" };
        let tail = if e.kind == EntryKind::Symlink { format!(" -> {}", e.target) } else { String::new() };
        if long {
            let kind = match e.kind {
                EntryKind::File => '-',
                EntryKind::Dir => 'd',
                EntryKind::Symlink => 'l',
            };
            let ratio = if e.kind == EntryKind::File && e.size > 0 { format!("{:>5.1}%", e.stored as f64 * 100.0 / e.size as f64) } else { "      ".into() };
            writeln!(out, "{kind}{:04o} {:>12} {ratio} {} {}{suffix}{tail}", e.mode & 0o7777, e.size, date(e.mtime_secs), e.path).ok();
        } else {
            writeln!(out, "{}{suffix}{tail}", e.path).ok();
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn info(archive: &Path) -> Result<ExitCode, String> {
    let len = std::fs::metadata(archive).map_err(|e| format!("{}: {e}", archive.display()))?.len();
    let a = open_archive(archive).map_err(|e| format!("{}: {e}", archive.display()))?;
    let (mut files, mut dirs, mut links, mut raw, mut stored) = (0u64, 0u64, 0u64, 0u64, 0u64);
    for e in &a.entries {
        match e.kind {
            EntryKind::File => {
                files += 1;
                raw += e.size;
                stored += e.stored;
            }
            EntryKind::Dir => dirs += 1,
            EntryKind::Symlink => links += 1,
        }
    }
    println!("format:        ARX {}.{}", a.header.major, a.header.minor);
    println!("created:       {} UTC", date(a.header.created as i64));
    println!("codec:         {} (chunks that do not shrink are stored as they are)", a.header.default_codec.name());
    println!("chunk size:    {}", human(a.header.chunk_size as u64));
    println!("entries:       {files} files, {dirs} directories, {links} symbolic links");
    println!("data:          {} stored for {} of files ({:.1}%)", human(stored), human(raw), if raw > 0 { stored as f64 * 100.0 / raw as f64 } else { 100.0 });
    println!("archive size:  {}", human(len));
    Ok(ExitCode::SUCCESS)
}

fn extract(archive: &str, paths: &[String], dest: &Path, existing: Existing, recover: bool, threads: usize, quiet: bool) -> Result<ExitCode, String> {
    std::fs::create_dir_all(dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    let mut sink = FsSink::new(dest, existing, paths);
    let started = Instant::now();
    let report = if archive == "-" {
        scan(io::stdin().lock(), threads, &mut sink, recover).map_err(|e| e.to_string())?
    } else {
        let path = Path::new(archive);
        let selective = !paths.is_empty();
        match (selective, open_archive(path)) {
            (true, Ok(mut a)) => a.extract_selected(&mut sink, paths).map_err(|e| e.to_string())?,
            (_, _) => {
                let f = File::open(path).map_err(|e| format!("{archive}: {e}"))?;
                scan(f, threads, &mut sink, recover).map_err(|e| format!("{archive}: {e}"))?
            }
        }
    };
    print_problems(&report);
    if report.skipped_bytes > 0 {
        eprintln!("arx: skipped {} damaged bytes while looking for the next intact record", report.skipped_bytes);
    }
    if !quiet {
        eprintln!(
            "extracted {} files ({}) and {} directories in {:.2} s{}",
            report.files_ok,
            human(report.bytes),
            report.dirs,
            started.elapsed().as_secs_f64(),
            if report.files_bad > 0 { format!("; {} files were damaged and not written", report.files_bad) } else { String::new() }
        );
    }
    Ok(if report.ok() { ExitCode::SUCCESS } else { ExitCode::from(1) })
}

fn verify(archive: &str, threads: usize) -> Result<ExitCode, String> {
    let started = Instant::now();
    let mut sink = NullSink;
    let report = if archive == "-" {
        scan(io::stdin().lock(), threads, &mut sink, true).map_err(|e| e.to_string())?
    } else {
        let f = File::open(archive).map_err(|e| format!("{archive}: {e}"))?;
        scan(f, threads, &mut sink, true).map_err(|e| format!("{archive}: {e}"))?
    };
    print_problems(&report);
    let secs = started.elapsed().as_secs_f64();
    if report.ok() {
        println!("ok: {} files ({}), {} directories, every checksum matches ({:.2} s, {:.0} MB/s)", report.files_ok, human(report.bytes), report.dirs, secs, report.bytes as f64 / secs.max(1e-9) / 1e6);
        Ok(ExitCode::SUCCESS)
    } else {
        println!("DAMAGED: {} problem(s); {} files verified, {} files damaged", report.problems.len(), report.files_ok, report.files_bad);
        Ok(ExitCode::from(1))
    }
}

#[allow(dead_code)]
fn _unused(_: std::time::SystemTime) {
    let _ = UNIX_EPOCH;
}
