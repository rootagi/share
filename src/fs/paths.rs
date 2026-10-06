//! Mapping untrusted URL paths onto the shared root.
//!
//! This module is the security boundary of the application. Every path that
//! reaches the filesystem goes through [`resolve`] (or [`resolve_dir`]), which
//! guarantees the result is inside the canonical share root.

use std::path::{Path, PathBuf};

use crate::error::{Result, ShareError};

/// Prefix/suffix of in-progress upload files. They are never listed or served.
pub const TEMP_PREFIX: &str = ".share-upload-";
pub const TEMP_SUFFIX: &str = ".part";

pub fn is_temp_name(name: &str) -> bool {
    name.starts_with(TEMP_PREFIX) && name.ends_with(TEMP_SUFFIX)
}

/// Split a client-supplied relative path into safe components.
///
/// * `..` is rejected outright (not normalised) – there is no legitimate use.
/// * `.` and empty segments are dropped.
/// * NUL bytes are rejected.
/// * Dot-files are reported as *not found* unless `allow_hidden` is set, so
///   the server does not confirm their existence.
pub fn clean_components(rel: &str, allow_hidden: bool) -> Result<Vec<&str>> {
    if rel.contains('\0') {
        return Err(ShareError::BadRequest("path contains a NUL byte".into()));
    }
    let mut out = Vec::new();
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                return Err(ShareError::Forbidden(
                    "path escapes the shared directory".into(),
                ));
            }
            s if is_temp_name(s) => return Err(ShareError::NotFound),
            s if !allow_hidden && s.starts_with('.') => return Err(ShareError::NotFound),
            s => out.push(s),
        }
    }
    Ok(out)
}

/// Resolve `rel` below `root` and verify the canonical result is still inside it.
///
/// `root` must already be canonical. Symlinks are followed, but a link whose
/// target lies outside `root` is refused.
pub async fn resolve(root: &Path, rel: &str, allow_hidden: bool) -> Result<PathBuf> {
    let components = clean_components(rel, allow_hidden)?;
    let mut candidate = root.to_path_buf();
    candidate.extend(&components);
    if components.is_empty() {
        return Ok(candidate);
    }
    let canonical = tokio::fs::canonicalize(&candidate)
        .await
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::PermissionDenied => {
                ShareError::Forbidden("permission denied".into())
            }
            _ => ShareError::NotFound,
        })?;
    if !canonical.starts_with(root) {
        return Err(ShareError::Forbidden(
            "path escapes the shared directory".into(),
        ));
    }
    Ok(canonical)
}

/// [`resolve`] and additionally require a directory.
pub async fn resolve_dir(root: &Path, rel: &str, allow_hidden: bool) -> Result<PathBuf> {
    let path = resolve(root, rel, allow_hidden).await?;
    let meta = tokio::fs::metadata(&path).await?;
    if !meta.is_dir() {
        return Err(ShareError::BadRequest("not a directory".into()));
    }
    Ok(path)
}

/// Resolve `rel` below `root`, creating any missing directories along the way,
/// while verifying every step remains inside the canonical `root`.
pub async fn resolve_or_create_dir(root: &Path, rel: &str, allow_hidden: bool) -> Result<PathBuf> {
    let components = clean_components(rel, allow_hidden)?;
    let mut current = root.to_path_buf();
    for comp in components {
        let safe = sanitize_upload_name(comp)?;
        let next = current.join(&safe);
        match tokio::fs::create_dir(&next).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(ShareError::from(e)),
        }
        let canon = tokio::fs::canonicalize(&next).await?;
        if !canon.starts_with(root) {
            return Err(ShareError::Forbidden(
                "path escapes the shared directory".into(),
            ));
        }
        let meta = tokio::fs::metadata(&canon).await?;
        if !meta.is_dir() {
            return Err(ShareError::BadRequest("not a directory".into()));
        }
        current = canon;
    }
    Ok(current)
}

