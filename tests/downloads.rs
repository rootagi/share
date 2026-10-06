//! Download behaviour: streaming, HTTP range/resume, conditional requests, large files, concurrency.

mod common;

use common::*;
use futures_util::StreamExt;
use reqwest::StatusCode;
use std::time::Duration;

fn expected(range: std::ops::Range<usize>) -> Vec<u8> {
    big_bytes()[range].to_vec()
}

#[tokio::test]
async fn full_download_has_correct_headers_and_content() {
    let s = TestServer::start(&[]).await;
    let r = client()
        .get(s.url("/download/big.bin"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let h = r.headers().clone();
    assert_eq!(h["content-length"].to_str().unwrap(), BIG_LEN.to_string());
    assert_eq!(h["accept-ranges"], "bytes");
    assert_eq!(h["content-type"], "application/octet-stream");
    assert!(h["etag"].to_str().unwrap().starts_with('"'));
    assert!(h.contains_key("last-modified"));
    assert!(
        h["content-disposition"]
            .to_str()
            .unwrap()
            .starts_with("attachment; filename=\"big.bin\"")
    );
    assert_eq!(r.bytes().await.unwrap().to_vec(), big_bytes());
    s.eventually("completed download recorded", |m| {
        m.completed_transfers == 1 && m.active.is_empty()
    })
    .await;
    assert_eq!(s.state.metrics.snapshot().bytes_sent, BIG_LEN as u64);
    s.stop().await;
}

#[tokio::test]
async fn unicode_names_and_mime_types() {
    let s = TestServer::start(&[]).await;
    let r = client()
        .get(s.url("/download/%E6%97%A5%E6%9C%AC%E8%AA%9E%20file.txt"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let cd = r.headers()["content-disposition"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        cd.contains("filename*=UTF-8''%E6%97%A5%E6%9C%AC%E8%AA%9E%20file.txt"),
        "{cd}"
    );
    assert!(cd.is_ascii());
    assert_eq!(r.headers()["content-type"], "text/plain");
    assert_eq!(r.text().await.unwrap(), "unicode");
    s.stop().await;
}

#[tokio::test]
async fn closed_range_returns_206_with_content_range() {
    let s = TestServer::start(&[]).await;
    let r = client()
        .get(s.url("/download/big.bin"))
        .header("Range", "bytes=1000-1999")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        r.headers()["content-range"].to_str().unwrap(),
        format!("bytes 1000-1999/{BIG_LEN}")
    );
    assert_eq!(r.headers()["content-length"], "1000");
    assert_eq!(r.bytes().await.unwrap().to_vec(), expected(1000..2000));
    s.stop().await;
}

#[tokio::test]
async fn open_ended_suffix_and_clamped_ranges() {
    let s = TestServer::start(&[]).await;
    let c = client();
    let get = |range: &'static str| {
        let c = c.clone();
        let url = s.url("/download/big.bin");
        async move { c.get(url).header("Range", range).send().await.unwrap() }
    };
    let r = get("bytes=3145700-").await;
    assert_eq!(
        (
            r.status(),
            r.headers()["content-range"].to_str().unwrap().to_string()
        ),
        (
            StatusCode::PARTIAL_CONTENT,
            format!("bytes 3145700-{}/{BIG_LEN}", BIG_LEN - 1)
        )
    );
    assert_eq!(
        r.bytes().await.unwrap().to_vec(),
        expected(3145700..BIG_LEN)
    );

    let r = get("bytes=-100").await;
    assert_eq!(r.status(), 206);
    assert_eq!(
        r.bytes().await.unwrap().to_vec(),
        expected(BIG_LEN - 100..BIG_LEN)
    );

    let r = get("bytes=0-99999999999").await;
    assert_eq!(r.status(), 206);
    assert_eq!(
        r.headers()["content-length"].to_str().unwrap(),
        BIG_LEN.to_string()
    );
    s.stop().await;
}

#[tokio::test]
async fn unsatisfiable_range_is_416_with_size() {
    let s = TestServer::start(&[]).await;
    let c = client();
    for range in [
        format!("bytes={BIG_LEN}-"),
        "bytes=99999999-100000000".to_string(),
        "bytes=-0".to_string(),
    ] {
        let r = c
            .get(s.url("/download/big.bin"))
            .header("Range", &range)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::RANGE_NOT_SATISFIABLE, "{range}");
        assert_eq!(
            r.headers()["content-range"].to_str().unwrap(),
            format!("bytes */{BIG_LEN}")
        );
    }
    // Any range on an empty file is unsatisfiable, but a plain GET works.
    let r = c
        .get(s.url("/download/empty.txt"))
        .header("Range", "bytes=0-")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 416);
    let r = c.get(s.url("/download/empty.txt")).send().await.unwrap();
    assert_eq!(
        (r.status(), r.bytes().await.unwrap().len()),
        (StatusCode::OK, 0)
    );
    s.stop().await;
}

