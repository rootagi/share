//! The browser UI, embedded into the executable at compile time.
//!
//! `include_str!` is used instead of a crate such as `rust-embed`: there are only
//! three small files, the compiler does the whole job, and no dependency is needed.

pub struct Asset {
    pub path: &'static str,
    pub content_type: &'static str,
    pub body: &'static str,
}

pub const INDEX_HTML: &str = include_str!("../../assets/web/index.html");
pub const APP_JS: &str = include_str!("../../assets/web/app.js");
pub const STYLE_CSS: &str = include_str!("../../assets/web/style.css");
pub const FAVICON_SVG: &str = include_str!("../../assets/web/favicon.svg");

pub const ASSETS: [Asset; 3] = [
    Asset {
        path: "app.js",
        content_type: "text/javascript; charset=utf-8",
        body: APP_JS,
    },
    Asset {
        path: "style.css",
        content_type: "text/css; charset=utf-8",
        body: STYLE_CSS,
    },
    Asset {
        path: "favicon.svg",
        content_type: "image/svg+xml",
        body: FAVICON_SVG,
    },
];

pub fn find(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|a| a.path == path)
}
