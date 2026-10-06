//! Directory listing and search.
//!
//! Listing is done in a single blocking task (`std::fs`) rather than one
//! `spawn_blocking` hop per entry, which is dramatically cheaper for large
//! directories. Memory use is bounded by *names*, not metadata:
//!
//! * sorting by name needs only the name and the (free) `d_type` of each entry;
//!   `stat` is called only for the entries on the requested page,
//! * sorting by size / date / type has to `stat` every matching entry,
//! * results are paginated, so the JSON response is always small.

use std::cmp::Ordering;
use std::fs::Metadata;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::metadata::{kind_for, mime_for, modified_secs};
use super::paths::{is_temp_name, join_rel};
use crate::error::{Result, ShareError};

/// Upper bounds for recursive search so one request cannot pin a thread forever.
const MAX_SEARCH_RESULTS: usize = 5_000;
const MAX_SEARCH_VISITED: usize = 500_000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortKey {
    #[default]
    Name,
    Size,
    Modified,
    Type,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortOrder {
    #[default]
    Asc,
    Desc,
}

#[derive(Debug, Clone)]
pub struct ListOptions {
    pub sort: SortKey,
    pub order: SortOrder,
    /// Case-insensitive substring filter on the file name.
    pub query: Option<String>,
    pub offset: usize,
    pub limit: usize,
    /// Search sub-directories too (only meaningful together with `query`).
    pub recursive: bool,
    pub show_hidden: bool,
}

impl Default for ListOptions {
    fn default() -> Self {
        Self {
            sort: SortKey::Name,
            order: SortOrder::Asc,
            query: None,
            offset: 0,
            limit: 200,
            recursive: false,
            show_hidden: false,
        }
    }
}

/// One row of a listing, as sent to the browser.
#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub name: String,
    /// Path relative to the share root, `/`-separated, not URL-encoded.
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<u64>,
    pub mime: String,
    pub kind: &'static str,
}

#[derive(Debug)]
pub struct Listing {
    pub entries: Vec<Entry>,
    /// Number of entries that matched, before pagination.
    pub total: usize,
    /// True when a recursive search hit its safety limits.
    pub truncated: bool,
}

struct Raw {
    name: String,
    rel: String,
    key: String,
    is_dir: bool,
    path: PathBuf,
    meta: Option<Metadata>,
}

impl Raw {
    fn ensure_meta(&mut self) {
        if self.meta.is_none() {
            self.meta = std::fs::metadata(&self.path).ok();
        }
    }
    fn size(&self) -> u64 {
        self.meta
            .as_ref()
            .map(|m| if m.is_dir() { 0 } else { m.len() })
            .unwrap_or(0)
    }
    fn modified(&self) -> Option<u64> {
        self.meta.as_ref().and_then(modified_secs)
    }
}

/// List (or search) `dir`, which must be a canonical directory inside `root`.
pub async fn list(
    root: PathBuf,
    dir: PathBuf,
    dir_rel: String,
    opts: ListOptions,
) -> Result<Listing> {
    tokio::task::spawn_blocking(move || list_blocking(&root, &dir, &dir_rel, &opts))
        .await
        .map_err(|e| ShareError::Internal(format!("listing task failed: {e}")))?
        .map_err(ShareError::from)
}

/// Listing of a single shared file (`share ./movie.mkv`).
pub fn single_file(path: &Path, name: &str) -> Result<Listing> {
    let meta = std::fs::metadata(path)?;
    Ok(Listing {
        total: 1,
        truncated: false,
        entries: vec![entry_from(
            name.to_string(),
            name.to_string(),
            false,
            Some(&meta),
        )],
    })
}

