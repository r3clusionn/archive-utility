//! How fast can this machine create the files of the benchmark tree with nothing but `std::fs`?
//! A reference for what extraction can possibly achieve. Usage: fsfloor PATHS_FILE DEST THREADS
//! (PATHS_FILE is the output of `arx list`).

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let paths: Vec<String> = fs::read_to_string(&args[1]).unwrap().lines().map(str::to_string).collect();
    let dest = PathBuf::from(&args[2]);
    let threads: usize = args[3].parse().unwrap();
    let files: Vec<&String> = paths.iter().filter(|p| !p.ends_with('/')).collect();
    let data = vec![b'x'; 30_000];
    let _ = fs::remove_dir_all(&dest);
    fs::create_dir_all(&dest).unwrap();
    let next = AtomicUsize::new(0);
    let t = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Relaxed);
                let Some(p) = files.get(i) else { break };
                let full = dest.join(p);
                fs::create_dir_all(full.parent().unwrap()).unwrap();
                let mut f = OpenOptions::new().write(true).create_new(true).open(&full).unwrap();
                f.write_all(&data).unwrap();
            });
        }
    });
    println!("{} files with {threads} thread(s): {:.2} s", files.len(), t.elapsed().as_secs_f64());
    let _ = fs::remove_dir_all(&dest);
}
