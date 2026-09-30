use super::*;
use std::collections::HashMap;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

fn config(endpoint: &str) -> HashMap<String, String> {
    [
        ("S3_ENDPOINT", endpoint),
        ("S3_BUCKET", "test-bucket"),
        ("S3_REGION", "auto"),
        ("AWS_ACCESS_KEY_ID", "synthetic-access"),
        ("AWS_SECRET_ACCESS_KEY", "synthetic-secret"),
        ("S3_FORCE_PATH_STYLE", "true"),
    ]
    .into_iter()
    .map(|(k, v)| (k.into(), v.into()))
    .collect()
}

#[test]
fn virtual_hosted_and_path_endpoints_are_explicit_and_validated() {
    assert_eq!(
        endpoint_url("https://storage.example", "test-bucket", false)
            .unwrap()
            .as_str(),
        "https://test-bucket.storage.example/"
    );
    assert_eq!(
        endpoint_url("https://test-bucket.storage.example", "test-bucket", false)
            .unwrap()
            .as_str(),
        "https://test-bucket.storage.example/"
    );
    assert_eq!(
        endpoint_url("https://storage.example", "test-bucket", true)
            .unwrap()
            .as_str(),
        "https://storage.example/"
    );
    for endpoint in [
        "http://storage.example",
        "https://user:secret@storage.example",
        "https://storage.example/path",
        "https://storage.example?token=secret",
        "https://storage.example#fragment",
    ] {
        assert!(endpoint_url(endpoint, "test-bucket", true).is_err());
    }
    assert!(endpoint_url("http://127.0.0.1:9000", "test-bucket", true).is_ok());
    assert!(endpoint_url("http://127.0.0.1:9000", "test-bucket", false).is_err());
    assert!(endpoint_url("https://storage.example", "../bad", true).is_err());
}

#[test]
fn missing_credentials_and_invalid_configuration_fail_closed_without_values() {
    for field in [
        "S3_ENDPOINT",
        "S3_BUCKET",
        "S3_REGION",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
    ] {
        let mut values = config("https://storage.example");
        values.remove(field);
        assert!(S3PrivateStorage::from_config(|k| values.get(k).cloned()).is_err());
    }
    let mut values = config("https://storage.example");
    values.insert("S3_FORCE_PATH_STYLE".into(), "invalid-secret-value".into());
    let error = S3PrivateStorage::from_config(|k| values.get(k).cloned())
        .err()
        .unwrap();
    assert!(!format!("{error:?}").contains("invalid-secret-value"));
}

struct Reply {
    method: &'static str,
    key: &'static str,
    status: &'static str,
    body: &'static str,
    extra_headers: &'static str,
}