pub fn list_blocking(
    root: &Path,
    dir: &Path,
    dir_rel: &str,
    opts: &ListOptions,
) -> io::Result<Listing> {
    let needle = opts
        .query
        .as_deref()
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .map(str::to_lowercase);

    let (mut raws, truncated) = match (&needle, opts.recursive) {
        (Some(n), true) => search(root, dir, dir_rel, n, opts.show_hidden)?,
        _ => (
            scan_one_level(root, dir, dir_rel, needle.as_deref(), opts.show_hidden)?,
            false,
        ),
    };

    // Only stat everything when the sort order needs it.
    if matches!(opts.sort, SortKey::Size | SortKey::Modified) {
        raws.iter_mut().for_each(Raw::ensure_meta);
    }
    let desc = opts.order == SortOrder::Desc;
    raws.sort_by(|a, b| {
        let primary = match opts.sort {
            SortKey::Name => natural_cmp(&a.key, &b.key),
            SortKey::Size => a
                .size()
                .cmp(&b.size())
                .then_with(|| natural_cmp(&a.key, &b.key)),
            SortKey::Modified => a
                .modified()
                .cmp(&b.modified())
                .then_with(|| natural_cmp(&a.key, &b.key)),
            SortKey::Type => ext_of(&a.name)
                .cmp(ext_of(&b.name))
                .then_with(|| natural_cmp(&a.key, &b.key)),
        };
        // Directories always come first, whatever the direction.
        b.is_dir
            .cmp(&a.is_dir)
            .then(if desc { primary.reverse() } else { primary })
    });

    let total = raws.len();
    let entries = raws
        .into_iter()
        .skip(opts.offset)
        .take(opts.limit)
        .map(|mut r| {
            r.ensure_meta();
            entry_from(r.name, r.rel, r.is_dir, r.meta.as_ref())
        })
        .collect();
    Ok(Listing {
        entries,
        total,
        truncated,
    })
}

fn entry_from(name: String, rel: String, is_dir: bool, meta: Option<&Metadata>) -> Entry {
    Entry {
        mime: if is_dir {
            "inode/directory".to_string()
        } else {
            mime_for(&name).to_string()
        },
        kind: kind_for(&name, is_dir),
        size: meta.map(|m| if is_dir { 0 } else { m.len() }).unwrap_or(0),
        modified: meta.and_then(modified_secs),
        name,
        path: rel,
        is_dir,
    }
}

/// Decide whether a directory entry is visible; returns `(name, is_dir, meta-if-already-known)`.
fn classify(
    root: &Path,
    entry: &std::fs::DirEntry,
    show_hidden: bool,
) -> Option<(String, bool, Option<Metadata>, bool)> {
    let name = entry.file_name().into_string().ok()?; // non-UTF-8 names cannot be addressed by URL
    if is_temp_name(&name) || (!show_hidden && name.starts_with('.')) {
        return None;
    }
    let ft = entry.file_type().ok()?;
    if ft.is_symlink() {
        // Follow links, but hide anything that resolves outside the share.
        let target = std::fs::canonicalize(entry.path()).ok()?;
        if !target.starts_with(root) {
            return None;
        }
        let meta = std::fs::metadata(&target).ok()?;
        let is_dir = meta.is_dir();
        if !is_dir && !meta.is_file() {
            return None;
        }
        return Some((name, is_dir, Some(meta), true));
    }
    if ft.is_dir() {
        Some((name, true, None, false))
    } else if ft.is_file() {
        Some((name, false, None, false))
    } else {
        None // sockets, fifos, devices
    }
}

fn scan_one_level(
    root: &Path,
    dir: &Path,
    dir_rel: &str,
    needle: Option<&str>,
    show_hidden: bool,
) -> io::Result<Vec<Raw>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let Ok(entry) = entry else { continue };
        let Some((name, is_dir, meta, _)) = classify(root, &entry, show_hidden) else {
            continue;
        };
        let key = name.to_lowercase();
        if needle.is_some_and(|n| !key.contains(n)) {
            continue;
        }
        out.push(Raw {
            rel: join_rel(dir_rel, &name),
            key,
            is_dir,
            path: entry.path(),
            meta,
            name,
        });
    }
    Ok(out)
}

