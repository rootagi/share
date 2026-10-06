//! Upload behaviour: streaming, atomic commit, collisions, cancellation, isolation.

mod common;

use bytes::Bytes;
use common::*;
use serde_json::Value;
use std::path::Path;

fn leftover_parts(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".share-upload-"))
        .collect()
}

async fn post(c: &reqwest::Client, url: String, body: Vec<u8>) -> reqwest::Response {
    c.post(url)
        .header("X-Share-Upload", "1")
        .body(body)
        .send()
        .await
        .unwrap()
}

async fn body_json(r: reqwest::Response) -> Value {
    serde_json::from_slice(&r.bytes().await.unwrap()).unwrap()
}

#[tokio::test]
async fn uploads_are_disabled_by_default() {
    let s = TestServer::start(&[]).await;
    let r = post(&client(), s.url("/api/upload?name=x.txt"), b"data".to_vec()).await;
    assert_eq!(r.status(), 403);
    assert!(r.text().await.unwrap().contains("disabled"));
    assert!(!s.root.join("x.txt").exists());
    s.stop().await;
}

#[tokio::test]
async fn read_only_flag_is_incompatible_with_upload() {
    use clap::Parser;
    assert!(share::cli::Cli::try_parse_from(["share", ".", "--read-only", "--upload"]).is_err());
}

#[tokio::test]
async fn upload_requires_the_custom_header() {
    let s = TestServer::start(&["--upload"]).await;
    let r = client()
        .post(s.url("/api/upload?name=x.txt"))
        .body("data")
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        403,
        "cross-site simple requests must not be able to upload"
    );
    assert!(!s.root.join("x.txt").exists());
    s.stop().await;
}

