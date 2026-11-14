//! Excludes for build output that `--exclude-caches` misses.
//!
//! cargo marks `target/` with a `CACHEDIR.TAG`, so `--exclude-caches` skips it;
//! game engines and .NET leave no such tag. Before each run the client walks the
//! job's `generatedExcludesFor` roots and lists the output folders it recognises
//! by their siblings:
//!
//! * Unity: `Library`, `Temp`, `obj`, `Logs`, `Build`, `Builds`, `MemoryCaptures`
//!   next to both `Assets` and `ProjectSettings`;
//! * Unreal: `Intermediate`, `DerivedDataCache`, `Binaries`, `Saved` next to a
//!   `.uproject`, and `Intermediate`, `Binaries` next to a plugin's `.uplugin`;
//! * .NET: `bin` and `obj` next to a `.csproj`, `.fsproj` or `.vbproj`;
//! * Godot: `.godot` (and Godot 3's `.import`) next to `project.godot`.
//!
//! Names compare without case: the Windows partitions are NTFS, where `library`
//! and `Library` are the same folder to Unity.
//!
//! restic reads exclude files as patterns, and expands `$NAME` in them from the
//! environment (`$RECYCLE.BIN` silently became `.BIN`). Generated paths are
//! therefore escaped (`\`, `*`, `?`, `[` get a backslash) and every `$` becomes
//! `?`, which matches the `$` itself since no escape keeps restic from expanding
//! it.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

enum Marker {
    /// Every one of these folders is present.
    Dirs(&'static [&'static str]),
    /// A file with one of these extensions is present.
    Ext(&'static [&'static str]),
    /// A file with this name is present.
    File(&'static str),
}

struct Rule {
    marker: Marker,
    outputs: &'static [&'static str],
}

const RULES: &[Rule] = &[
    // Unity
    Rule {
        marker: Marker::Dirs(&["assets", "projectsettings"]),
        outputs: &[
            "library",
            "temp",
            "obj",
            "logs",
            "build",
            "builds",
            "memorycaptures",
        ],
    },
    // Unreal project
    Rule {
        marker: Marker::Ext(&["uproject"]),
        outputs: &["intermediate", "deriveddatacache", "binaries", "saved"],
    },
    // Unreal plugin
    Rule {
        marker: Marker::Ext(&["uplugin"]),
        outputs: &["intermediate", "binaries"],
    },
    // .NET
    Rule {
        marker: Marker::Ext(&["csproj", "fsproj", "vbproj"]),
        outputs: &["bin", "obj"],
    },
    // Godot
    Rule {
        marker: Marker::File("project.godot"),
        outputs: &[".godot", ".import"],
    },
];

/// Folders never worth descending into while looking for projects.
const NEVER_SCANNED: &[&str] = &[".git", ".hg", ".svn", "node_modules"];

pub struct Scan<'a> {
    pub roots: &'a [String],
    /// Absolute paths not to descend into (the job's literal excludes).
    pub skip: HashSet<PathBuf>,
    /// With `--exclude-caches`, folders holding a `CACHEDIR.TAG` are skipped by
    /// restic, so not worth scanning either.
    pub exclude_caches: bool,
}

impl Scan<'_> {
    /// The build-output folders under the roots, sorted.
    pub fn run(&self) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let mut stack: Vec<PathBuf> = self.roots.iter().map(PathBuf::from).collect();
        while let Some(dir) = stack.pop() {
            if self.skip.contains(&dir) {
                continue;
            }
            // Unreadable folders are restic's to report, not ours.
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            let mut dirs: Vec<(String, PathBuf)> = Vec::new();
            let mut files: Vec<String> = Vec::new();
            for e in entries.flatten() {
                let Ok(t) = e.file_type() else { continue };
                let name = e.file_name().to_string_lossy().into_owned();
                if t.is_dir() {
                    dirs.push((name, e.path()));
                } else if t.is_file() {
                    files.push(name);
                }
            }
            if self.exclude_caches && files.iter().any(|f| f == "CACHEDIR.TAG") {
                continue;
            }
            let lower_dirs: HashSet<String> = dirs.iter().map(|(n, _)| n.to_lowercase()).collect();
            let lower_files: Vec<String> = files.iter().map(|f| f.to_lowercase()).collect();
            let mut outputs: HashSet<&str> = HashSet::new();
            for rule in RULES {
                let hit = match &rule.marker {
                    Marker::Dirs(names) => names.iter().all(|n| lower_dirs.contains(*n)),
                    Marker::Ext(exts) => lower_files.iter().any(|f| {
                        Path::new(f)
                            .extension()
                            .and_then(|x| x.to_str())
                            .is_some_and(|x| exts.contains(&x))
                    }),
                    Marker::File(name) => lower_files.iter().any(|f| f == name),
                };
                if hit {
                    outputs.extend(rule.outputs.iter().copied());
                }
            }
            for (name, path) in dirs {
                let lower = name.to_lowercase();
                if outputs.contains(lower.as_str()) {
                    found.push(path);
                } else if !NEVER_SCANNED.contains(&lower.as_str()) {
                    stack.push(path);
                }
            }
        }
        found.sort();
        found
    }
}