fn search(
    root: &Path,
    dir: &Path,
    dir_rel: &str,
    needle: &str,
    show_hidden: bool,
) -> io::Result<(Vec<Raw>, bool)> {
    let mut results = Vec::new();
    let mut stack = vec![(dir.to_path_buf(), dir_rel.trim_matches('/').to_string())];
    let mut visited = 0usize;
    let mut truncated = false;

    'walk: while let Some((current, rel)) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in read {
            let Ok(entry) = entry else { continue };
            visited += 1;
            if visited > MAX_SEARCH_VISITED || results.len() >= MAX_SEARCH_RESULTS {
                truncated = true;
                break 'walk;
            }
            let Some((name, is_dir, meta, was_symlink)) = classify(root, &entry, show_hidden)
            else {
                continue;
            };
            let child_rel = join_rel(&rel, &name);
            if is_dir && !was_symlink {
                stack.push((entry.path(), child_rel.clone()));
            }
            if name.to_lowercase().contains(needle) {
                results.push(Raw {
                    key: child_rel.to_lowercase(),
                    rel: child_rel,
                    is_dir,
                    path: entry.path(),
                    meta,
                    name,
                });
            }
        }
    }
    Ok((results, truncated))
}

fn ext_of(name: &str) -> &str {
    name.rsplit_once('.').map(|(_, e)| e).unwrap_or("")
}

/// Case-insensitive "natural" comparison: `file2` sorts before `file10`.
/// Inputs are expected to be lower-cased already.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut ai, mut bi) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let na = take_number(&mut ai);
                let nb = take_number(&mut bi);
                match na.cmp(&nb) {
                    Ordering::Equal => {}
                    other => return other,
                }
            }
            (Some(x), Some(y)) => match x.cmp(&y) {
                Ordering::Equal => {
                    ai.next();
                    bi.next();
                }
                other => return other,
            },
        }
    }
}

