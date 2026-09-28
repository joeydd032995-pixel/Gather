//! Listing a project folder the user picked, so the webview can upload its
//! files one at a time with their paths, and report the folders left out.
//! Nothing is read here but names and sizes; the bytes are read per file by
//! `read_upload_file` when their turn comes.

use std::path::{Path, PathBuf};

use serde::Serialize;

/// Folders left out whole: version-control history, installed dependencies
/// and tool caches. They aren't descended into; they are listed so the
/// project still shows them. The daemon applies the same list.
const LEFT_OUT_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "bower_components",
    ".venv",
    "venv",
    "__pycache__",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".gradle",
    ".next",
    ".nuxt",
    ".terraform",
];
/// Operating-system and archive-tool clutter, left out without a trace.
const CLUTTER_DIRS: &[&str] = &["__macosx"];
const CLUTTER_FILES: &[&str] = &[".ds_store", "thumbs.db", "desktop.ini", ".localized"];
/// Most files listed from one folder.
const MAX_FILES: usize = 20_000;

#[derive(Serialize)]
pub struct FolderFile {
    /// Path inside the project, `/`-separated.
    pub path: String,
    /// Where it is on disk, for `read_upload_file`.
    pub abs: PathBuf,
    pub size: u64,
}

#[derive(Serialize)]
pub struct FolderListing {
    /// The folder's own name: the project's name.
    pub name: String,
    pub files: Vec<FolderFile>,
    /// Folders left out whole, by their path inside the project.
    pub left_out: Vec<String>,
    /// Folders with nothing in them, so the project's tree keeps them.
    pub empty_folders: Vec<String>,
    /// True when the folder held more than the files listed.
    pub truncated: bool,
}

pub fn list(root: &Path) -> Result<FolderListing, String> {
    let meta = std::fs::symlink_metadata(root).map_err(|e| format!("{}: {e}", root.display()))?;
    if !meta.is_dir() {
        return Err("choose a folder".to_string());
    }
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Project".to_string());
    let mut files = Vec::new();
    let mut left_out = Vec::new();
    let mut empty_folders = Vec::new();
    let mut truncated = false;
    // Depth-first with an explicit stack: deep trees can't overflow it.
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue; // unreadable folder: skip rather than fail the project
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        // Anything kept from this folder: a file, a folder, one left out.
        let mut kept = 0usize;
        for entry in entries.into_iter().rev() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let lower = name.to_ascii_lowercase();
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            // Links are never followed: they can point outside the project.
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                if LEFT_OUT_DIRS.contains(&lower.as_str()) {
                    left_out.push(rel);
                    kept += 1;
                } else if !CLUTTER_DIRS.contains(&lower.as_str()) {
                    stack.push((entry.path(), rel));
                    kept += 1;
                }
            } else if meta.is_file() {
                if CLUTTER_FILES.contains(&lower.as_str()) || lower.starts_with("._") {
                    continue;
                }
                if files.len() >= MAX_FILES {
                    truncated = true;
                    break;
                }
                files.push(FolderFile {
                    path: rel,
                    abs: entry.path(),
                    size: meta.len(),
                });
                kept += 1;
            }
        }
        if kept == 0 && !prefix.is_empty() {
            empty_folders.push(prefix);
        }
        if truncated {
            break;
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    left_out.sort();
    empty_folders.sort();
    Ok(FolderListing {
        name,
        files,
        left_out,
        empty_folders,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_files_with_paths_and_skips_tooling() {
        let dir = tempfile_dir();
        let root = dir.join("Atlas");
        std::fs::create_dir_all(root.join("docs/specs")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("node_modules/x")).unwrap();
        std::fs::write(root.join("README.md"), "# Atlas").unwrap();
        std::fs::write(root.join("docs/specs/api.txt"), "api").unwrap();
        std::fs::write(root.join(".git/HEAD"), "ref").unwrap();
        std::fs::write(root.join("node_modules/x/i.js"), "1").unwrap();
        std::fs::write(root.join(".DS_Store"), "x").unwrap();
        std::fs::create_dir_all(root.join("drafts/empty")).unwrap();
        std::fs::create_dir_all(root.join("only-clutter")).unwrap();
        std::fs::write(root.join("only-clutter/Thumbs.db"), "x").unwrap();
        let listing = list(&root).unwrap();
        assert_eq!(listing.name, "Atlas");
        let paths: Vec<&str> = listing.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["README.md", "docs/specs/api.txt"]);
        assert_eq!(listing.left_out, vec![".git", "node_modules"]);
        assert_eq!(listing.empty_folders, vec!["drafts/empty", "only-clutter"]);
        assert!(!listing.truncated);
        assert!(list(&root.join("README.md")).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn tempfile_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gather-folders-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
