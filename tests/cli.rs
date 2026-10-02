mod common;

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use common::*;

fn arx(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_arx")).args(args).output().unwrap()
}

fn text_of(o: &[u8]) -> String {
    String::from_utf8_lossy(o).replace("\r\n", "\n")
}

fn src_tree() -> (tempfile::TempDir, std::path::PathBuf) {
    let d = tmp();
    let t = d.path().join("project");
    fs::create_dir(&t).unwrap();
    make_tree(&t, 4096);
    (d, t)
}

#[test]
fn create_list_info_verify_extract_round_trip() {
    let (d, tree) = src_tree();
    let a = d.path().join("p.arx");
    let o = arx(&["create", a.to_str().unwrap(), tree.to_str().unwrap(), "--codec", "deflate", "--chunk-kib", "16"]);
    assert!(o.status.success(), "{}", text_of(&o.stderr));
    assert!(text_of(&o.stderr).contains("files,"), "{}", text_of(&o.stderr));

    let list = arx(&["list", a.to_str().unwrap()]);
    let names = text_of(&list.stdout);
    assert!(names.contains("project/five-chunks.txt\n") && names.contains("project/dir/sub/\n") && names.contains("project/empty-dir/nested-empty/\n"), "{names}");
    let long = text_of(&arx(&["list", "-l", a.to_str().unwrap(), "project/dir/sub"]).stdout);
    assert!(long.lines().count() == 4 && long.contains("-0644") && long.contains("d0755") && long.contains(" 20"), "{long}");

    let info = text_of(&arx(&["info", a.to_str().unwrap()]).stdout);
    assert!(info.contains("format:        ARX 1.0") && info.contains("codec:         deflate") && info.contains("files"), "{info}");

    let v = arx(&["verify", a.to_str().unwrap()]);
    assert!(v.status.success(), "{}", text_of(&v.stderr));
    assert!(text_of(&v.stdout).starts_with("ok: "), "{}", text_of(&v.stdout));

    let out = d.path().join("out");
    let x = arx(&["extract", a.to_str().unwrap(), "-C", out.to_str().unwrap(), "-q"]);
    assert!(x.status.success(), "{}", text_of(&x.stderr));
    assert_eq!(snapshot(&tree), snapshot(&out.join("project")));
}

#[test]
fn cat_and_selective_extraction() {
    let (d, tree) = src_tree();
    let a = d.path().join("p.arx");
    assert!(arx(&["create", a.to_str().unwrap(), tree.to_str().unwrap(), "-q"]).status.success());
    let c = arx(&["cat", a.to_str().unwrap(), "project/dir/sub/deep/file.txt"]);
    assert!(c.status.success());
    assert_eq!(c.stdout, fs::read(tree.join("dir/sub/deep/file.txt")).unwrap());
    let missing = arx(&["cat", a.to_str().unwrap(), "project/nope.txt"]);
    assert_eq!(missing.status.code(), Some(2));

    let out = d.path().join("sel");
    assert!(arx(&["extract", a.to_str().unwrap(), "-C", out.to_str().unwrap(), "project/one.bin", "project/many", "-q"]).status.success());
    let got: Vec<_> = snapshot(&out).into_iter().map(|(p, _)| p).collect();
    assert!(got.contains(&"project/one.bin".to_string()) && got.contains(&"project/many/f007.txt".to_string()));
    assert!(!got.iter().any(|p| p.contains("five-chunks") || p.contains("dir/sub")), "{got:?}");
}

#[test]
fn existing_files_need_force_or_skip() {
    let (d, tree) = src_tree();
    let a = d.path().join("p.arx");
    arx(&["create", a.to_str().unwrap(), tree.to_str().unwrap(), "-q"]);
    let out = d.path().join("out");
    assert!(arx(&["extract", a.to_str().unwrap(), "-C", out.to_str().unwrap(), "-q"]).status.success());
    fs::write(out.join("project/one.bin"), b"changed").unwrap();
    let again = arx(&["extract", a.to_str().unwrap(), "-C", out.to_str().unwrap(), "-q"]);
    assert_eq!(again.status.code(), Some(1));
    assert!(text_of(&again.stderr).contains("already exists"), "{}", text_of(&again.stderr));
    assert_eq!(fs::read(out.join("project/one.bin")).unwrap(), b"changed");
    assert!(arx(&["extract", a.to_str().unwrap(), "-C", out.to_str().unwrap(), "-q", "--skip-existing"]).status.success());
    assert_eq!(fs::read(out.join("project/one.bin")).unwrap(), b"changed");
    assert!(arx(&["extract", a.to_str().unwrap(), "-C", out.to_str().unwrap(), "-q", "--force"]).status.success());
    assert_eq!(fs::read(out.join("project/one.bin")).unwrap(), vec![42]);
}