/// Resolve the existing parent directory and sanitized leaf name for `rel` inside `root`.
pub async fn resolve_parent_and_name(
    root: &Path,
    rel: &str,
    allow_hidden: bool,
) -> Result<(PathBuf, String)> {
    let components = clean_components(rel, allow_hidden)?;
    let Some((&last, parent_parts)) = components.split_last() else {
        return Err(ShareError::Forbidden(
            "cannot modify the shared root".into(),
        ));
    };
    let name = sanitize_upload_name(last)?;
    let parent_rel = parent_parts.join("/");
    let parent = resolve_dir(root, &parent_rel, allow_hidden).await?;
    Ok((parent, name))
}

/// Join a relative directory path and a file name into a `/`-separated relative path.
pub fn join_rel(dir_rel: &str, name: &str) -> String {
    let dir = dir_rel.trim_matches('/');
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

/// Normalise a relative path for display/links (no leading/trailing/duplicate slashes).
pub fn normalize_rel(rel: &str) -> String {
    rel.split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect::<Vec<_>>()
        .join("/")
}

/// Validate a client-supplied upload file name.
///
/// Only a bare file name is accepted: no separators, no NUL, no `.`/`..`,
/// no reserved temp-file pattern. Control characters are replaced and the
/// name is trimmed to the usual 255-byte filesystem limit (keeping the extension).
pub fn sanitize_upload_name(raw: &str) -> Result<String> {
    if raw.contains('/') || raw.contains('\0') || raw.contains('\\') {
        return Err(ShareError::BadRequest(
            "file name must not contain path separators".into(),
        ));
    }
    let cleaned: String = raw
        .chars()
        .map(|c| if c.is_control() { '_' } else { c })
        .collect();
    let cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        return Err(ShareError::BadRequest("invalid file name".into()));
    }
    if is_temp_name(&cleaned) {
        return Err(ShareError::BadRequest("reserved file name".into()));
    }
    Ok(truncate_name(cleaned, 255 - 8)) // leave room for " (9999)"
}

fn truncate_name(name: String, max_bytes: usize) -> String {
    if name.len() <= max_bytes {
        return name;
    }
    let (stem, ext) = split_ext(&name);
    let keep = max_bytes.saturating_sub(ext.len());
    let mut end = keep.min(stem.len());
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &stem[..end], ext)
}

/// Split into (stem, extension-with-dot). Treats `.tar.gz` and friends as one extension.
pub fn split_ext(name: &str) -> (&str, &str) {
    let lower = name.to_ascii_lowercase();
    for compound in [".tar.gz", ".tar.xz", ".tar.bz2", ".tar.zst"] {
        if lower.ends_with(compound) && name.len() > compound.len() {
            let idx = name.len() - compound.len();
            return (&name[..idx], &name[idx..]);
        }
    }
    match name.rfind('.') {
        Some(idx) if idx > 0 => (&name[..idx], &name[idx..]),
        _ => (name, ""),
    }
}