/// Only synthetic data and loopback TCP. Verifies actual signed SDK HTTP requests.
async fn server(replies: Vec<Reply>) -> (S3PrivateStorage, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let values = config(&format!("http://{}", listener.local_addr().unwrap()));
    let store = S3PrivateStorage::from_config(|k| values.get(k).cloned()).unwrap();
    let task = tokio::spawn(async move {
        for reply in replies {
            let (mut stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut received = Vec::new();
            let header_end = loop {
                let mut buffer = [0; 1024];
                let n = stream.read(&mut buffer).await.unwrap();
                assert!(n > 0);
                received.extend_from_slice(&buffer[..n]);
                if let Some(end) = received.windows(4).position(|w| w == b"\r\n\r\n") {
                    break end + 4;
                }
            };
            let headers = String::from_utf8_lossy(&received[..header_end]).to_lowercase();
            assert!(
                headers.starts_with(&format!(
                    "{} /test-bucket/{} ",
                    reply.method.to_lowercase(),
                    reply.key
                )),
                "unexpected request"
            );
            assert!(headers.contains("authorization: aws4-hmac-sha256 "));
            assert!(headers.contains("x-amz-date:"));
            assert!(!headers.contains("synthetic-secret"));
            if reply.method == "PUT" {
                assert!(
                    headers.contains("if-none-match: *"),
                    "immutable write missing precondition"
                );
                let len: usize = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .unwrap()
                    .parse()
                    .unwrap();
                while received.len() < header_end + len {
                    let mut buffer = [0; 1024];
                    let n = stream.read(&mut buffer).await.unwrap();
                    assert!(n > 0);
                    received.extend_from_slice(&buffer[..n]);
                }
            }
            let response = format!(
                "HTTP/1.1 {}\r\nConnection: close\r\nContent-Length: {}\r\nETag: \"fixture\"\r\nLast-Modified: Wed, 30 Sep 2026 00:00:00 GMT\r\n{}\r\n{}",
                reply.status,
                reply.body.len(),
                reply.extra_headers,
                reply.body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        }
    });
    (store, task)
}

fn reply(
    method: &'static str,
    key: &'static str,
    status: &'static str,
    body: &'static str,
) -> Reply {
    Reply {
        method,
        key,
        status,
        body,
        extra_headers: "",
    }
}

#[tokio::test]
async fn signed_create_read_and_idempotent_delete_preserve_immutable_objects() {
    let (store, server) = server(vec![
        reply("PUT", "source.wav", "200 OK", ""),
        reply(
            "PUT",
            "source.wav",
            "412 Precondition Failed",
            "<Error><Code>PreconditionFailed</Code></Error>",
        ),
        reply(
            "GET",
            "source.wav",
            "200 OK",
            "original synthetic recording",
        ),
        reply("DELETE", "source.wav", "204 No Content", ""),
        reply(
            "DELETE",
            "source.wav",
            "404 Not Found",
            "<Error><Code>NoSuchKey</Code></Error>",
        ),
        reply(
            "GET",
            "source.wav",
            "404 Not Found",
            "<Error><Code>NoSuchKey</Code></Error>",
        ),
    ])
    .await;
    store
        .put("source.wav", b"original synthetic recording")
        .await
        .unwrap();
    assert!(
        store
            .put("source.wav", b"replacement must fail")
            .await
            .is_err()
    );
    assert_eq!(
        store.read("source.wav").await.unwrap(),
        b"original synthetic recording"
    );
    store.delete("source.wav").await.unwrap();
    store.delete("source.wav").await.unwrap();
    assert!(store.read("source.wav").await.is_err());
    server.await.unwrap();
}

#[tokio::test]
async fn reads_and_writes_are_bounded_and_failures_do_not_expose_provider_body() {
    let (mut store, server) = server(vec![
        reply("GET", "large.wav", "200 OK", "too large"),
        reply("GET", "private.wav", "403 Forbidden", "<Error><Code>AccessDenied</Code><Message>private credential material</Message></Error>"),
    ]).await;
    store.max_bytes = 4;
    assert!(matches!(
        store.put("large.wav", b"too large").await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        store.read("large.wav").await,
        Err(Error::Invalid(_))
    ));
    let error = store.read("private.wav").await.unwrap_err();
    assert_eq!(format!("{error:?}"), "Storage");
    server.await.unwrap();
}

#[tokio::test]
async fn transient_failures_retry_but_unsupported_conditional_writes_never_fall_back() {
    let (store, server) = server(vec![
        reply(
            "GET",
            "fixture.wav",
            "503 Service Unavailable",
            "<Error><Code>SlowDown</Code></Error>",
        ),
        reply("GET", "fixture.wav", "200 OK", "synthetic"),
        reply(
            "PUT",
            "new.wav",
            "400 Bad Request",
            "<Error><Code>NotImplemented</Code></Error>",
        ),
    ])
    .await;
    assert_eq!(store.read("fixture.wav").await.unwrap(), b"synthetic");
    assert!(store.put("new.wav", b"synthetic").await.is_err());
    server.await.unwrap();
}

#[tokio::test]
async fn invalid_keys_fail_before_any_request_and_unknown_backend_fails_closed() {
    let values = config("http://127.0.0.1:1");
    let store = S3PrivateStorage::from_config(|k| values.get(k).cloned()).unwrap();
    for key in [
        "",
        "../file",
        "folder/file",
        ".hidden",
        "file?secret",
        "file%2Fname",
    ] {
        assert!(matches!(store.put(key, b"a").await, Err(Error::Invalid(_))));
        assert!(matches!(store.read(key).await, Err(Error::Invalid(_))));
        assert!(matches!(store.delete(key).await, Err(Error::Invalid(_))));
    }
    assert!(
        super::super::storage_from_config(
            |k| (k == "EVIDENCE_STORAGE_BACKEND").then(|| "unknown".into())
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn factory_preserves_local_default_create_only_and_delete_semantics() {
    let path = std::env::temp_dir().join(format!("v0-storage-{}", uuid::Uuid::new_v4()));
    let store = super::super::storage_from_config(|k| {
        (k == "EVIDENCE_STORAGE_DIR").then(|| path.to_string_lossy().into_owned())
    })
    .await
    .unwrap();
    store.put("fixture.wav", b"synthetic").await.unwrap();
    assert!(store.put("fixture.wav", b"replace").await.is_err());
    assert_eq!(store.read("fixture.wav").await.unwrap(), b"synthetic");
    store.delete("fixture.wav").await.unwrap();
    store.delete("fixture.wav").await.unwrap();
    tokio::fs::remove_dir_all(path).await.unwrap();
}

#[tokio::test]
async fn streaming_limit_is_enforced_independently_of_reported_length() {
    let chunks = futures_util::stream::iter([Ok(b"1234".as_slice()), Ok(b"5".as_slice())]);
    assert!(matches!(
        collect_bounded(chunks, 4).await,
        Err(Error::Invalid(_))
    ));
    let chunks = futures_util::stream::iter([Ok(b"12".as_slice()), Ok(b"34".as_slice())]);
    assert_eq!(collect_bounded(chunks, 4).await.unwrap(), b"1234");
}

#[tokio::test]
async fn operation_deadline_bounds_an_unresponsive_server() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let values = config(&format!("http://{}", listener.local_addr().unwrap()));
    let mut store = S3PrivateStorage::from_config(|k| values.get(k).cloned()).unwrap();
    store.timeout = Duration::from_millis(30);
    let task = tokio::spawn(async move {
        let (_stream, _) = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
    });
    assert!(matches!(
        store.read("fixture.wav").await,
        Err(Error::Storage)
    ));
    task.abort();
}

#[tokio::test]
#[ignore = "writes and deletes a synthetic object in explicitly configured S3 storage"]
async fn live_s3_private_storage_round_trip() {
    // Requires process environment, deliberately does not load a developer's .env.
    assert_eq!(
        std::env::var("EVIDENCE_STORAGE_BACKEND").as_deref(),
        Ok("s3")
    );
    let store = S3PrivateStorage::from_config(|k| std::env::var(k).ok()).unwrap();
    let key = format!("deployment-probe-{}.txt", uuid::Uuid::new_v4());
    let result: Result<()> = async {
        store
            .put(&key, b"synthetic deployment storage probe")
            .await?;
        if store.put(&key, b"must not overwrite").await.is_ok() {
            return Err(Error::Invalid(
                "S3 provider did not enforce immutable creation",
            ));
        }
        if store.read(&key).await? != b"synthetic deployment storage probe" {
            return Err(Error::Invalid("S3 stored bytes differ"));
        }
        Ok(())
    }
    .await;
    let cleanup = store.delete(&key).await;
    result.unwrap();
    cleanup.unwrap();
    store.delete(&key).await.unwrap();
    assert!(store.read(&key).await.is_err());
}