#[test]
fn damage_is_reported_with_exit_status_1() {
    let (d, tree) = src_tree();
    let a = d.path().join("p.arx");
    arx(&["create", a.to_str().unwrap(), tree.to_str().unwrap(), "--codec", "none", "-q"]);
    let mut bytes = fs::read(&a).unwrap();
    let mid = bytes.len() / 3;
    bytes[mid] ^= 0x40;
    fs::write(&a, &bytes).unwrap();
    let v = arx(&["verify", a.to_str().unwrap()]);
    assert_eq!(v.status.code(), Some(1));
    assert!(text_of(&v.stdout).starts_with("DAMAGED"), "{}", text_of(&v.stdout));
    assert!(text_of(&v.stderr).contains("arx: "), "{}", text_of(&v.stderr));
    // Extraction skips the damaged file, writes the others, and says so.
    let out = d.path().join("out");
    let x = arx(&["extract", a.to_str().unwrap(), "-C", out.to_str().unwrap(), "--recover"]);
    assert_eq!(x.status.code(), Some(1));
    let n = snapshot(&out).iter().filter(|(_, c)| c.is_some()).count();
    assert!(n >= 40, "most files survive one flipped byte: {n}");
    assert!(text_of(&x.stderr).contains("damaged"), "{}", text_of(&x.stderr));
    for (p, c) in snapshot(&out) {
        if let Some(c) = c {
            assert_eq!(c, fs::read(d.path().join(&p)).unwrap(), "{p} was written but differs from the original");
        }
    }
}

#[test]
fn archives_stream_through_standard_input_and_output() {
    let (d, tree) = src_tree();
    // create to stdout
    let o = arx(&["create", "-", tree.to_str().unwrap(), "--codec", "lz4", "-q"]);
    assert!(o.status.success(), "{}", text_of(&o.stderr));
    assert!(o.stdout.starts_with(b"ARX\0"));
    // list, verify and extract from stdin
    let run_stdin = |args: &[&str]| -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_arx")).args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let data = o.stdout.clone();
        let mut stdin = child.stdin.take().unwrap();
        let writer = std::thread::spawn(move || {
            let _ = stdin.write_all(&data);
        });
        let out = child.wait_with_output().unwrap();
        writer.join().unwrap();
        out
    };
    let v = run_stdin(&["verify", "-"]);
    assert!(v.status.success(), "{}", text_of(&v.stderr));
    let l = run_stdin(&["list", "-"]);
    assert!(text_of(&l.stdout).contains("project/five-chunks.txt"));
    let out = d.path().join("piped");
    let x = run_stdin(&["extract", "-", "-C", out.to_str().unwrap(), "-q"]);
    assert!(x.status.success(), "{}", text_of(&x.stderr));
    assert_eq!(snapshot(&tree), snapshot(&out.join("project")));
}

#[test]
fn the_archive_does_not_contain_itself() {
    let d = tmp();
    let t = d.path().join("dir");
    fs::create_dir(&t).unwrap();
    fs::write(t.join("a.txt"), b"hello").unwrap();
    let a = t.join("self.arx");
    assert!(arx(&["create", a.to_str().unwrap(), t.to_str().unwrap(), "-q"]).status.success());
    let names = text_of(&arx(&["list", a.to_str().unwrap()]).stdout);
    assert!(names.contains("dir/a.txt") && !names.contains("self.arx"), "{names}");
}

#[test]
fn errors_use_exit_status_2() {
    let d = tmp();
    let none = d.path().join("nope.arx");
    for args in [vec!["verify", none.to_str().unwrap()], vec!["list", none.to_str().unwrap()], vec!["info", none.to_str().unwrap()], vec!["create", d.path().join("x.arx").to_str().unwrap(), none.to_str().unwrap()], vec!["bogus"], vec!["create"]] {
        let o = arx(&args);
        assert_eq!(o.status.code(), Some(2), "{args:?}: {}", text_of(&o.stderr));
    }
    // two inputs with the same name
    let a = d.path().join("one");
    let b = d.path().join("two").join("one");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    let o = arx(&["create", d.path().join("dup.arx").to_str().unwrap(), a.to_str().unwrap(), b.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(2));
    assert!(text_of(&o.stderr).contains("would be stored as 'one'"), "{}", text_of(&o.stderr));
    // not an archive
    let junk = d.path().join("junk.arx");
    fs::write(&junk, vec![0u8; 500]).unwrap();
    let o = arx(&["verify", junk.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(2));
    assert!(text_of(&o.stderr).contains("not an ARX archive"), "{}", text_of(&o.stderr));
    let _ = Path::new("");
}

#[test]
fn an_empty_directory_and_an_empty_file_are_archived() {
    let d = tmp();
    let t = d.path().join("e");
    fs::create_dir_all(t.join("sub")).unwrap();
    fs::write(t.join("zero"), b"").unwrap();
    let a = d.path().join("e.arx");
    assert!(arx(&["create", a.to_str().unwrap(), t.to_str().unwrap(), "-q"]).status.success());
    let out = d.path().join("out");
    assert!(arx(&["extract", a.to_str().unwrap(), "-C", out.to_str().unwrap(), "-q"]).status.success());
    assert!(out.join("e/sub").is_dir());
    assert_eq!(fs::metadata(out.join("e/zero")).unwrap().len(), 0);
    assert!(arx(&["verify", a.to_str().unwrap()]).status.success());
}