#[tokio::test]
async fn upload_creates_the_file_atomically_and_it_shows_up_in_listings() {
    let s = TestServer::start(&["--upload"]).await;
    let c = client();
    let r = post(
        &c,
        s.url("/api/upload?name=new%20file.txt"),
        b"payload".to_vec(),
    )
    .await;
    assert_eq!(r.status(), 201);
    let v: Value = body_json(r).await;
    assert_eq!(
        (
            v["name"].as_str().unwrap(),
            v["size"].as_u64().unwrap(),
            v["renamed"].as_bool().unwrap()
        ),
        ("new file.txt", 7, false)
    );
    assert_eq!(
        std::fs::read(s.root.join("new file.txt")).unwrap(),
        b"payload"
    );
    assert!(leftover_parts(&s.root).is_empty());
    let listing: Value = body_json(c.get(s.url("/api/files")).send().await.unwrap()).await;
    assert!(
        listing["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["name"] == "new file.txt")
    );
    s.eventually("upload recorded", |m| m.completed_transfers == 1)
        .await;
    assert_eq!(s.state.metrics.snapshot().bytes_received, 7);
    s.stop().await;
}

#[tokio::test]
async fn upload_into_a_subdirectory_and_put_method() {
    let s = TestServer::start(&["--upload"]).await;
    let c = client();
    let r = post(&c, s.url("/api/upload?name=a.bin&dir=sub"), vec![1, 2, 3]).await;
    assert_eq!(r.status(), 201);
    assert_eq!(std::fs::read(s.root.join("sub/a.bin")).unwrap(), [1, 2, 3]);
    let r = c
        .put(s.url("/api/upload?name=put.bin"))
        .body(vec![9u8; 10])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    assert_eq!(std::fs::read(s.root.join("put.bin")).unwrap().len(), 10);
    // Missing target directory.
    assert_eq!(
        post(&c, s.url("/api/upload?name=a.bin&dir=nope"), vec![1])
            .await
            .status(),
        404
    );
    s.stop().await;
}

#[tokio::test]
async fn name_collisions_never_overwrite() {
    let s = TestServer::start(&["--upload"]).await;
    let c = client();
    for (i, expected) in ["hello (1).txt", "hello (2).txt", "hello (3).txt"]
        .into_iter()
        .enumerate()
    {
        let r = post(
            &c,
            s.url("/api/upload?name=hello.txt"),
            format!("v{i}").into_bytes(),
        )
        .await;
        assert_eq!(r.status(), 201);
        let v: Value = body_json(r).await;
        assert_eq!(v["name"], expected);
        assert_eq!(v["renamed"], true);
        assert_eq!(
            std::fs::read_to_string(s.root.join(expected)).unwrap(),
            format!("v{i}")
        );
    }
    assert_eq!(
        std::fs::read_to_string(s.root.join("hello.txt")).unwrap(),
        "hello world",
        "original untouched"
    );
    s.stop().await;
}

#[tokio::test]
async fn hostile_names_and_directories_are_rejected() {
    let s = TestServer::start(&["--upload"]).await;
    let c = client();
    for name in [
        "..",
        ".",
        "",
        "..%2Fescape.txt",
        "a%2Fb.txt",
        "a%5Cb.txt",
        ".share-upload-1-1.part",
    ] {
        let r = post(
            &c,
            s.url(&format!("/api/upload?name={name}")),
            b"x".to_vec(),
        )
        .await;
        assert_eq!(r.status(), 400, "name {name:?}");
    }
    assert!(!s.outer.join("escape.txt").exists());
    for dir in ["..", "../..", "sub/../..", "%2e%2e"] {
        let r = post(
            &c,
            s.url(&format!("/api/upload?name=evil.txt&dir={dir}")),
            b"x".to_vec(),
        )
        .await;
        assert_eq!(r.status(), 403, "dir {dir:?}");
    }
    assert!(!s.outer.join("evil.txt").exists());
    // Uploading to a hidden directory is refused like reading from it.
    std::fs::create_dir(s.root.join(".private")).unwrap();
    assert_eq!(
        post(
            &c,
            s.url("/api/upload?name=x.txt&dir=.private"),
            b"x".to_vec()
        )
        .await
        .status(),
        404
    );
    s.stop().await;
}

#[tokio::test]
async fn large_streamed_upload_without_content_length() {
    let s = TestServer::start(&["--upload"]).await;
    // 48 MiB sent as a chunked stream: the server must handle it without knowing the size upfront.
    let chunk = Bytes::from(vec![0xABu8; 1024 * 1024]);
    let body = reqwest::Body::wrap_stream(futures_util::stream::iter(
        (0..48).map(move |_| Ok::<_, std::io::Error>(chunk.clone())),
    ));
    let r = client()
        .post(s.url("/api/upload?name=stream.bin"))
        .header("X-Share-Upload", "1")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    assert_eq!(
        std::fs::metadata(s.root.join("stream.bin")).unwrap().len(),
        48 * 1024 * 1024
    );
    s.eventually("bytes counted", |m| m.bytes_received == 48 * 1024 * 1024)
        .await;
    s.stop().await;
}

#[tokio::test]
async fn concurrent_uploads_all_land_intact() {
    let s = TestServer::start(&["--upload"]).await;
    let c = client();
    let mut tasks = Vec::new();
    for i in 0..8u8 {
        let (c, url) = (c.clone(), s.url(&format!("/api/upload?name=file{i}.bin")));
        tasks.push(tokio::spawn(async move {
            post(&c, url, vec![i; 2 * 1024 * 1024]).await.status()
        }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap(), 201);
    }
    for i in 0..8u8 {
        let data = std::fs::read(s.root.join(format!("file{i}.bin"))).unwrap();
        assert_eq!(data.len(), 2 * 1024 * 1024);
        assert!(data.iter().all(|b| *b == i), "file{i} is corrupt");
    }
    assert!(leftover_parts(&s.root).is_empty());
    s.stop().await;
}

#[tokio::test]
async fn uploads_and_downloads_run_side_by_side() {
    let s = TestServer::start(&["--upload"]).await;
    let c = client();
    let up = {
        let (c, url) = (c.clone(), s.url("/api/upload?name=during.bin"));
        tokio::spawn(async move { post(&c, url, vec![5u8; 8 * 1024 * 1024]).await.status() })
    };
    let down = {
        let (c, url) = (c.clone(), s.url("/download/big.bin"));
        tokio::spawn(async move {
            c.get(url)
                .send()
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap()
                .len()
        })
    };
    let browse = c.get(s.url("/api/files")).send().await.unwrap().status();
    assert_eq!(
        (
            up.await.unwrap().as_u16(),
            down.await.unwrap(),
            browse.as_u16()
        ),
        (201, BIG_LEN, 200)
    );
    s.stop().await;
}

#[tokio::test]
async fn aborted_upload_leaves_no_partial_file_behind() {
    use tokio::io::AsyncWriteExt;
    let s = TestServer::start(&["--upload"]).await;
    let mut sock = tokio::net::TcpStream::connect(s.addr).await.unwrap();
    sock.write_all(b"POST /api/upload?name=partial.bin HTTP/1.1\r\nHost: t\r\nX-Share-Upload: 1\r\nContent-Length: 10000000\r\n\r\n").await.unwrap();
    sock.write_all(&vec![1u8; 200_000]).await.unwrap();
    s.eventually("temp file appears while receiving", |m| {
        m.active.len() == 1 && m.active[0].transferred > 0
    })
    .await;
    assert_eq!(
        leftover_parts(&s.root).len(),
        1,
        "data goes to a hidden .part file first"
    );
    // The .part file is neither listed nor downloadable while in progress.
    let listing: Value = body_json(
        client()
            .get(s.url("/api/files?q=share-upload"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(listing["total"], 0);
    drop(sock);
    s.eventually("abort recorded", |m| {
        m.aborted_transfers == 1 && m.active.is_empty()
    })
    .await;
    for _ in 0..50 {
        if leftover_parts(&s.root).is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        leftover_parts(&s.root).is_empty(),
        "partial upload must be cleaned up"
    );
    assert!(
        !s.root.join("partial.bin").exists(),
        "a partial upload must never appear under its final name"
    );
    s.stop().await;
}

#[tokio::test]
async fn upload_dir_redirects_all_uploads() {
    let inbox = tempfile::tempdir().unwrap();
    let inbox_path = std::fs::canonicalize(inbox.path()).unwrap();
    let s = TestServer::start(&["--upload-dir", inbox_path.to_str().unwrap()]).await;
    let c = client();
    // `dir` is ignored (even a hostile one): everything lands in the inbox.
    let r = post(&c, s.url("/api/upload?name=in.txt&dir=sub"), b"x".to_vec()).await;
    assert_eq!(r.status(), 201);
    assert_eq!(std::fs::read(inbox_path.join("in.txt")).unwrap(), b"x");
    assert!(!s.root.join("sub/in.txt").exists() && !s.root.join("in.txt").exists());
    let r = post(
        &c,
        s.url("/api/upload?name=in2.txt&dir=../.."),
        b"y".to_vec(),
    )
    .await;
    assert_eq!(r.status(), 201);
    assert!(inbox_path.join("in2.txt").exists());
    let v: Value = body_json(c.get(s.url("/api/status")).send().await.unwrap()).await;
    assert_eq!(v["upload_fixed_dir"], true);
    s.stop().await;
}

#[tokio::test]
async fn single_file_share_with_upload_dir() {
    let dir = tempfile::tempdir().unwrap();
    let file = std::fs::canonicalize(dir.path()).unwrap().join("one.txt");
    std::fs::write(&file, "one").unwrap();
    let inbox = tempfile::tempdir().unwrap();
    let inbox_path = std::fs::canonicalize(inbox.path()).unwrap();
    let s =
        TestServer::start_path(&file, &["--upload-dir", inbox_path.to_str().unwrap()], dir).await;
    let r = post(
        &client(),
        s.url("/api/upload?name=two.txt"),
        b"two".to_vec(),
    )
    .await;
    assert_eq!(r.status(), 201);
    assert!(inbox_path.join("two.txt").exists());
    s.stop().await;
}

#[tokio::test]
async fn resumable_upload_retains_partial_and_resumes_from_offset() {
    let s = TestServer::start(&["--upload"]).await;
    let c = client();

    // Send first half (5 bytes) over a raw TCP connection and disconnect mid-body.
    {
        use tokio::io::AsyncWriteExt;
        let mut sock = tokio::net::TcpStream::connect(s.addr).await.unwrap();
        let req = concat!(
            "POST /api/upload?name=resume.bin&size=10&mtime=999 HTTP/1.1\r\n",
            "Host: t\r\n",
            "X-Share-Upload: 1\r\n",
            "Content-Length: 10\r\n\r\n",
            "hello"
        );
        sock.write_all(req.as_bytes()).await.unwrap();
        sock.flush().await.unwrap();
        s.eventually("5 bytes received by server", |m| {
            m.active.len() == 1 && m.active[0].transferred == 5
        })
        .await;
    }
    s.eventually("upload abort recorded", |m| {
        m.aborted_transfers == 1 && m.active.is_empty()
    })
    .await;

    // Query GET /api/upload/status and HEAD /api/upload to discover the saved offset.
    let status_res = c
        .get(s.url("/api/upload/status?name=resume.bin&size=10&mtime=999"))
        .send()
        .await
        .unwrap();
    assert_eq!(status_res.status(), 200);
    assert_eq!(status_res.headers()["upload-offset"], "5");
    let status_json: Value = body_json(status_res).await;
    assert_eq!(status_json["offset"], 5);

    let head = c
        .head(s.url("/api/upload?name=resume.bin&size=10&mtime=999"))
        .send()
        .await
        .unwrap();
    assert_eq!(head.status(), 200);
    assert_eq!(head.headers()["upload-offset"], "5");

    // Resume from offset 5 with the remaining 5 bytes via ?offset=5 query param.
    let r = c
        .post(s.url("/api/upload?name=resume.bin&size=10&mtime=999&offset=5"))
        .header("X-Share-Upload", "1")
        .body(b"world".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    assert_eq!(
        std::fs::read(s.root.join("resume.bin")).unwrap(),
        b"helloworld"
    );
    assert!(leftover_parts(&s.root).is_empty());
    s.stop().await;
}

#[tokio::test]
async fn recursive_folder_upload_creates_subdirectories_when_mkdir_set() {
    let s = TestServer::start(&["--upload"]).await;
    let c = client();

    let r = post(
        &c,
        s.url("/api/upload?name=deep.txt&dir=folder/sub/leaf&mkdir=true"),
        b"deep-data".to_vec(),
    )
    .await;
    assert_eq!(r.status(), 201);
    assert_eq!(
        std::fs::read(s.root.join("folder/sub/leaf/deep.txt")).unwrap(),
        b"deep-data"
    );

    // Traversal in mkdir is still rejected.
    let bad = post(
        &c,
        s.url("/api/upload?name=evil.txt&dir=folder/../../escape&mkdir=true"),
        b"no".to_vec(),
    )
    .await;
    assert_eq!(bad.status(), 403);
    assert!(!s.outer.join("escape/evil.txt").exists());
    s.stop().await;
}

#[tokio::test]
async fn live_upload_toggle_enables_and_disables_uploads_at_runtime() {
    let s = TestServer::start(&[]).await;
    let c = client();

    assert_eq!(
        post(&c, s.url("/api/upload?name=one.txt"), b"1".to_vec())
            .await
            .status(),
        403
    );

    s.state.set_upload_enabled(true);
    let status: Value = body_json(c.get(s.url("/api/status")).send().await.unwrap()).await;
    assert_eq!(status["upload_enabled"], true);

    let r = post(&c, s.url("/api/upload?name=one.txt"), b"1".to_vec()).await;
    assert_eq!(r.status(), 201);
    assert_eq!(std::fs::read(s.root.join("one.txt")).unwrap(), b"1");

    s.state.set_upload_enabled(false);
    assert_eq!(
        post(&c, s.url("/api/upload?name=two.txt"), b"2".to_vec())
            .await
            .status(),
        403
    );
    s.stop().await;
}