/// `report.pdf`, `report (1).pdf`, `report (2).pdf`, ...
pub fn collision_candidates(name: &str) -> impl Iterator<Item = String> + '_ {
    let (stem, ext) = split_ext(name);
    std::iter::once(name.to_string())
        .chain((1..=9999u32).map(move |n| format!("{stem} ({n}){ext}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn components_are_cleaned() {
        assert_eq!(clean_components("a/b/c", false).unwrap(), ["a", "b", "c"]);
        assert_eq!(
            clean_components("/a//b/./c/", false).unwrap(),
            ["a", "b", "c"]
        );
        assert!(clean_components("", false).unwrap().is_empty());
        assert_eq!(
            clean_components("日本語/ファイル.txt", false)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn dot_dot_is_rejected_everywhere() {
        for bad in ["..", "../x", "a/../b", "a/b/..", "/../../etc/passwd"] {
            assert!(
                matches!(clean_components(bad, true), Err(ShareError::Forbidden(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn hidden_and_temp_files_are_not_found() {
        assert!(matches!(
            clean_components(".ssh/id_rsa", false),
            Err(ShareError::NotFound)
        ));
        assert!(matches!(
            clean_components("a/.env", false),
            Err(ShareError::NotFound)
        ));
        assert!(clean_components("a/.env", true).is_ok());
        assert!(matches!(
            clean_components(".share-upload-1-2.part", true),
            Err(ShareError::NotFound)
        ));
    }

    #[test]
    fn nul_is_rejected() {
        assert!(matches!(
            clean_components("a\0b", true),
            Err(ShareError::BadRequest(_))
        ));
    }

    #[tokio::test]
    async fn resolve_stays_inside_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/file.txt"), b"hi").unwrap();

        let ok = resolve(&root, "sub/file.txt", false).await.unwrap();
        assert_eq!(ok, root.join("sub/file.txt"));
        assert_eq!(resolve(&root, "", false).await.unwrap(), root);
        assert!(matches!(
            resolve(&root, "sub/missing", false).await,
            Err(ShareError::NotFound)
        ));
        assert!(matches!(
            resolve(&root, "../../etc/passwd", false).await,
            Err(ShareError::Forbidden(_))
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_escape_is_refused() {
        let outer = tempfile::tempdir().unwrap();
        let outer_path = std::fs::canonicalize(outer.path()).unwrap();
        std::fs::write(outer_path.join("secret.txt"), b"secret").unwrap();
        let root = outer_path.join("share");
        std::fs::create_dir(&root).unwrap();
        std::os::unix::fs::symlink(outer_path.join("secret.txt"), root.join("link.txt")).unwrap();
        std::os::unix::fs::symlink(&outer_path, root.join("dirlink")).unwrap();
        std::fs::write(root.join("real.txt"), b"ok").unwrap();
        std::os::unix::fs::symlink(root.join("real.txt"), root.join("inner.txt")).unwrap();

        assert!(matches!(
            resolve(&root, "link.txt", false).await,
            Err(ShareError::Forbidden(_))
        ));
        assert!(matches!(
            resolve(&root, "dirlink/secret.txt", false).await,
            Err(ShareError::Forbidden(_))
        ));
        // A link that stays inside the root is fine.
        assert!(resolve(&root, "inner.txt", false).await.is_ok());
    }

    #[test]
    fn upload_names_are_validated() {
        assert_eq!(sanitize_upload_name("photo.jpg").unwrap(), "photo.jpg");
        assert_eq!(
            sanitize_upload_name("  spaced name.txt ").unwrap(),
            "spaced name.txt"
        );
        assert_eq!(sanitize_upload_name("tab\there").unwrap(), "tab_here");
        for bad in [
            "",
            "   ",
            ".",
            "..",
            "a/b",
            "a\\b",
            "a\0b",
            ".share-upload-1.part",
        ] {
            assert!(sanitize_upload_name(bad).is_err(), "{bad:?}");
        }
        let long = format!("{}.txt", "x".repeat(400));
        let cut = sanitize_upload_name(&long).unwrap();
        assert!(cut.len() <= 255 && cut.ends_with(".txt"));
        let unicode = format!("{}.txt", "é".repeat(300));
        assert!(sanitize_upload_name(&unicode).unwrap().len() <= 255);
    }

    #[test]
    fn collision_names() {
        let v: Vec<String> = collision_candidates("report.pdf").take(3).collect();
        assert_eq!(v, ["report.pdf", "report (1).pdf", "report (2).pdf"]);
        let v: Vec<String> = collision_candidates("backup.tar.gz").take(2).collect();
        assert_eq!(v, ["backup.tar.gz", "backup (1).tar.gz"]);
        let v: Vec<String> = collision_candidates("README").take(2).collect();
        assert_eq!(v, ["README", "README (1)"]);
        let v: Vec<String> = collision_candidates(".bashrc").take(2).collect();
        assert_eq!(v, [".bashrc", ".bashrc (1)"]);
    }

    #[test]
    fn rel_helpers() {
        assert_eq!(join_rel("", "a"), "a");
        assert_eq!(join_rel("x/y/", "a"), "x/y/a");
        assert_eq!(normalize_rel("/a//b/./c/"), "a/b/c");
    }
}
