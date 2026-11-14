//! `requireMounts`: refuse to back up a mount folder while nothing is mounted on
//! it. restic would record it empty, and every file in it would look deleted.

use std::path::Path;

/// Whether `path` is a mountpoint. On Linux this reads the kernel's mount table,
/// which also sees bind mounts and mounts of the parent's own filesystem;
/// elsewhere it compares the device of the folder with its parent's.
pub fn is_mountpoint(path: &Path) -> bool {
    let Ok(real) = path.canonicalize() else {
        return false;
    };
    #[cfg(target_os = "linux")]
    if let Ok(table) = std::fs::read_to_string("/proc/self/mountinfo") {
        let wanted = real.to_string_lossy();
        return table
            .lines()
            .filter_map(|l| l.split(' ').nth(4))
            .any(|m| unescape_mountinfo(m) == wanted);
    }
    device_differs_from_parent(&real)
}

fn device_differs_from_parent(real: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Some(parent) = real.parent() else {
        // The root is always mounted.
        return true;
    };
    match (std::fs::metadata(real), std::fs::metadata(parent)) {
        (Ok(a), Ok(b)) => a.dev() != b.dev() || a.ino() == b.ino(),
        _ => false,
    }
}

/// mountinfo writes space, tab, newline and backslash as `\040`-style octal.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn unescape_mountinfo(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            let v = (b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0');
            out.push(v);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The first configured path that is not a mountpoint, as the run's message.
pub fn check(required: &[String]) -> Result<(), String> {
    for m in required {
        if !is_mountpoint(Path::new(m)) {
            return Err(format!("{m} is not a mountpoint"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_is_mounted_a_temp_folder_is_not() {
        assert!(is_mountpoint(Path::new("/")));
        let d = tempfile::tempdir().unwrap();
        assert!(!is_mountpoint(d.path()));
        assert!(!is_mountpoint(&d.path().join("missing")));
        let missing = d.path().join("missing").to_string_lossy().into_owned();
        assert_eq!(
            check(&["/".into(), missing.clone()]),
            Err(format!("{missing} is not a mountpoint"))
        );
    }

    #[test]
    fn mountinfo_escapes() {
        assert_eq!(unescape_mountinfo("/mnt/win\\040c"), "/mnt/win c");
        assert_eq!(unescape_mountinfo("/plain"), "/plain");
    }
}
