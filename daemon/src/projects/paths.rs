//! Paths inside an uploaded project: normalized, never escaping the project,
//! and screened for what shouldn't be read at all.

/// Folders whose contents are tooling rather than the project: version
/// control, installed dependencies, caches. They are left out silently.
const IGNORED_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "bower_components",
    "__macosx",
    "__pycache__",
    ".venv",
    "venv",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".gradle",
    ".idea",
    ".next",
    ".nuxt",
    ".terraform",
];
/// Operating-system clutter, left out silently.
const IGNORED_FILES: &[&str] = &[".ds_store", "thumbs.db", "desktop.ini", ".localized"];
/// Files that usually hold keys or passwords. They are recorded as skipped,
/// never read.
const SECRET_NAMES: &[&str] = &[
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
    ".npmrc",
    ".pypirc",
    ".netrc",
    ".pgpass",
    ".htpasswd",
    "credentials",
    "credentials.json",
    "secrets.json",
    "secrets.yaml",
    "secrets.yml",
];
const SECRET_EXTENSIONS: &[&str] = &[
    "pem", "key", "p12", "pfx", "jks", "keystore", "kdbx", "ppk", "asc", "gpg",
];
/// Archives inside a project are not unpacked.
const ARCHIVE_EXTENSIONS: &[&str] = &["zip", "7z", "rar", "tar", "gz", "tgz", "bz2", "xz", "zst"];

const MAX_PATH_BYTES: usize = 4096;
const MAX_SEGMENT_BYTES: usize = 255;
const MAX_DEPTH: usize = 64;

/// A relative path made safe: `/`-separated, no empty, `.` or `..`
/// segments, nothing absolute. `Err` names why a path can't be used.
pub fn normalize(raw: &str) -> Result<String, &'static str> {
    let unified = raw.replace('\\', "/");
    if unified.starts_with('/') || unified.as_bytes().get(1) == Some(&b':') {
        return Err("absolute paths aren't allowed");
    }
    let mut parts = Vec::new();
    for segment in unified.split('/') {
        match segment {
            "" | "." => continue,
            ".." => return Err("paths may not leave the project"),
            s if s.chars().any(char::is_control) => return Err("the path has control characters"),
            s if s.len() > MAX_SEGMENT_BYTES => return Err("a name in the path is too long"),
            s => parts.push(s),
        }
    }
    if parts.is_empty() {
        return Err("the path is empty");
    }
    if parts.len() > MAX_DEPTH {
        return Err("the path is nested too deeply");
    }
    let path = parts.join("/");
    if path.len() > MAX_PATH_BYTES {
        return Err("the path is too long");
    }
    Ok(path)
}

/// What to do with a (normalized) path before reading it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    /// Read it.
    Read,
    /// Leave it out without a trace (tooling folders, OS clutter).
    Ignore,
    /// Record it as skipped, with this reason, without reading it.
    Skip(&'static str),
}

pub fn screen(path: &str) -> Screen {
    let segments: Vec<String> = path.split('/').map(str::to_ascii_lowercase).collect();
    let (name, folders) = segments
        .split_last()
        .expect("normalized paths are non-empty");
    if folders.iter().any(|f| IGNORED_DIRS.contains(&f.as_str())) {
        return Screen::Ignore;
    }
    if IGNORED_FILES.contains(&name.as_str()) || name.starts_with("._") {
        return Screen::Ignore;
    }
    let ext = name.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    if name.starts_with(".env")
        || SECRET_NAMES.contains(&name.as_str())
        || SECRET_EXTENSIONS.contains(&ext)
    {
        return Screen::Skip("looks like it holds keys or passwords, so it wasn't read");
    }
    if ARCHIVE_EXTENSIONS.contains(&ext) {
        return Screen::Skip("archives inside a project aren't unpacked");
    }
    Screen::Read
}

/// Folder paths above `path`, outermost first: `a/b/c.md` → `a`, `a/b`.
pub fn ancestors(path: &str) -> Vec<&str> {
    path.match_indices('/').map(|(i, _)| &path[..i]).collect()
}

pub fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_normalized_and_confined() {
        assert_eq!(normalize("Proj\\docs\\a.md").unwrap(), "Proj/docs/a.md");
        assert_eq!(normalize("./a//b/./c.txt").unwrap(), "a/b/c.txt");
        assert!(normalize("../etc/passwd").is_err());
        assert!(normalize("a/../../b").is_err());
        assert!(normalize("/etc/passwd").is_err());
        assert!(normalize("C:\\Windows\\x").is_err());
        assert!(normalize("a/b\u{0}c").is_err());
        assert!(normalize("").is_err());
        assert!(normalize("./").is_err());
        assert!(normalize(&"a/".repeat(100)).is_err());
    }

    #[test]
    fn tooling_clutter_and_secrets_are_screened() {
        assert_eq!(screen("p/src/main.rs"), Screen::Read);
        assert_eq!(screen("p/.git/config"), Screen::Ignore);
        assert_eq!(screen("p/web/node_modules/x/index.js"), Screen::Ignore);
        assert_eq!(screen("p/.DS_Store"), Screen::Ignore);
        assert_eq!(screen("p/__MACOSX/._a.md"), Screen::Ignore);
        assert!(matches!(screen("p/.env.local"), Screen::Skip(_)));
        assert!(matches!(screen("p/deploy/server.pem"), Screen::Skip(_)));
        assert!(matches!(screen("p/.ssh/id_ed25519"), Screen::Skip(_)));
        assert!(matches!(screen("p/old/backup.zip"), Screen::Skip(_)));
    }

    #[test]
    fn ancestors_are_outermost_first() {
        assert_eq!(ancestors("a/b/c.md"), vec!["a", "a/b"]);
        assert!(ancestors("c.md").is_empty());
        assert_eq!(file_name("a/b/c.md"), "c.md");
    }
}
