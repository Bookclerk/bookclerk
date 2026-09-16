//! AWS SDK HTTP through the workerd socket proxy.
//!
//! Nested `NetPolicy::Deny` guests cannot dial `AF_INET`. When
//! [`bookclerk_plugin_sdk::SOCKET_PROXY_ENV`] is set, the S3 client uses the
//! same CONNECT + rustls path as [`bookclerk_plugin_sdk::http::Client`].

#![allow(clippy::missing_docs_in_private_items)]

use std::fmt;

use aws_sdk_s3::config::SharedHttpClient;
use aws_smithy_runtime_api::client::http::{
    http_client_fn, HttpConnector, HttpConnectorFuture, SharedHttpConnector,
};
use aws_smithy_runtime_api::client::orchestrator::{HttpRequest, HttpResponse};
use aws_smithy_runtime_api::client::result::ConnectorError;
use aws_smithy_types::body::SdkBody;
use bookclerk_plugin_sdk::http::{header, Client, HeaderMap, HeaderValue, Method};
use bookclerk_plugin_sdk::SOCKET_PROXY_ENV;
use bytes::Bytes;
use http_body_util::BodyExt;

/// True when the guest must speak S3 through the workerd CONNECT proxy.
#[must_use]
pub(crate) fn socket_proxy_enabled() -> bool {
    std::env::var_os(SOCKET_PROXY_ENV).is_some()
}

/// Smithy HTTP client that dials via [`bookclerk_plugin_sdk::http::Client`].
pub(crate) fn socket_proxy_http_client() -> Result<SharedHttpClient, crate::StorageError> {
    let http = Client::builder()
        .build()
        .map_err(|err| crate::StorageError::S3(format!("s3 socket-proxy http client: {err}")))?;
    let connector = SdkSocketConnector { http };
    Ok(http_client_fn(move |_settings, _components| {
        SharedHttpConnector::new(connector.clone())
    }))
}

#[derive(Clone)]
struct SdkSocketConnector {
    http: Client,
}

impl fmt::Debug for SdkSocketConnector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SdkSocketConnector").finish_non_exhaustive()
    }
}

impl HttpConnector for SdkSocketConnector {
    fn call(&self, request: HttpRequest) -> HttpConnectorFuture {
        let http = self.http.clone();
        HttpConnectorFuture::new(async move { dispatch(http, request).await })
    }
}

async fn dispatch(http: Client, request: HttpRequest) -> Result<HttpResponse, ConnectorError> {
    let method = Method::from_bytes(request.method().as_bytes())
        .map_err(|err| ConnectorError::other(err.into(), None))?;
    let uri = request.uri().to_string();
    let mut headers = HeaderMap::new();
    for (name, value) in request.headers() {
        if skip_request_header(name) {
            continue;
        }
        let Ok(k) = header::HeaderName::try_from(name) else {
            continue;
        };
        let Ok(v) = HeaderValue::try_from(value) else {
            continue;
        };
        headers.append(k, v);
    }
    let body = collect_body(request.into_body()).await?;
    let mut builder = http.request(method, uri).headers(headers);
    if !body.is_empty() {
        builder = builder.body(body.to_vec());
    }
    let response = builder
        .send()
        .await
        .map_err(|err| ConnectorError::io(err.into()))?;
    let status = response
        .status()
        .as_u16()
        .try_into()
        .map_err(|err| ConnectorError::other(Box::new(err), None))?;
    let headers = response.headers().clone();
    let body = response
        .bytes()
        .await
        .map_err(|err| ConnectorError::io(err.into()))?;
    let mut out = HttpResponse::new(status, SdkBody::from(body));
    for (name, value) in &headers {
        let Ok(v) = value.to_str() else {
            continue;
        };
        let _ = out
            .headers_mut()
            .try_insert(name.as_str().to_owned(), v.to_owned());
    }
    Ok(out)
}

fn skip_request_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection" | "proxy-connection" | "transfer-encoding" | "content-length"
    )
}

async fn collect_body(body: SdkBody) -> Result<Bytes, ConnectorError> {
    if let Some(bytes) = body.bytes() {
        return Ok(Bytes::copy_from_slice(bytes));
    }
    let collected = BodyExt::collect(body).await.map_err(ConnectorError::io)?;
    Ok(collected.to_bytes())
}

#[cfg(all(test, unix))]
#[allow(clippy::missing_panics_doc, clippy::await_holding_lock)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    static SOCKET_PROXY_ENV_LOCK: Mutex<()> = Mutex::new(());

    #[tokio::test]
    async fn socket_proxy_http_client_get_through_fake_connect() {
        let _env = SOCKET_PROXY_ENV_LOCK.lock().expect("env lock");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s3.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0_u8; 2048];
            let n = stream.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]);
            assert!(req.contains("CONNECT example.test:80"), "{req}");
            stream
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .unwrap();
            let mut rest = vec![0_u8; 2048];
            let n = stream.read(&mut rest).await.unwrap();
            let http = String::from_utf8_lossy(&rest[..n]);
            assert!(http.starts_with("GET /bucket"), "{http}");
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nx\n")
                .await
                .unwrap();
        });
        let _guard = std::env::var_os(SOCKET_PROXY_ENV);
        std::env::set_var(SOCKET_PROXY_ENV, &path);
        let client = Client::builder().build().unwrap();
        let connector = SdkSocketConnector { http: client };
        let mut req = HttpRequest::new(SdkBody::empty());
        req.set_method("GET").unwrap();
        req.set_uri("http://example.test/bucket").unwrap();
        let resp = connector.call(req).await.expect("proxied s3 get");
        assert!(resp.status().is_success());
        std::env::remove_var(SOCKET_PROXY_ENV);
        if let Some(prev) = _guard {
            std::env::set_var(SOCKET_PROXY_ENV, prev);
        }
        server.await.unwrap();
    }
}
