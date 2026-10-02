//! Turning a path stored in an archive into a path that is safe to create under a destination.
//!
//! An archive is untrusted input. A path such as `../../etc/passwd`, an absolute path, a drive
//! letter or a Windows device name must never make extraction write outside the destination.

use std::path::{Path, PathBuf};

/// Names Windows treats as devices, with or without an extension.
const RESERVED: [&str; 22] = ["CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9"];

/// Checks a stored path and returns it as a relative `PathBuf`.
///
/// Stored paths use `/`. Everything else that could escape or confuse is refused: empty
/// components, `.` and `..`, a leading `/`, any `\` or NUL, a `:` (drive letters and alternate data
/// streams), and Windows device names and names ending in a dot or a space.
pub fn sanitize(stored: &str) -> Result<PathBuf, String> {
    if stored.is_empty() {
        return Err("empty path".to_string());
    }
    if stored.len() > 4096 {
        return Err("path is too long".to_string());
    }
    if stored.contains('\0') {
        return Err("path contains a NUL byte".to_string());
    }
    if stored.contains('\\') {
        return Err("path contains a backslash".to_string());
    }
    if stored.starts_with('/') {
        return Err("path is absolute".to_string());
    }
    let mut out = PathBuf::new();
    for part in stored.split('/') {
        match part {
            "" => return Err("path has an empty component".to_string()),
            "." => return Err("path contains '.'".to_string()),
            ".." => return Err("path climbs out of the destination with '..'".to_string()),
            _ => {}
        }
        if part.contains(':') {
            return Err("path contains ':' (a drive letter or stream name)".to_string());
        }
        if part.ends_with('.') || part.ends_with(' ') {
            return Err("a path component ends in a dot or a space".to_string());
        }
        let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
        if RESERVED.contains(&stem.as_str()) {
            return Err(format!("'{part}' is a reserved Windows device name"));
        }
        if part.chars().any(|c| c.is_control() || matches!(c, '<' | '>' | '"' | '|' | '?' | '*')) {
            return Err("a path component contains a control or reserved character".to_string());
        }
        out.push(part);
    }
    Ok(out)
}

/// Joins a sanitized relative path under `dest` and makes sure no existing parent is a symbolic
/// link, which would let an earlier entry redirect a later one outside the destination.
pub fn join_under(dest: &Path, rel: &Path) -> Result<PathBuf, String> {
    let mut cur = dest.to_path_buf();
    let comps: Vec<_> = rel.components().collect();
    for (i, c) in comps.iter().enumerate() {
        cur.push(c);
        if i + 1 < comps.len() {
            if let Ok(meta) = std::fs::symlink_metadata(&cur) {
                if meta.file_type().is_symlink() {
                    return Err(format!("'{}' is a symbolic link; refusing to write through it", cur.display()));
                }
            }
        }
    }
    Ok(cur)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_paths_pass() {
        for p in ["a", "a/b/c.txt", "dir/файл.txt", "x y/z", ".hidden/file", "a.b.c", "weird name (1).tar.gz"] {
            assert!(sanitize(p).is_ok(), "{p}");
        }
        assert_eq!(sanitize("a/b").unwrap(), PathBuf::from("a").join("b"));
    }

    #[test]
    fn traversal_and_absolute_paths_are_refused() {
        for p in [
            "../x", "a/../../x", "a/..", "..", "/etc/passwd", "//server/share", "C:/x", "C:\\x", "a\\b", "..\\x", "a//b", "a/", "./a", "a/./b", "",
            "a/b:stream", "con", "CON.txt", "a/nul", "a/Lpt1.log", "trailing.", "trailing ", "a\0b", "a/b*c", "tab\there",
        ] {
            assert!(sanitize(p).is_err(), "'{}' should be refused", p.escape_debug());
        }
        assert!(sanitize(&"a/".repeat(3000)).is_err());
    }

    #[test]
    fn nothing_that_passes_can_escape_the_destination() {
        // Whatever sanitize accepts, joining it under a base stays under that base.
        let base = std::env::temp_dir().join("arx-base");
        let candidates = ["a", "a/b", "...", "..a", "a..", "a/..b", ".a", "a/.b/c"];
        for c in candidates {
            if let Ok(rel) = sanitize(c) {
                let joined = base.join(&rel);
                assert!(joined.starts_with(&base), "{c}");
                assert!(rel.components().all(|k| matches!(k, std::path::Component::Normal(_))), "{c}");
            }
        }
    }

    #[test]
    fn join_under_refuses_to_pass_through_a_symlink() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("real");
        std::fs::create_dir(&real).unwrap();
        // A plain directory is fine.
        assert!(join_under(d.path(), Path::new("real/file")).is_ok());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/tmp", d.path().join("link")).unwrap();
            assert!(join_under(d.path(), Path::new("link/file")).is_err());
        }
    }
}