/// Parse a run of digits into (significant-length, digits) so that arbitrarily long
/// numbers compare correctly without overflow.
fn take_number(it: &mut std::iter::Peekable<std::str::Chars<'_>>) -> (usize, String) {
    let mut digits = String::new();
    while let Some(&c) = it.peek() {
        if c.is_ascii_digit() {
            digits.push(c);
            it.next();
        } else {
            break;
        }
    }
    let trimmed = digits.trim_start_matches('0').to_string();
    (trimmed.len(), trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        fs::create_dir_all(root.join("zeta")).unwrap();
        fs::create_dir_all(root.join("alpha/deep")).unwrap();
        fs::write(root.join("b.txt"), vec![0u8; 10]).unwrap();
        fs::write(root.join("a10.txt"), vec![0u8; 300]).unwrap();
        fs::write(root.join("a2.txt"), vec![0u8; 20]).unwrap();
        fs::write(root.join(".hidden"), b"h").unwrap();
        fs::write(root.join(".share-upload-1-2.part"), b"partial").unwrap();
        fs::write(root.join("alpha/deep/needle.log"), b"n").unwrap();
        fs::write(root.join("日本語 file.txt"), b"u").unwrap();
        (dir, root)
    }

    fn names(l: &Listing) -> Vec<&str> {
        l.entries.iter().map(|e| e.name.as_str()).collect()
    }

    #[test]
    fn natural_ordering() {
        assert_eq!(natural_cmp("a2", "a10"), Ordering::Less);
        assert_eq!(natural_cmp("a010", "a10"), Ordering::Equal);
        assert_eq!(natural_cmp("abc", "abd"), Ordering::Less);
        assert_eq!(
            natural_cmp("x99999999999999999999", "x100000000000000000000"),
            Ordering::Less
        );
    }

    #[test]
    fn default_listing_folders_first_natural_sorted_hides_dotfiles() {
        let (_g, root) = fixture();
        let l = list_blocking(&root, &root, "", &ListOptions::default()).unwrap();
        assert_eq!(
            names(&l),
            [
                "alpha",
                "zeta",
                "a2.txt",
                "a10.txt",
                "b.txt",
                "日本語 file.txt"
            ]
        );
        assert_eq!(l.total, 6);
        assert!(l.entries[0].is_dir && l.entries[0].kind == "dir");
        assert_eq!(l.entries[2].size, 20);
        assert_eq!(l.entries[2].mime, "text/plain");
    }

    #[test]
    fn hidden_files_shown_on_request_but_temp_files_never() {
        let (_g, root) = fixture();
        let opts = ListOptions {
            show_hidden: true,
            ..Default::default()
        };
        let l = list_blocking(&root, &root, "", &opts).unwrap();
        assert!(names(&l).contains(&".hidden"));
        assert!(!names(&l).iter().any(|n| n.ends_with(".part")));
    }

    #[test]
    fn sort_by_size_desc_keeps_folders_first() {
        let (_g, root) = fixture();
        let opts = ListOptions {
            sort: SortKey::Size,
            order: SortOrder::Desc,
            ..Default::default()
        };
        let l = list_blocking(&root, &root, "", &opts).unwrap();
        let files: Vec<_> = l
            .entries
            .iter()
            .filter(|e| !e.is_dir)
            .map(|e| e.size)
            .collect();
        assert!(files.windows(2).all(|w| w[0] >= w[1]));
        assert!(l.entries[0].is_dir && l.entries[1].is_dir);
    }

    #[test]
    fn pagination_reports_total() {
        let (_g, root) = fixture();
        let opts = ListOptions {
            offset: 2,
            limit: 2,
            ..Default::default()
        };
        let l = list_blocking(&root, &root, "", &opts).unwrap();
        assert_eq!(l.total, 6);
        assert_eq!(names(&l), ["a2.txt", "a10.txt"]);
    }

    #[test]
    fn filter_is_case_insensitive_and_unicode_aware() {
        let (_g, root) = fixture();
        let opts = ListOptions {
            query: Some("A1".into()),
            ..Default::default()
        };
        assert_eq!(
            names(&list_blocking(&root, &root, "", &opts).unwrap()),
            ["a10.txt"]
        );
        let opts = ListOptions {
            query: Some("日本".into()),
            ..Default::default()
        };
        assert_eq!(
            names(&list_blocking(&root, &root, "", &opts).unwrap()),
            ["日本語 file.txt"]
        );
    }

    #[test]
    fn recursive_search_finds_nested_files_with_relative_paths() {
        let (_g, root) = fixture();
        let opts = ListOptions {
            query: Some("needle".into()),
            recursive: true,
            ..Default::default()
        };
        let l = list_blocking(&root, &root, "", &opts).unwrap();
        assert_eq!(l.entries.len(), 1);
        assert_eq!(l.entries[0].path, "alpha/deep/needle.log");
        // Without the recursive flag the same query finds nothing at top level.
        let flat = ListOptions {
            query: Some("needle".into()),
            ..Default::default()
        };
        assert!(
            list_blocking(&root, &root, "", &flat)
                .unwrap()
                .entries
                .is_empty()
        );
    }

    #[test]
    fn nested_listing_builds_relative_paths() {
        let (_g, root) = fixture();
        let l =
            list_blocking(&root, &root.join("alpha"), "alpha", &ListOptions::default()).unwrap();
        assert_eq!(l.entries[0].path, "alpha/deep");
    }

    #[cfg(unix)]
    #[test]
    fn escaping_symlinks_are_hidden() {
        let (_g, root) = fixture();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), b"s").unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret"), root.join("leak")).unwrap();
        std::os::unix::fs::symlink(root.join("b.txt"), root.join("inside-link")).unwrap();
        let l = list_blocking(&root, &root, "", &ListOptions::default()).unwrap();
        assert!(!names(&l).contains(&"leak"));
        assert!(names(&l).contains(&"inside-link"));
    }

    #[test]
    fn large_file_metadata_uses_64_bit_sizes() {
        // A sparse 6 GiB file: no disk space is consumed, but the size exceeds u32.
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let f = fs::File::create(root.join("huge.bin")).unwrap();
        f.set_len(6 * 1024 * 1024 * 1024).unwrap();
        let l = list_blocking(&root, &root, "", &ListOptions::default()).unwrap();
        assert_eq!(l.entries[0].size, 6 * 1024 * 1024 * 1024);
    }
}