/// A literal path as a restic pattern that matches only itself.
pub fn escape_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        match c {
            '\\' | '*' | '?' | '[' => {
                out.push('\\');
                out.push(c);
            }
            '$' => out.push('?'),
            c => out.push(c),
        }
    }
    out
}

/// A pattern written in the configuration, made safe from restic's `$`
/// expansion.
pub fn escape_pattern(pattern: &str) -> String {
    pattern.replace('$', "?")
}

/// Literal absolute excludes (no wildcard), which the scan need not enter.
pub fn literal_paths(patterns: &[String]) -> HashSet<PathBuf> {
    patterns
        .iter()
        .filter(|p| p.starts_with('/') && !p.contains(['*', '?', '[', '\\', '$']))
        .map(|p| PathBuf::from(p.trim_end_matches('/')))
        .collect()
}

/// The exclude file given to restic: the configured patterns, then the
/// generated paths.
pub fn exclude_file(patterns: &[String], generated: &[PathBuf]) -> String {
    let mut out = String::from(
        "# Written by replique-client before each run; edits are overwritten.\n# Configured excludes:\n",
    );
    for p in patterns {
        out.push_str(&escape_pattern(p));
        out.push('\n');
    }
    out.push_str("# Build output found by the scan:\n");
    for p in generated {
        out.push_str(&escape_path(&p.to_string_lossy()));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(root: &Path, rel: &str) {
        let p = root.join(rel);
        if rel.ends_with('/') {
            fs::create_dir_all(&p).unwrap();
        } else {
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, b"x").unwrap();
        }
    }

    #[test]
    fn escaping() {
        assert_eq!(escape_path("/a/b [1]*?/c\\d"), "/a/b \\[1]\\*\\?/c\\\\d");
        assert_eq!(escape_path("/mnt/c/$RECYCLE.BIN"), "/mnt/c/?RECYCLE.BIN");
        assert_eq!(escape_pattern("$RECYCLE.BIN"), "?RECYCLE.BIN");
        assert_eq!(escape_pattern("/home/*/.cache"), "/home/*/.cache");
    }

    #[test]
    fn literal_excludes() {
        let l = literal_paths(&[
            "/home/sigma/.cache/".into(),
            "/home/*/x".into(),
            ".thumbnails".into(),
        ]);
        assert_eq!(l, HashSet::from([PathBuf::from("/home/sigma/.cache")]));
    }

    #[test]
    fn finds_build_output_by_siblings() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        for p in [
            "Game/Assets/a.png",
            "Game/ProjectSettings/p.asset",
            "Game/Library/big",
            "Game/Temp/t",
            "Game/obj/o",
            // Not a Unity project: only Assets.
            "Site/Assets/x",
            "Site/Library/keep",
            "Shooter/Shooter.uproject",
            "Shooter/Intermediate/i",
            "Shooter/Saved/s",
            "Shooter/Content/c",
            "Shooter/Plugins/P/P.uplugin",
            "Shooter/Plugins/P/Binaries/b",
            "Tool/Tool.csproj",
            "Tool/bin/Debug/t.dll",
            "Tool/obj/o",
            "Tool/src/bin/keep.cs",
            "Platformer/project.godot",
            "Platformer/.godot/cache",
            "rust/Cargo.toml",
            "rust/target/CACHEDIR.TAG",
            "rust/target/debug/Game/Assets/x",
            "rust/target/debug/Game/ProjectSettings/x",
            "rust/target/debug/Game/Library/x",
            "skipped/Tool.csproj",
            "skipped/bin/x",
        ] {
            mk(r, p);
        }
        let root = r.to_string_lossy().into_owned();
        let roots = [root];
        let scan = Scan {
            roots: &roots,
            skip: HashSet::from([r.join("skipped")]),
            exclude_caches: true,
        };
        let rel: Vec<String> = scan
            .run()
            .iter()
            .map(|p| p.strip_prefix(r).unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            rel,
            [
                "Game/Library",
                "Game/Temp",
                "Game/obj",
                "Platformer/.godot",
                "Shooter/Intermediate",
                "Shooter/Plugins/P/Binaries",
                "Shooter/Saved",
                "Tool/bin",
                "Tool/obj",
            ]
        );
    }
}
