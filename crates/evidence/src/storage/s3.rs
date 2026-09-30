use super::{MAX_PRIVATE_OBJECT_BYTES, PrivateStorage, validate_key};
use crate::{Error, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use object_store::{
    ClientOptions, ObjectStore, ObjectStoreExt, PutMode, RetryConfig,
    aws::{AmazonS3, AmazonS3Builder, S3ConditionalPut},
    path::Path,
};
use std::time::Duration;
use url::Url;

const OPERATION_TIMEOUT: Duration = Duration::from_secs(45);

/// The SDK signs private requests. Neither credentials nor signed URLs leave this adapter.
pub struct S3PrivateStorage {
    client: AmazonS3,
    max_bytes: usize,
    timeout: Duration,
}

impl S3PrivateStorage {
    pub(super) fn from_config(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let required = |name| {
            get(name)
                .filter(|value| !value.trim().is_empty())
                .ok_or(Error::Invalid(name))
        };
        // Use static error descriptions; never echo supplied credentials/config values.
        let bucket = required("S3_BUCKET")?;
        let endpoint = required("S3_ENDPOINT")?;
        let region = required("S3_REGION")?;
        let access_key = required("AWS_ACCESS_KEY_ID")?;
        let secret_key = required("AWS_SECRET_ACCESS_KEY")?;
        let path_style = match get("S3_FORCE_PATH_STYLE").as_deref() {
            None | Some("false") => false,
            Some("true") => true,
            _ => return Err(Error::Invalid("S3_FORCE_PATH_STYLE must be true or false")),
        };
        let endpoint = endpoint_url(&endpoint, &bucket, path_style)?;
        let allow_http = endpoint.scheme() == "http";
        let client = AmazonS3Builder::new()
            .with_endpoint(endpoint.as_str().trim_end_matches('/'))
            .with_bucket_name(bucket)
            .with_region(region)
            .with_access_key_id(access_key)
            .with_secret_access_key(secret_key)
            .with_virtual_hosted_style_request(!path_style)
            .with_conditional_put(S3ConditionalPut::ETagMatch)
            .with_disable_bulk_delete(true)
            .with_client_options(
                ClientOptions::new()
                    .with_allow_http(allow_http)
                    .with_connect_timeout(Duration::from_secs(5))
                    .with_timeout(Duration::from_secs(25)),
            )
            .with_retry(RetryConfig {
                max_retries: 2,
                retry_timeout: Duration::from_secs(35),
                ..Default::default()
            })
            .build()
            .map_err(|_| Error::Invalid("S3 configuration"))?;
        Ok(Self {
            client,
            max_bytes: MAX_PRIVATE_OBJECT_BYTES,
            timeout: OPERATION_TIMEOUT,
        })
    }
}

fn endpoint_url(raw: &str, bucket: &str, path_style: bool) -> Result<Url> {
    if bucket.len() < 3
        || bucket.len() > 63
        || !bucket
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        || bucket.starts_with('-')
        || bucket.ends_with('-')
    {
        return Err(Error::Invalid(
            "S3_BUCKET must be a DNS-compatible bucket name",
        ));
    }
    let mut endpoint = Url::parse(raw).map_err(|_| Error::Invalid("S3_ENDPOINT"))?;
    let host = endpoint
        .host_str()
        .ok_or(Error::Invalid("S3_ENDPOINT host"))?;
    let local = matches!(host, "127.0.0.1" | "[::1]");
    if (endpoint.scheme() != "https" && !(endpoint.scheme() == "http" && local && path_style))
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
        || endpoint.path() != "/"
    {
        return Err(Error::Invalid(
            "S3_ENDPOINT must be an HTTPS origin; loopback HTTP requires path style",
        ));
    }
    if !path_style && !host.starts_with(&format!("{bucket}.")) {
        let host = format!("{bucket}.{host}");
        endpoint
            .set_host(Some(&host))
            .map_err(|_| Error::Invalid("S3_ENDPOINT host"))?;
    }
    Ok(endpoint)
}

#[async_trait]
impl PrivateStorage for S3PrivateStorage {
    async fn put(&self, key: &str, bytes: &[u8]) -> Result<()> {
        validate_key(key)?;
        if bytes.len() > self.max_bytes {
            return Err(Error::Invalid("private object size limit"));
        }
        // Atomic If-None-Match: *. Never fall back to an unconditional overwrite.
        tokio::time::timeout(
            self.timeout,
            self.client.put_opts(
                &Path::from(key),
                bytes.to_vec().into(),
                PutMode::Create.into(),
            ),
        )
        .await
        .map_err(|_| Error::Storage)?
        .map_err(|_| Error::Storage)?;
        Ok(())
    }

    async fn read(&self, key: &str) -> Result<Vec<u8>> {
        validate_key(key)?;
        tokio::time::timeout(self.timeout, async {
            let result = self
                .client
                .get(&Path::from(key))
                .await
                .map_err(|_| Error::Storage)?;
            if result.meta.size > self.max_bytes as u64 {
                return Err(Error::Invalid("private object size limit"));
            }
            collect_bounded(result.into_stream(), self.max_bytes).await
        })
        .await
        .map_err(|_| Error::Storage)?
    }

    async fn delete(&self, key: &str) -> Result<()> {
        validate_key(key)?;
        match tokio::time::timeout(self.timeout, self.client.delete(&Path::from(key)))
            .await
            .map_err(|_| Error::Storage)?
        {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(_) => Err(Error::Storage),
        }
    }
}

async fn collect_bounded<T: AsRef<[u8]>>(
    mut stream: impl futures_util::Stream<Item = object_store::Result<T>> + Unpin,
    cap: usize,
) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| Error::Storage)?;
        let chunk = chunk.as_ref();
        if bytes.len().saturating_add(chunk.len()) > cap {
            return Err(Error::Invalid("private object size limit"));
        }
        bytes.extend_from_slice(chunk);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests;
