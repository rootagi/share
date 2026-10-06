//! End-to-end behaviour of the HTTP server: UI, listing API, security boundary, TLS.

mod common;

use common::*;
use reqwest::StatusCode;
use serde_json::Value;

async fn json(c: &reqwest::Client, url: String) -> (StatusCode, Value) {
    let r = c.get(url).send().await.unwrap();
    let status = r.status();
    let bytes = r.bytes().await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn names(v: &Value) -> Vec<String> {
    v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn ui_is_served_with_security_headers() {
    let s = TestServer::start(&[]).await;
    let c = client();
    for path in ["/", "/browse", "/browse/sub", "/browse/sub/deeper/x"] {
        let r = c.get(s.url(path)).send().await.unwrap();
        assert_eq!(r.status(), 200, "{path}");
        assert!(
            r.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/html")
        );
        assert!(
            r.headers()["content-security-policy"]
                .to_str()
                .unwrap()
                .contains("default-src 'self'")
        );
        assert_eq!(r.headers()["x-content-type-options"], "nosniff");
        let body = r.text().await.unwrap();
        assert!(
            body.contains("/assets/app.js")
                && !body.contains("http://")
                && !body.contains("https://"),
            "no external resources"
        );
    }
    for (path, ct) in [
        ("/assets/app.js", "javascript"),
        ("/assets/style.css", "text/css"),
        ("/assets/favicon.svg", "svg"),
    ] {
        let r = c.get(s.url(path)).send().await.unwrap();
        assert_eq!(r.status(), 200, "{path}");
        assert!(r.headers()["content-type"].to_str().unwrap().contains(ct));
    }
    assert_eq!(
        c.get(s.url("/assets/missing.js"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    s.stop().await;
}

#[tokio::test]
async fn status_api_reports_configuration() {
    let s = TestServer::start(&["--upload"]).await;
    let (st, v) = json(&client(), s.url("/api/status")).await;
    assert_eq!(st, 200);
    assert_eq!(v["name"], "share");
    assert_eq!(v["protocol"], "HTTP");
    assert_eq!(v["kind"], "dir");
    assert_eq!(v["upload_enabled"], true);
    assert!(v["transfers"].is_array());
    s.stop().await;
}

#[tokio::test]
async fn listing_shows_folders_first_hides_dotfiles_and_handles_unicode() {
    let s = TestServer::start(&[]).await;
    let (st, v) = json(&client(), s.url("/api/files")).await;
    assert_eq!(st, 200);
    assert_eq!(
        names(&v),
        [
            "empty-dir",
            "sub",
            "big.bin",
            "empty.txt",
            "hello.txt",
            "日本語 file.txt"
        ]
    );
    assert_eq!(v["total"], 6);
    let big = v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "big.bin")
        .unwrap();
    assert_eq!(big["size"], BIG_LEN);
    assert_eq!(big["kind"], "file");
    assert_eq!(v["entries"][0]["kind"], "dir");
    s.stop().await;
}

#[tokio::test]
async fn listing_supports_subdirs_sorting_pagination_and_filtering() {
    let s = TestServer::start(&[]).await;
    let c = client();
    let (_, v) = json(&c, s.url("/api/files?path=sub")).await;
    assert_eq!(names(&v), ["nested.txt"]);
    assert_eq!(v["entries"][0]["path"], "sub/nested.txt");

    let (_, v) = json(&c, s.url("/api/files?sort=size&order=desc")).await;
    let files: Vec<_> = names(&v).into_iter().skip(2).collect();
    assert_eq!(files[0], "big.bin");

    let (_, v) = json(&c, s.url("/api/files?offset=2&limit=2")).await;
    assert_eq!(v["total"], 6);
    assert_eq!(names(&v), ["big.bin", "empty.txt"]);

    let (_, v) = json(&c, s.url("/api/files?q=HELLO")).await;
    assert_eq!(names(&v), ["hello.txt"]);
    let (_, v) = json(&c, s.url("/api/files?q=%E6%97%A5%E6%9C%AC")).await;
    assert_eq!(names(&v), ["日本語 file.txt"]);

    let (st, v) = json(&c, s.url("/api/files?path=hello.txt")).await;
    assert_eq!(st, 400);
    assert!(v["error"].as_str().unwrap().contains("not a directory"));
    assert_eq!(json(&c, s.url("/api/files?path=nope")).await.0, 404);
    s.stop().await;
}

#[tokio::test]
async fn recursive_search_requires_the_flag() {
    let plain = TestServer::start(&[]).await;
    let (_, v) = json(&client(), plain.url("/api/files?q=nested&recursive=true")).await;
    assert!(
        names(&v).is_empty(),
        "without --recursive the search stays in the current folder"
    );
    plain.stop().await;

    let rec = TestServer::start(&["--recursive"]).await;
    let (_, v) = json(&client(), rec.url("/api/files?q=nested&recursive=true")).await;
    assert_eq!(v["entries"][0]["path"], "sub/nested.txt");
    rec.stop().await;
}

#[tokio::test]
async fn hidden_files_need_the_flag() {
    let s = TestServer::start(&[]).await;
    assert_eq!(
        client()
            .get(s.url("/download/.secret"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    s.stop().await;
    let s = TestServer::start(&["--hidden"]).await;
    let r = client()
        .get(s.url("/download/.secret"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap(), "hidden");
    let (_, v) = json(&client(), s.url("/api/files")).await;
    assert!(names(&v).contains(&".secret".to_string()));
    s.stop().await;
}

#[tokio::test]
async fn directory_traversal_is_blocked_in_every_encoding() {
    let s = TestServer::start(&[]).await;
    let attacks = [
        "/download/../secret.txt",
        "/download/%2e%2e/secret.txt",
        "/download/%2E%2E/secret.txt",
        "/download/..%2fsecret.txt",
        "/download/sub/../../secret.txt",
        "/download/sub/%2e%2e/%2e%2e/secret.txt",
        "/download/sub/..%2f..%2fsecret.txt",
        "/download/%2e%2e%2fsecret.txt",
        "/download//../secret.txt",
        "/download/./../secret.txt",
        "/download/..%5csecret.txt",
        "/download/%00",
        "/download/hello.txt%00.png",
        "/api/files?path=..",
        "/api/files?path=../..",
        "/api/files?path=sub/../..",
        "/api/files?path=%2e%2e",
    ];
    for target in attacks {
        let (status, text) = raw_get(s.addr, target).await;
        assert!(matches!(status, 400 | 403 | 404), "{target} → {status}");
        assert!(!text.contains(SECRET), "{target} leaked the secret file");
    }
    // The absolute path of the secret must not work either.
    let abs = format!("/download/{}", s.outer.join("secret.txt").display());
    let (status, text) = raw_get(s.addr, &abs).await;
    assert!(matches!(status, 403 | 404) && !text.contains(SECRET));
    s.stop().await;
}

#[cfg(unix)]
#[tokio::test]
async fn symlinks_leaving_the_share_are_refused_and_hidden() {
    let s = TestServer::start(&[]).await;
    std::os::unix::fs::symlink(s.outer.join("secret.txt"), s.root.join("leak.txt")).unwrap();
    std::os::unix::fs::symlink(&s.outer, s.root.join("outer-dir")).unwrap();
    std::os::unix::fs::symlink(s.root.join("hello.txt"), s.root.join("inner-link.txt")).unwrap();
    let c = client();
    assert_eq!(
        c.get(s.url("/download/leak.txt"))
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        c.get(s.url("/download/outer-dir/secret.txt"))
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        c.get(s.url("/api/files?path=outer-dir"))
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let (_, v) = json(&c, s.url("/api/files")).await;
    let n = names(&v);
    assert!(!n.contains(&"leak.txt".to_string()) && !n.contains(&"outer-dir".to_string()));
    assert!(n.contains(&"inner-link.txt".to_string()));
    assert_eq!(
        c.get(s.url("/download/inner-link.txt"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "hello world"
    );
    s.stop().await;
}

#[tokio::test]
async fn errors_are_json_with_correct_status_codes() {
    let s = TestServer::start(&[]).await;
    let c = client();
    for (path, code) in [
        ("/download/missing.txt", 404),
        ("/download/sub", 404),
        ("/nope", 404),
        ("/api/nope", 404),
    ] {
        let r = c.get(s.url(path)).send().await.unwrap();
        assert_eq!(r.status(), code, "{path}");
        let v: Value = serde_json::from_slice(&r.bytes().await.unwrap()).unwrap();
        assert_eq!(v["status"], code);
        assert!(v["error"].is_string());
    }
    // Wrong method.
    assert_eq!(
        c.post(s.url("/download/hello.txt"))
            .send()
            .await
            .unwrap()
            .status(),
        405
    );
    s.stop().await;
}

#[tokio::test]
async fn single_file_share_serves_exactly_that_file() {
    let dir = tempfile::tempdir().unwrap();
    let file = std::fs::canonicalize(dir.path())
        .unwrap()
        .join("movie night.mkv");
    std::fs::write(&file, vec![7u8; 5000]).unwrap();
    let s = TestServer::start_path(&file, &[], dir).await;
    let c = client();

    let (_, v) = json(&c, s.url("/api/status")).await;
    assert_eq!(v["kind"], "file");
    let (_, v) = json(&c, s.url("/api/files")).await;
    assert_eq!(names(&v), ["movie night.mkv"]);
    assert_eq!(json(&c, s.url("/api/files?path=sub")).await.0, 404);

    for path in ["/download", "/download/", "/download/movie%20night.mkv"] {
        let r = c.get(s.url(path)).send().await.unwrap();
        assert_eq!(r.status(), 200, "{path}");
        assert_eq!(r.bytes().await.unwrap().len(), 5000);
    }
    assert_eq!(
        c.get(s.url("/download/other.txt"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        c.get(s.url("/download/../x"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    s.stop().await;
}

#[tokio::test]
async fn https_with_user_supplied_certificate() {
    let certs = tempfile::tempdir().unwrap();
    let names = ["127.0.0.1".to_string(), "localhost".to_string()]
        .into_iter()
        .collect();
    let (cert_pem, key_pem) = share::tls::certificate::generate_self_signed(&names).unwrap();
    let (cert, key) = (certs.path().join("c.pem"), certs.path().join("k.pem"));
    std::fs::write(&cert, cert_pem).unwrap();
    std::fs::write(&key, key_pem).unwrap();

    let s = TestServer::start(&[
        "--cert",
        cert.to_str().unwrap(),
        "--key",
        key.to_str().unwrap(),
    ])
    .await;
    assert!(s.base.starts_with("https://"));
    let c = client();
    let r = c.get(s.url("/download/hello.txt")).send().await.unwrap();
    assert_eq!(r.text().await.unwrap(), "hello world");
    // Range requests work over TLS too.
    let r = c
        .get(s.url("/download/hello.txt"))
        .header("Range", "bytes=6-")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 206);
    assert_eq!(r.text().await.unwrap(), "world");
    // A client that insists on verifying the self-signed certificate must be rejected.
    assert!(reqwest::Client::new().get(s.url("/")).send().await.is_err());
    // Plain HTTP to the TLS port must not crash the server.
    let (_, _) = raw_get(s.addr, "/").await;
    assert_eq!(
        c.get(s.url("/api/status")).send().await.unwrap().status(),
        200
    );
    s.stop().await;
}

#[tokio::test]
async fn invalid_certificates_are_reported_clearly() {
    use clap::Parser;
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = (dir.path().join("c.pem"), dir.path().join("k.pem"));
    std::fs::write(&cert, "not a certificate").unwrap();
    std::fs::write(&key, "not a key").unwrap();
    let cli = share::cli::Cli::try_parse_from([
        "share",
        dir.path().to_str().unwrap(),
        "--port",
        "0",
        "--bind",
        "127.0.0.1",
        "--cert",
        cert.to_str().unwrap(),
        "--key",
        key.to_str().unwrap(),
    ])
    .unwrap();
    let config = share::config::Config::from_cli_with(cli, &[]).unwrap();
    let err = share::app::start(config, share::logging::LogBuffer::new(10))
        .await
        .err()
        .unwrap();
    assert!(
        matches!(err, share::ShareError::InvalidCertificate(_)),
        "{err}"
    );
}

#[tokio::test]
async fn port_in_use_is_a_friendly_error() {
    use clap::Parser;
    let s = TestServer::start(&[]).await;
    let port = s.addr.port().to_string();
    let dir = tempfile::tempdir().unwrap();
    let cli = share::cli::Cli::try_parse_from([
        "share",
        dir.path().to_str().unwrap(),
        "--http",
        "--bind",
        "127.0.0.1",
        "--port",
        &port,
    ])
    .unwrap();
    let config = share::config::Config::from_cli_with(cli, &[]).unwrap();
    let err = share::app::start(config, share::logging::LogBuffer::new(10))
        .await
        .err()
        .unwrap();
    assert!(matches!(err, share::ShareError::AddrInUse(_)), "{err}");
    assert!(err.hint().is_some());
    s.stop().await;
}

#[tokio::test]
async fn graceful_shutdown_stops_accepting() {
    let s = TestServer::start(&[]).await;
    let addr = s.addr;
    assert_eq!(
        client()
            .get(s.url("/api/status"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    s.stop().await;
    assert!(
        tokio::net::TcpStream::connect(addr).await.is_err(),
        "listener must be closed after shutdown"
    );
}

#[tokio::test]
async fn capability_url_token_isolates_share_under_secret_prefix() {
    let s = TestServer::start(&["--token", "secret123"]).await;
    let c = client();

    // Unprefixed paths return 404.
    assert_eq!(c.get(s.url("/")).send().await.unwrap().status(), 404);
    assert_eq!(
        c.get(s.url("/api/list")).send().await.unwrap().status(),
        404
    );
    assert_eq!(
        c.get(s.url("/api/status")).send().await.unwrap().status(),
        404
    );
    assert_eq!(
        c.get(s.url("/download/hello.txt"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );

    // Prefixed paths work normally.
    assert_eq!(
        c.get(s.url("/s/secret123/")).send().await.unwrap().status(),
        200
    );
    assert_eq!(
        c.get(s.url("/s/secret123/api/list"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        c.get(s.url("/s/secret123/api/status"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let body = c
        .get(s.url("/s/secret123/download/hello.txt"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(body, "hello world");
    s.stop().await;
}

#[tokio::test]
async fn auth_middleware_protects_api_and_supports_basic_and_pin_session() {
    let s = TestServer::start(&["--pin", "424242"]).await;
    let c = client();

    // SPA shell and static assets remain accessible for browsers so the login modal can render.
    assert_eq!(c.get(s.url("/")).send().await.unwrap().status(), 200);
    assert_eq!(
        c.get(s.url("/assets/app.js"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );

    // CLI clients (curl) hitting GET / receive 401 Unauthorized.
    assert_eq!(
        c.get(s.url("/"))
            .header("User-Agent", "curl/8.5.0")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );

    // Protected endpoints reject unauthenticated requests with 401.
    assert_eq!(
        c.get(s.url("/api/status")).send().await.unwrap().status(),
        401
    );
    assert_eq!(
        c.get(s.url("/download/hello.txt"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );

    // HTTP Basic Auth works for CLI clients (any username, PIN as password).
    let r = c
        .get(s.url("/download/hello.txt"))
        .basic_auth("", Some("424242"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap(), "hello world");

    // Wrong PIN via POST /api/auth returns 401.
    let bad = c
        .post(s.url("/api/auth"))
        .header("Content-Type", "application/json")
        .body(r#"{"pin":"000000"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 401);

    // Correct PIN sets session cookie that unlocks protected routes.
    let ok = c
        .post(s.url("/api/auth"))
        .header("Content-Type", "application/json")
        .body(r#"{"pin":"424242"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
    let set_cookie = ok.headers()["set-cookie"].to_str().unwrap().to_string();
    let cookie_pair = set_cookie.split(';').next().unwrap();

    let authed = c
        .get(s.url("/api/status"))
        .header("Cookie", cookie_pair)
        .send()
        .await
        .unwrap();
    assert_eq!(authed.status(), 200);
    s.stop().await;
}

#[tokio::test]
async fn ephemeral_share_stops_after_max_downloads() {
    let s = TestServer::start(&["--max-downloads", "1"]).await;
    let c = client();
    assert!(!s.running.token.is_cancelled());

    // HEAD requests, partial range scrubs, and inline previews do not burn the download quota.
    assert_eq!(
        c.head(s.url("/download/hello.txt"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        c.get(s.url("/download/hello.txt"))
            .header("Range", "bytes=0-4")
            .send()
            .await
            .unwrap()
            .status(),
        206
    );
    assert_eq!(
        c.get(s.url("/download/hello.txt?inline=1"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(s.state.metrics.completed_downloads(), 0);
    assert!(!s.running.token.is_cancelled());

    // A full download increments completed_downloads and triggers shutdown.
    let r = c.get(s.url("/download/hello.txt")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap(), "hello world");

    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        s.running.token.cancelled(),
    )
    .await
    .expect("server should cancel shutdown token after reaching max-downloads");
    s.stop().await;
}

#[tokio::test]
async fn ephemeral_share_stops_after_expire_timer() {
    let s = TestServer::start(&["--expire", "1s"]).await;
    assert!(!s.running.token.is_cancelled());

    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        s.running.token.cancelled(),
    )
    .await
    .expect("server should cancel shutdown token after expire duration");
    s.stop().await;
}

#[tokio::test]
async fn webdav_read_and_write_operations() {
    let s = TestServer::start(&["--upload"]).await;
    let c = client();

    // 1. OPTIONS
    let opt = c
        .request(reqwest::Method::OPTIONS, s.url("/dav/"))
        .send()
        .await
        .unwrap();
    assert_eq!(opt.status(), 200);
    assert!(opt.headers()["dav"].to_str().unwrap().contains('1'));

    // 2. PROPFIND Depth: 1
    let propfind = reqwest::Method::from_bytes(b"PROPFIND").unwrap();
    let pf = c
        .request(propfind.clone(), s.url("/dav/"))
        .header("Depth", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(pf.status().as_u16(), 207);
    let xml = pf.text().await.unwrap();
    assert!(xml.contains("hello.txt"));
    assert!(xml.contains("/dav/sub/"));

    // 3. GET via WebDAV
    let body = c
        .get(s.url("/dav/hello.txt"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(body, "hello world");

    // 4. MKCOL
    let mkcol = reqwest::Method::from_bytes(b"MKCOL").unwrap();
    let r = c.request(mkcol, s.url("/dav/davdir")).send().await.unwrap();
    assert_eq!(r.status(), 201);
    assert!(s.root.join("davdir").is_dir());

    // 5. PUT
    let r = c
        .put(s.url("/dav/davdir/note.txt"))
        .body("webdav-payload")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    assert_eq!(
        std::fs::read_to_string(s.root.join("davdir/note.txt")).unwrap(),
        "webdav-payload"
    );

    // 6. COPY
    let copy = reqwest::Method::from_bytes(b"COPY").unwrap();
    let r = c
        .request(copy, s.url("/dav/davdir/note.txt"))
        .header("Destination", s.url("/dav/davdir/copy.txt"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    assert_eq!(
        std::fs::read_to_string(s.root.join("davdir/copy.txt")).unwrap(),
        "webdav-payload"
    );

    // 7. MOVE
    let mv = reqwest::Method::from_bytes(b"MOVE").unwrap();
    let r = c
        .request(mv, s.url("/dav/davdir/copy.txt"))
        .header("Destination", s.url("/dav/davdir/moved.txt"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    assert!(!s.root.join("davdir/copy.txt").exists());
    assert_eq!(
        std::fs::read_to_string(s.root.join("davdir/moved.txt")).unwrap(),
        "webdav-payload"
    );

    // 8. LOCK & UNLOCK
    let lock = reqwest::Method::from_bytes(b"LOCK").unwrap();
    let r = c
        .request(lock, s.url("/dav/davdir/moved.txt"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.headers().contains_key("lock-token"));

    let unlock = reqwest::Method::from_bytes(b"UNLOCK").unwrap();
    let r = c
        .request(unlock, s.url("/dav/davdir/moved.txt"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);

    // 9. DELETE
    let r = c
        .delete(s.url("/dav/davdir/moved.txt"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    assert!(!s.root.join("davdir/moved.txt").exists());

    // 10. Root-mounted WebDAV (`dav://host:8080` without `/dav`)
    let opt_root = c
        .request(reqwest::Method::OPTIONS, s.url("/"))
        .send()
        .await
        .unwrap();
    assert_eq!(opt_root.status(), 200);
    assert!(opt_root.headers()["dav"].to_str().unwrap().contains('1'));

    let pf_root = c
        .request(propfind, s.url("/"))
        .header("Depth", "1")
        .body("<?xml version=\"1.0\"?><D:propfind xmlns:D=\"DAV:\"><D:allprop/></D:propfind>")
        .send()
        .await
        .unwrap();
    assert_eq!(pf_root.status().as_u16(), 207);
    let root_xml = pf_root.text().await.unwrap();
    assert!(root_xml.contains("<D:href>/</D:href>"));
    assert!(root_xml.contains("<D:href>/hello.txt</D:href>"));

    let root_get = c
        .get(s.url("/hello.txt"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(root_get, "hello world");

    s.stop().await;
}