#[tokio::test]
async fn malformed_ranges_are_400_and_unknown_forms_fall_back_to_200() {
    let s = TestServer::start(&[]).await;
    let c = client();
    for bad in [
        "bytes=abc-",
        "bytes=5-2",
        "bytes=",
        "garbage",
        "bytes=--5",
        "bytes=1-2-3",
    ] {
        let r = c
            .get(s.url("/download/big.bin"))
            .header("Range", bad)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "{bad}");
    }
    for ignored in ["items=0-5", "bytes=0-5,10-15"] {
        let r = c
            .get(s.url("/download/big.bin"))
            .header("Range", ignored)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "{ignored}");
        assert_eq!(r.content_length(), Some(BIG_LEN as u64));
    }
    s.stop().await;
}

#[tokio::test]
async fn interrupted_download_can_be_resumed_byte_exactly() {
    let s = TestServer::start(&[]).await;
    let c = client();
    // First attempt: read part of the body, then walk away.
    let r = c.get(s.url("/download/big.bin")).send().await.unwrap();
    let mut stream = r.bytes_stream();
    let mut got = Vec::new();
    while got.len() < 500_000 {
        got.extend_from_slice(&stream.next().await.unwrap().unwrap());
    }
    drop(stream);
    got.truncate(500_000);
    // Resume exactly where we left off, the way `curl -C -` and `wget -c` do.
    let r = c
        .get(s.url("/download/big.bin"))
        .header("Range", "bytes=500000-")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 206);
    got.extend_from_slice(&r.bytes().await.unwrap());
    assert_eq!(got, big_bytes());
    s.stop().await;
}

#[tokio::test]
async fn if_range_and_conditional_requests() {
    let s = TestServer::start(&[]).await;
    let c = client();
    let first = c
        .get(s.url("/download/big.bin"))
        .header("Range", "bytes=0-9")
        .send()
        .await
        .unwrap();
    let etag = first.headers()["etag"].to_str().unwrap().to_string();
    let modified = first.headers()["last-modified"]
        .to_str()
        .unwrap()
        .to_string();

    // If-Range with the current validator keeps the range.
    for validator in [etag.clone(), modified.clone()] {
        let r = c
            .get(s.url("/download/big.bin"))
            .header("Range", "bytes=0-9")
            .header("If-Range", validator)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 206);
    }
    // A stale validator means "send the whole thing".
    let r = c
        .get(s.url("/download/big.bin"))
        .header("Range", "bytes=0-9")
        .header("If-Range", "\"stale\"")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.content_length(), Some(BIG_LEN as u64));

    // If-None-Match → 304 with no body.
    let r = c
        .get(s.url("/download/big.bin"))
        .header("If-None-Match", &etag)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 304);
    assert!(r.bytes().await.unwrap().is_empty());
    let r = c
        .get(s.url("/download/big.bin"))
        .header("If-None-Match", "\"other\"")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    s.stop().await;
}

