//! File metadata helpers: MIME type, UI "kind", validators (ETag / Last-Modified).

use std::fs::Metadata;
use std::time::{SystemTime, UNIX_EPOCH};

/// Best-effort MIME type from the file name; unknown extensions are `application/octet-stream`.
pub fn mime_for(name: &str) -> mime_guess::Mime {
    mime_guess::from_path(name).first_or_octet_stream()
}

/// Coarse category used by the browser UI to pick an icon and colour.
pub fn kind_for(name: &str, is_dir: bool) -> &'static str {
    if is_dir {
        return "dir";
    }
    let mime = mime_for(name);
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match (mime.type_().as_str(), mime.subtype().as_str()) {
        ("image", _) => "image",
        ("video", _) => "video",
        ("audio", _) => "audio",
        (_, "pdf") => "document",
        _ => match ext.as_str() {
            "zip" | "tar" | "gz" | "tgz" | "xz" | "bz2" | "zst" | "7z" | "rar" | "iso" | "deb"
            | "rpm" | "dmg" | "apk" | "jar" | "appimage" => "archive",
            "doc" | "docx" | "odt" | "xls" | "xlsx" | "ods" | "ppt" | "pptx" | "odp" | "epub"
            | "rtf" => "document",
            "rs" | "py" | "js" | "mjs" | "ts" | "tsx" | "jsx" | "c" | "h" | "cc" | "cpp"
            | "hpp" | "go" | "java" | "kt" | "rb" | "php" | "sh" | "bash" | "zsh" | "fish"
            | "lua" | "json" | "toml" | "yaml" | "yml" | "xml" | "html" | "htm" | "css"
            | "scss" | "sql" | "swift" | "cs" => "code",
            "txt" | "md" | "log" | "csv" | "tsv" | "ini" | "conf" | "cfg" | "nfo" => "text",
            _ if mime.type_() == "text" => "text",
            _ => "file",
        },
    }
}

/// Seconds since the Unix epoch, if the platform provides a modification time.
pub fn modified_secs(meta: &Metadata) -> Option<u64> {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

/// Strong validator derived from size and modification time (nanosecond precision).
pub fn etag(meta: &Metadata) -> String {
    let (secs, nanos) = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| (d.as_secs(), d.subsec_nanos()))
        .unwrap_or((0, 0));
    format!("\"{:x}-{:x}-{:x}\"", meta.len(), secs, nanos)
}

/// `Last-Modified` value, truncated to whole seconds as HTTP requires.
pub fn last_modified(meta: &Metadata) -> Option<SystemTime> {
    let secs = modified_secs(meta)?;
    Some(UNIX_EPOCH + std::time::Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_detection() {
        assert_eq!(mime_for("movie.mkv").essence_str(), "video/x-matroska");
        assert_eq!(mime_for("photo.JPG").essence_str(), "image/jpeg");
        assert_eq!(mime_for("notes.txt").essence_str(), "text/plain");
        assert_eq!(mime_for("doc.pdf").essence_str(), "application/pdf");
        assert_eq!(
            mime_for("blob.unknownext").essence_str(),
            "application/octet-stream"
        );
        assert_eq!(
            mime_for("noextension").essence_str(),
            "application/octet-stream"
        );
    }

    #[test]
    fn kinds() {
        assert_eq!(kind_for("a", true), "dir");
        assert_eq!(kind_for("a.png", false), "image");
        assert_eq!(kind_for("a.mp4", false), "video");
        assert_eq!(kind_for("a.flac", false), "audio");
        assert_eq!(kind_for("a.tar.gz", false), "archive");
        assert_eq!(kind_for("a.rs", false), "code");
        assert_eq!(kind_for("a.pdf", false), "document");
        assert_eq!(kind_for("a.txt", false), "text");
        assert_eq!(kind_for("a.bin", false), "file");
    }

    #[test]
    fn etag_changes_with_content_size() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, b"1").unwrap();
        let a = etag(&std::fs::metadata(&p).unwrap());
        std::fs::write(&p, b"12").unwrap();
        let b = etag(&std::fs::metadata(&p).unwrap());
        assert_ne!(a, b);
        assert!(a.starts_with('"') && a.ends_with('"'));
    }
}