#[tokio::test]
async fn head_returns_headers_without_a_transfer() {
    let s = TestServer::start(&[]).await;
    let r = client()
        .head(s.url("/download/big.bin"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.headers()["content-length"].to_str().unwrap(),
        BIG_LEN.to_string()
    );
    assert!(r.bytes().await.unwrap().is_empty());
    let r = client()
        .head(s.url("/download/big.bin"))
        .header("Range", "bytes=0-9")
        .send()
        .await
        .unwrap();
    assert_eq!(
        (
            r.status(),
            r.headers()["content-length"].to_str().unwrap().to_string()
        ),
        (StatusCode::PARTIAL_CONTENT, "10".to_string())
    );
    let snap = s.state.metrics.snapshot();
    assert_eq!(
        snap.completed_transfers + snap.aborted_transfers,
        0,
        "HEAD must not register a transfer"
    );
    s.stop().await;
}

#[tokio::test]
async fn inline_is_honoured_only_for_safe_types() {
    let s = TestServer::start(&[]).await;
    std::fs::write(s.root.join("page.html"), "<script>alert(1)</script>").unwrap();
    std::fs::write(s.root.join("pic.png"), b"\x89PNG").unwrap();
    let c = client();
    let r = c
        .get(s.url("/download/page.html?inline=1"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.headers()["content-type"],
        "text/plain; charset=utf-8",
        "HTML is never rendered as HTML"
    );
    assert!(
        r.headers()["content-disposition"]
            .to_str()
            .unwrap()
            .starts_with("inline")
    );
    let r = c
        .get(s.url("/download/pic.png?inline=1"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.headers()["content-type"], "image/png");
    let r = c.get(s.url("/download/page.html")).send().await.unwrap();
    assert!(
        r.headers()["content-disposition"]
            .to_str()
            .unwrap()
            .starts_with("attachment")
    );
    s.stop().await;
}

#[tokio::test]
async fn files_larger_than_4_gib_use_64_bit_offsets() {
    let s = TestServer::start(&[]).await;
    // A sparse file: reports 5 GiB but occupies almost no disk space.
    let size: u64 = 5 * 1024 * 1024 * 1024;
    let f = std::fs::File::create(s.root.join("huge.img")).unwrap();
    f.set_len(size).unwrap();
    let c = client();

    let r = c.head(s.url("/download/huge.img")).send().await.unwrap();
    assert_eq!(
        r.headers()["content-length"].to_str().unwrap(),
        size.to_string()
    );

    let start = 4_294_967_296u64; // 2^32
    let r = c
        .get(s.url("/download/huge.img"))
        .header("Range", format!("bytes={start}-{}", start + 3))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 206);
    assert_eq!(
        r.headers()["content-range"].to_str().unwrap(),
        format!("bytes {start}-{}/{size}", start + 3)
    );
    assert_eq!(r.bytes().await.unwrap().to_vec(), vec![0u8; 4]);

    let r = c
        .get(s.url("/download/huge.img"))
        .header("Range", "bytes=-16")
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.headers()["content-range"].to_str().unwrap(),
        format!("bytes {}-{}/{size}", size - 16, size - 1)
    );

    let r = c
        .get(s.url("/download/huge.img"))
        .header("Range", format!("bytes={size}-"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 416);
    s.stop().await;
}

#[tokio::test]
async fn many_concurrent_downloads_are_all_correct() {
    let s = TestServer::start(&[]).await;
    let c = client();
    let mut tasks = Vec::new();
    for i in 0..10usize {
        let (c, url) = (c.clone(), s.url("/download/big.bin"));
        tasks.push(tokio::spawn(async move {
            // Half the clients request a range, half the whole file.
            if i % 2 == 0 {
                c.get(url)
                    .send()
                    .await
                    .unwrap()
                    .bytes()
                    .await
                    .unwrap()
                    .to_vec()
            } else {
                c.get(url)
                    .header("Range", "bytes=1000-")
                    .send()
                    .await
                    .unwrap()
                    .bytes()
                    .await
                    .unwrap()
                    .to_vec()
            }
        }));
    }
    for (i, t) in tasks.into_iter().enumerate() {
        let body = t.await.unwrap();
        assert_eq!(
            body,
            if i % 2 == 0 {
                expected(0..BIG_LEN)
            } else {
                expected(1000..BIG_LEN)
            },
            "client {i}"
        );
    }
    s.eventually("all transfers settled", |m| {
        m.completed_transfers == 10 && m.active.is_empty()
    })
    .await;
    s.stop().await;
}

#[tokio::test]
async fn client_disconnect_is_recorded_as_aborted_and_server_stays_healthy() {
    let s = TestServer::start(&[]).await;
    // Big enough that the kernel socket buffers cannot swallow it, so back-pressure kicks in.
    let f = std::fs::File::create(s.root.join("large.bin")).unwrap();
    f.set_len(512 * 1024 * 1024).unwrap();

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut sock = tokio::net::TcpStream::connect(s.addr).await.unwrap();
    sock.write_all(b"GET /download/large.bin HTTP/1.1\r\nHost: t\r\n\r\n")
        .await
        .unwrap();
    let mut buf = vec![0u8; 64 * 1024];
    let _ = sock.read(&mut buf).await.unwrap();
    s.eventually("transfer visible while running", |m| m.active.len() == 1)
        .await;
    drop(sock);

    s.eventually("abort recorded", |m| {
        m.aborted_transfers == 1 && m.active.is_empty() && m.active_connections == 0
    })
    .await;
    let snap = s.state.metrics.snapshot();
    assert!(
        snap.bytes_sent < 512 * 1024 * 1024,
        "streaming must stop when the client leaves"
    );
    assert_eq!(
        snap.finished[0].status,
        share::metrics::TransferStatus::Aborted
    );
    // Still serving.
    assert_eq!(
        client()
            .get(s.url("/download/hello.txt"))
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
async fn speed_is_measured_for_an_active_transfer() {
    let s = TestServer::start(&[]).await;
    let f = std::fs::File::create(s.root.join("slow.bin")).unwrap();
    f.set_len(256 * 1024 * 1024).unwrap();
    let r = client()
        .get(s.url("/download/slow.bin"))
        .send()
        .await
        .unwrap();
    let mut stream = r.bytes_stream();
    // Pull data for ~1.2 s so at least two sampler ticks happen.
    let until = tokio::time::Instant::now() + Duration::from_millis(1200);
    while tokio::time::Instant::now() < until {
        stream.next().await.unwrap().unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let snap = s.state.metrics.snapshot();
    assert_eq!(snap.active.len(), 1);
    assert!(
        snap.active[0].speed > 0 && snap.download_speed > 0,
        "{snap:?}"
    );
    assert!(snap.peak_download_speed >= snap.download_speed);
    assert!(snap.active[0].progress().unwrap() > 0.0);
    drop(stream);
    s.stop().await;
}

#[tokio::test]
async fn streaming_folder_archive_zip_tar_gz_and_tar() {
    use std::io::Read;
    let s = TestServer::start(&[]).await;
    let c = client();

    // 1. Streaming .zip via ?format=zip and .zip suffix
    for path in ["/archive/sub?format=zip", "/archive/sub.zip"] {
        let r = c.get(s.url(path)).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::OK, "{path}");
        assert_eq!(r.headers()["content-type"], "application/zip");
        assert!(
            r.headers()["content-disposition"]
                .to_str()
                .unwrap()
                .contains("sub.zip")
        );
        let zip_bytes = r.bytes().await.unwrap();
        assert!(zip_bytes.starts_with(b"PK\x03\x04"), "ZIP local header");
        assert!(
            zip_bytes.windows(4).any(|w| w == b"PK\x01\x02"),
            "ZIP central directory header"
        );
        assert!(
            zip_bytes.windows(4).any(|w| w == b"PK\x05\x06"),
            "ZIP end of central directory"
        );
        let zip_lossy = String::from_utf8_lossy(&zip_bytes);
        assert!(zip_lossy.contains("sub/nested.txt"));
    }

    // 2. Compressed .tar.gz for a subdirectory (both suffix and ?format=tar.gz)
    let r = c
        .get(s.url("/archive/sub?format=tar.gz"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers()["content-type"], "application/gzip");
    assert!(
        r.headers()["content-disposition"]
            .to_str()
            .unwrap()
            .contains("sub.tar.gz")
    );
    let gz_bytes = r.bytes().await.unwrap();
    let mut tar_bytes = Vec::new();
    flate2::read::GzDecoder::new(&gz_bytes[..])
        .read_to_end(&mut tar_bytes)
        .unwrap();
    let tar_str = String::from_utf8_lossy(&tar_bytes);
    assert!(tar_str.contains("sub/nested.txt"));
    assert!(tar_str.contains("nested"));

    // 3. Uncompressed .tar for a subdirectory (both ?format=tar and .tar suffix)
    let r = c
        .get(s.url("/archive/sub?format=tar"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers()["content-type"], "application/x-tar");
    let raw_tar = r.bytes().await.unwrap();
    assert_eq!(raw_tar.as_ref(), tar_bytes.as_slice());

    // 4. Root archive excludes hidden files by default
    let r = c.get(s.url("/archive")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let root_gz = r.bytes().await.unwrap();
    let mut root_tar = Vec::new();
    flate2::read::GzDecoder::new(&root_gz[..])
        .read_to_end(&mut root_tar)
        .unwrap();
    let root_str = String::from_utf8_lossy(&root_tar);
    assert!(root_str.contains("hello.txt"));
    assert!(!root_str.contains(".secret"));

    s.stop().await;
}

#[tokio::test]
async fn rate_limit_throttles_download_throughput() {
    let s = TestServer::start(&["--rate-limit", "1M"]).await;
    let data = vec![b'a'; 768 * 1024];
    std::fs::write(s.root.join("paced.bin"), &data).unwrap();

    let start = tokio::time::Instant::now();
    let body = client()
        .get(s.url("/download/paced.bin"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let elapsed = start.elapsed();
    assert_eq!(body.len(), data.len());
    // 768 KiB at 1 MiB/s with 256 KiB initial burst leaves 512 KiB (~0.5s).
    assert!(
        elapsed >= Duration::from_millis(350),
        "expected throttled transfer to take >= 350ms, took {elapsed:?}"
    );
    s.stop().await;
}

#[tokio::test]
async fn operator_can_kill_active_download_transfer() {
    let s = TestServer::start(&[]).await;
    let f = std::fs::File::create(s.root.join("killme.bin")).unwrap();
    f.set_len(512 * 1024 * 1024).unwrap();

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut sock = tokio::net::TcpStream::connect(s.addr).await.unwrap();
    sock.write_all(b"GET /download/killme.bin HTTP/1.1\r\nHost: t\r\n\r\n")
        .await
        .unwrap();
    let mut buf = vec![0u8; 64 * 1024];
    let _ = sock.read(&mut buf).await.unwrap();
    s.eventually("transfer active", |m| m.active.len() == 1)
        .await;

    let id = s.state.metrics.snapshot().active[0].id;
    assert!(s.state.metrics.registry.cancel(id).is_some());

    s.eventually("killed transfer recorded as aborted", |m| {
        m.aborted_transfers == 1 && m.active.is_empty()
    })
    .await;
    s.stop().await;
}
