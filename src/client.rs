use bytes::Bytes;
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use reqwest::{
    header::{self, HeaderMap, HeaderValue},
    Client, ClientBuilder, StatusCode,
};
use std::time::Duration;

use crate::error::{SbError, SbResult};

/// Characters to percent-encode in URL path segments.
///
/// Starts from the CONTROLS base set and adds all characters that have special
/// meaning in URLs but are valid in SilverBullet page names. The `/` character
/// is intentionally excluded so that path separators are preserved.
const PATH_SEGMENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'!')
    .add(b'"')
    .add(b'#')
    .add(b'$')
    .add(b'%')
    .add(b'&')
    .add(b'\'')
    .add(b'(')
    .add(b')')
    .add(b'*')
    .add(b'+')
    .add(b',')
    .add(b':')
    .add(b';')
    .add(b'<')
    .add(b'=')
    .add(b'>')
    .add(b'?')
    .add(b'@')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

/// Percent-encode a file path for use in a URL, preserving `/` separators.
fn encode_path(path: &str) -> String {
    utf8_percent_encode(path, PATH_SEGMENT).to_string()
}

/// File metadata returned by GET /.fs listing.
///
/// Timestamps are Unix milliseconds (i64) — never convert to seconds.
/// `perm` is "rw" or "ro"; absent on some server versions.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileMeta {
    pub name: String,
    pub last_modified: i64,
    pub created: i64,
    pub content_type: String,
    pub size: u64,
    #[serde(default)]
    pub perm: Option<String>,
}

/// Server configuration returned by GET /.config
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerConfig {
    pub read_only: bool,
    pub space_folder_path: String,
    pub index_page: String,
    #[serde(default)]
    pub log_push: bool,
    #[serde(default)]
    pub enable_client_encryption: bool,
}

/// HTTP client wrapper for SilverBullet API.
///
/// Bakes `X-Sync-Mode: true` and `Authorization: Bearer <token>` into every
/// request via reqwest default headers.
#[derive(Clone)]
pub struct SbClient {
    inner: Client,
    base_url: String,
}

impl SbClient {
    /// Create a new SbClient.
    ///
    /// - `base_url`: SilverBullet server root (trailing slash stripped).
    /// - `token`: Bearer token. Pass empty string for no-auth servers.
    pub fn new(base_url: &str, token: &str) -> SbResult<Self> {
        let mut headers = HeaderMap::new();

        // X-Sync-Mode: true on every request
        headers.insert("X-Sync-Mode", HeaderValue::from_static("true"));

        // Authorization: Bearer <token> — skip if token is empty
        if !token.is_empty() {
            let auth_value =
                HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| SbError::Config {
                    message: "auth token contains invalid header characters".into(),
                })?;
            headers.insert(header::AUTHORIZATION, auth_value);
        }

        let client = ClientBuilder::new()
            .default_headers(headers)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| SbError::Config {
                message: format!("failed to build HTTP client: {e}"),
            })?;

        Ok(Self {
            inner: client,
            base_url: base_url.trim_end_matches('/').to_string(),
        })
    }

    /// Return the base URL this client is configured for.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// GET `/.ping` — returns round-trip duration.
    pub async fn ping(&self) -> SbResult<Duration> {
        let url = format!("{}/.ping", self.base_url);
        let start = std::time::Instant::now();
        let resp = self.send_with_retry(&url, || self.inner.get(&url)).await?;
        self.check_status(resp.status(), &url)?;
        Ok(start.elapsed())
    }

    /// GET `/.config` — returns deserialized server configuration.
    pub async fn get_config(&self) -> SbResult<ServerConfig> {
        let url = format!("{}/.config", self.base_url);
        let resp = self.send_with_retry(&url, || self.inner.get(&url)).await?;
        let resp = Self::check_response(resp, &url).await?;
        let status = resp.status();
        resp.json::<ServerConfig>()
            .await
            .map_err(|e| SbError::HttpStatus {
                status: status.as_u16(),
                url,
                body: format!("failed to parse server config: {e}"),
            })
    }

    /// GET `/.fs/<name>.md` — fetch a page's content from the server.
    ///
    /// Returns the page content as a `String`.
    /// Returns `SbError::PageNotFound` if the server returns 404.
    /// Returns `SbError::AuthFailed` if the server returns 401 or 403.
    pub async fn get_page(&self, name: &str) -> SbResult<String> {
        let url = format!("{}/.fs/{}.md", self.base_url, encode_path(name));
        let resp = self.send_with_retry(&url, || self.inner.get(&url)).await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Err(SbError::PageNotFound {
                name: name.to_string(),
            });
        }
        let resp = Self::check_response(resp, &url).await?;
        let status = resp.status();
        resp.text().await.map_err(|e| SbError::HttpStatus {
            status: status.as_u16(),
            url,
            body: format!("failed to read response body: {e}"),
        })
    }

    /// GET `/.fs` — list all files on the server.
    ///
    /// Returns a `Vec<FileMeta>` with name, timestamps, size, and permissions.
    pub async fn list_files(&self) -> SbResult<Vec<FileMeta>> {
        let url = format!("{}/.fs", self.base_url);
        let resp = self.send_with_retry(&url, || self.inner.get(&url)).await?;
        let resp = Self::check_response(resp, &url).await?;
        let status = resp.status();
        resp.json::<Vec<FileMeta>>()
            .await
            .map_err(|e| SbError::HttpStatus {
                status: status.as_u16(),
                url,
                body: format!("failed to parse file listing: {e}"),
            })
    }

    /// GET `/.fs/<path>` — download raw file content.
    ///
    /// Returns `SbError::PageNotFound` on 404.
    pub async fn get_file(&self, path: &str) -> SbResult<Bytes> {
        let url = format!("{}/.fs/{}", self.base_url, encode_path(path));
        let resp = self.send_with_retry(&url, || self.inner.get(&url)).await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Err(SbError::PageNotFound {
                name: path.to_string(),
            });
        }
        let resp = Self::check_response(resp, &url).await?;
        let status = resp.status();
        resp.bytes().await.map_err(|e| SbError::HttpStatus {
            status: status.as_u16(),
            url,
            body: format!("failed to read file content: {e}"),
        })
    }

    /// GET `/.fs/<path>` with `X-Get-Meta: true` — fetch only file metadata.
    ///
    /// Returns the `X-Last-Modified` header value as Unix milliseconds (i64).
    /// Returns `0` when the header is absent.
    /// Returns `SbError::PageNotFound` on 404.
    pub async fn get_file_meta(&self, path: &str) -> SbResult<i64> {
        let url = format!("{}/.fs/{}", self.base_url, encode_path(path));
        let resp = self
            .send_with_retry(&url, || self.inner.get(&url).header("X-Get-Meta", "true"))
            .await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Err(SbError::PageNotFound {
                name: path.to_string(),
            });
        }
        let resp = Self::check_response(resp, &url).await?;
        let mtime = resp
            .headers()
            .get("X-Last-Modified")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        Ok(mtime)
    }

    /// PUT `/.fs/<path>` — upload file content to the server.
    ///
    /// Sets `Content-Type: text/markdown` for `.md` files,
    /// `application/octet-stream` otherwise.
    pub async fn put_file(&self, path: &str, content: bytes::Bytes) -> SbResult<()> {
        let url = format!("{}/.fs/{}", self.base_url, encode_path(path));
        let content_type = if path.ends_with(".md") {
            "text/markdown"
        } else {
            "application/octet-stream"
        };
        let resp = self
            .send_with_retry(&url, || {
                self.inner
                    .put(&url)
                    .header(header::CONTENT_TYPE, content_type)
                    .body(content.clone())
            })
            .await?;
        Self::check_response(resp, &url).await?;
        Ok(())
    }

    /// DELETE `/.fs/<path>` — delete a file from the server.
    ///
    /// Returns `SbError::PageNotFound` on 404.
    pub async fn delete_file(&self, path: &str) -> SbResult<()> {
        let url = format!("{}/.fs/{}", self.base_url, encode_path(path));
        let resp = self
            .send_with_retry(&url, || self.inner.delete(&url))
            .await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Err(SbError::PageNotFound {
                name: path.to_string(),
            });
        }
        Self::check_response(resp, &url).await?;
        Ok(())
    }

    /// POST `<endpoint>` with a `text/plain` body.
    ///
    /// The `endpoint` is a path like `/.runtime/lua` (NOT a full URL).
    /// Returns the raw Response so callers can inspect status and body.
    /// Uses `send_with_retry` internally for timeout resilience.
    pub async fn post_text(&self, endpoint: &str, body: &str) -> SbResult<reqwest::Response> {
        let url = format!("{}{}", self.base_url, endpoint);
        let body_bytes = bytes::Bytes::from(body.to_string());
        self.send_with_retry(&url, || {
            self.inner
                .post(&url)
                .header(header::CONTENT_TYPE, "text/plain")
                .body(body_bytes.clone())
        })
        .await
    }

    /// POST `<endpoint>` with a JSON body (e.g., "/.shell").
    ///
    /// The `endpoint` is a path like `/.shell` (NOT a full URL).
    /// Returns the raw Response so callers can inspect status and body.
    pub async fn post_json<T: serde::Serialize>(
        &self,
        endpoint: &str,
        body: &T,
    ) -> SbResult<reqwest::Response> {
        let url = format!("{}{}", self.base_url, endpoint);
        let json_bytes =
            bytes::Bytes::from(serde_json::to_vec(body).map_err(|e| SbError::Config {
                message: format!("failed to serialize request body: {e}"),
            })?);
        self.send_with_retry(&url, || {
            self.inner
                .post(&url)
                .header(header::CONTENT_TYPE, "application/json")
                .body(json_bytes.clone())
        })
        .await
    }

    /// Probe Runtime API availability by sending an empty POST to `/.runtime/lua`.
    ///
    /// - Returns `Ok(true)` for any status except 503 SERVICE_UNAVAILABLE
    ///   (400 "empty body" means the endpoint exists and is active).
    /// - Returns `Ok(false)` when status is 503 (Runtime API not running).
    /// - Network errors propagate as `Err`.
    ///
    /// NOTE: Does NOT use `send_with_retry` because 503 is the expected "unavailable"
    /// signal and should not be retried (best-effort, never blocks caller).
    pub async fn probe_runtime_api(&self) -> SbResult<bool> {
        let url = format!("{}/.runtime/lua", self.base_url);
        let resp = self
            .inner
            .post(&url)
            .header(header::CONTENT_TYPE, "text/plain")
            .body("")
            .send()
            .await
            .map_err(|e| SbError::Network {
                url: url.clone(),
                source: Box::new(e),
            })?;
        if resp.status() == StatusCode::SERVICE_UNAVAILABLE {
            return Ok(false);
        }
        Ok(true)
    }

    /// Retry transient HTTP failures (5xx, timeouts) with exponential backoff.
    ///
    /// Closure pattern because reqwest::RequestBuilder is not Clone (Pitfall 1).
    /// Retries up to 3 times with delays: 1s, 2s, 4s.
    /// Non-retryable errors (4xx, connection refused) propagate immediately.
    async fn send_with_retry(
        &self,
        url: &str,
        build: impl Fn() -> reqwest::RequestBuilder,
    ) -> SbResult<reqwest::Response> {
        let max_retries = 3u32;
        let mut last_err: Option<SbError> = None;
        for attempt in 0..=max_retries {
            if attempt > 0 {
                let secs = 1u64 << (attempt - 1); // 1, 2, 4
                tracing::warn!(
                    attempt,
                    delay_secs = secs,
                    url,
                    "retrying transient HTTP error"
                );
                tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
            }
            match build().send().await {
                Err(e) if e.is_timeout() => {
                    last_err = Some(SbError::Network {
                        url: url.to_string(),
                        source: Box::new(e),
                    });
                    continue;
                }
                Err(e) => {
                    // Non-timeout network error (connection refused, DNS) — not retryable
                    return Err(SbError::Network {
                        url: url.to_string(),
                        source: Box::new(e),
                    });
                }
                Ok(resp) if resp.status().is_server_error() => {
                    let status = resp.status().as_u16();
                    last_err = Some(SbError::HttpStatus {
                        status,
                        url: url.to_string(),
                        body: format!("server error (attempt {})", attempt + 1),
                    });
                    continue;
                }
                Ok(resp) => return Ok(resp),
            }
        }
        tracing::warn!(url, "all {} retry attempts exhausted", max_retries);
        Err(last_err.expect("loop always sets last_err before reaching here"))
    }

    /// Check a response's status, consuming it on error and returning it on success.
    ///
    /// On 401/403, returns `SbError::AuthFailed`.
    /// On any other non-2xx, reads the body and returns `SbError::HttpStatus`.
    /// On 2xx, returns the response so the caller can read its body.
    async fn check_response(resp: reqwest::Response, url: &str) -> SbResult<reqwest::Response> {
        let status = resp.status();
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(SbError::AuthFailed {
                url: url.to_string(),
                status: status.as_u16(),
            });
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(SbError::HttpStatus {
                status: status.as_u16(),
                url: url.to_string(),
                body,
            });
        }
        Ok(resp)
    }

    /// Map an HTTP status to `SbError`. Used internally after sending requests.
    fn check_status(&self, status: StatusCode, url: &str) -> SbResult<()> {
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(SbError::AuthFailed {
                url: url.to_string(),
                status: status.as_u16(),
            });
        }
        if !status.is_success() {
            return Err(SbError::HttpStatus {
                status: status.as_u16(),
                url: url.to_string(),
                body: String::new(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // Helper: build a client pointing at the wiremock server
    fn make_client(base_url: &str, token: &str) -> SbClient {
        SbClient::new(base_url, token).expect("SbClient::new should succeed")
    }

    // --- list_files tests ---

    #[tokio::test]
    async fn list_files_returns_file_meta_vec_on_200() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.fs"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"[{"name":"notes/page.md","lastModified":1700000000000,"created":1699000000000,"contentType":"text/markdown","size":1024,"perm":"rw"}]"#,
            ))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let result = client.list_files().await;
        assert!(result.is_ok(), "list_files should succeed: {result:?}");
        let files = result.unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "notes/page.md");
        assert_eq!(files[0].last_modified, 1700000000000i64);
        assert_eq!(files[0].created, 1699000000000i64);
        assert_eq!(files[0].content_type, "text/markdown");
        assert_eq!(files[0].size, 1024);
        assert_eq!(files[0].perm, Some("rw".to_string()));
    }

    #[tokio::test]
    async fn list_files_returns_auth_failed_on_401() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.fs"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "bad-token");
        let result = client.list_files().await;
        assert!(result.is_err());
        match result.unwrap_err() {
            SbError::AuthFailed { status, .. } => assert_eq!(status, 401),
            other => panic!("expected AuthFailed, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn list_files_returns_http_status_on_500() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.fs"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let result = client.list_files().await;
        assert!(result.is_err());
        match result.unwrap_err() {
            SbError::HttpStatus { status, .. } => assert_eq!(status, 500),
            other => panic!("expected HttpStatus, got: {other:?}"),
        }
    }

    // --- get_file tests ---

    #[tokio::test]
    async fn get_file_returns_bytes_on_200() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.fs/notes/page.md"))
            .respond_with(ResponseTemplate::new(200).set_body_string("# My Page\n\nContent here."))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let result = client.get_file("notes/page.md").await;
        assert!(result.is_ok(), "get_file should succeed: {result:?}");
        let bytes = result.unwrap();
        let text = std::str::from_utf8(&bytes).expect("valid utf8");
        assert!(text.contains("My Page"));
    }

    #[tokio::test]
    async fn get_file_returns_page_not_found_on_404() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.fs/missing.md"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let result = client.get_file("missing.md").await;
        assert!(result.is_err());
        match result.unwrap_err() {
            SbError::PageNotFound { name } => assert_eq!(name, "missing.md"),
            other => panic!("expected PageNotFound, got: {other:?}"),
        }
    }

    // --- get_file_meta tests ---

    #[tokio::test]
    async fn get_file_meta_returns_last_modified_from_header() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.fs/notes/page.md"))
            .and(header("X-Get-Meta", "true"))
            .respond_with(
                ResponseTemplate::new(200).insert_header("X-Last-Modified", "1700000000000"),
            )
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let result = client.get_file_meta("notes/page.md").await;
        assert!(result.is_ok(), "get_file_meta should succeed: {result:?}");
        assert_eq!(result.unwrap(), 1700000000000i64);
    }

    #[tokio::test]
    async fn get_file_meta_returns_zero_when_header_absent() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.fs/notes/page.md"))
            .and(header("X-Get-Meta", "true"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let result = client.get_file_meta("notes/page.md").await;
        assert!(result.is_ok(), "get_file_meta should return 0: {result:?}");
        assert_eq!(result.unwrap(), 0i64);
    }

    #[tokio::test]
    async fn get_file_meta_returns_page_not_found_on_404() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.fs/missing.md"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let result = client.get_file_meta("missing.md").await;
        assert!(result.is_err());
        match result.unwrap_err() {
            SbError::PageNotFound { name } => assert_eq!(name, "missing.md"),
            other => panic!("expected PageNotFound, got: {other:?}"),
        }
    }

    // --- put_file tests ---

    #[tokio::test]
    async fn put_file_sends_put_and_succeeds_on_200() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/.fs/notes/page.md"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let content = b"# My Page\n\nContent.".to_vec();
        let result = client
            .put_file("notes/page.md", bytes::Bytes::from(content))
            .await;
        assert!(result.is_ok(), "put_file should succeed: {result:?}");
    }

    #[tokio::test]
    async fn put_file_sends_text_markdown_content_type_for_md_files() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/.fs/notes/page.md"))
            .and(header("Content-Type", "text/markdown"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let content = b"# My Page".to_vec();
        client
            .put_file("notes/page.md", bytes::Bytes::from(content))
            .await
            .expect("put_file should succeed with markdown content-type");
    }

    // --- delete_file tests ---

    #[tokio::test]
    async fn delete_file_sends_delete_and_succeeds_on_200() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/.fs/notes/page.md"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let result = client.delete_file("notes/page.md").await;
        assert!(result.is_ok(), "delete_file should succeed: {result:?}");
    }

    #[tokio::test]
    async fn delete_file_returns_page_not_found_on_404() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/.fs/missing.md"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let result = client.delete_file("missing.md").await;
        assert!(result.is_err());
        match result.unwrap_err() {
            SbError::PageNotFound { name } => assert_eq!(name, "missing.md"),
            other => panic!("expected PageNotFound, got: {other:?}"),
        }
    }

    // --- header verification for new methods ---

    #[tokio::test]
    async fn list_files_sends_x_sync_mode_and_authorization() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.fs"))
            .and(header("X-Sync-Mode", "true"))
            .and(header("Authorization", "Bearer testtoken"))
            .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        client
            .list_files()
            .await
            .expect("list_files should succeed with correct headers");
    }

    #[tokio::test]
    async fn new_with_valid_token_succeeds() {
        let client = SbClient::new("http://localhost:1234", "testtoken");
        assert!(client.is_ok());
    }

    #[tokio::test]
    async fn new_with_empty_token_succeeds() {
        let client = SbClient::new("http://localhost:1234", "");
        assert!(client.is_ok());
    }

    #[tokio::test]
    async fn ping_returns_duration_on_200() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.ping"))
            .respond_with(ResponseTemplate::new(200).set_body_string("OK"))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let result = client.ping().await;
        assert!(result.is_ok(), "ping should succeed: {result:?}");
    }

    #[tokio::test]
    async fn ping_returns_http_status_error_on_500() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.ping"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let result = client.ping().await;
        assert!(result.is_err());
        match result.unwrap_err() {
            SbError::HttpStatus { status, .. } => assert_eq!(status, 500),
            other => panic!("expected HttpStatus, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn every_request_sends_x_sync_mode_true() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.ping"))
            .and(header("X-Sync-Mode", "true"))
            .respond_with(ResponseTemplate::new(200).set_body_string("OK"))
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        client.ping().await.expect("request should succeed");
        // MockServer verifies expect(1) on drop
    }

    #[tokio::test]
    async fn every_request_sends_authorization_bearer() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.ping"))
            .and(header("Authorization", "Bearer testtoken"))
            .respond_with(ResponseTemplate::new(200).set_body_string("OK"))
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        client.ping().await.expect("request should succeed");
    }

    #[tokio::test]
    async fn get_config_returns_server_config_on_200() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.config"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(
                    r#"{"readOnly":false,"spaceFolderPath":"/space","indexPage":"index","logPush":false,"enableClientEncryption":true}"#,
                ),
            )
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let result = client.get_config().await;
        assert!(result.is_ok(), "get_config should succeed: {result:?}");
        let cfg = result.unwrap();
        assert!(!cfg.read_only);
        assert_eq!(cfg.space_folder_path, "/space");
        assert_eq!(cfg.index_page, "index");
        assert!(cfg.enable_client_encryption);
    }

    #[tokio::test]
    async fn request_returning_401_produces_auth_failed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.config"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "bad-token");
        let result = client.get_config().await;
        assert!(result.is_err());
        match result.unwrap_err() {
            SbError::AuthFailed { status, .. } => assert_eq!(status, 401),
            other => panic!("expected AuthFailed, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn request_returning_403_produces_auth_failed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.config"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "bad-token");
        let result = client.get_config().await;
        assert!(result.is_err());
        match result.unwrap_err() {
            SbError::AuthFailed { status, .. } => assert_eq!(status, 403),
            other => panic!("expected AuthFailed, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn request_returning_404_produces_http_status_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.config"))
            .respond_with(ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let result = client.get_config().await;
        assert!(result.is_err());
        match result.unwrap_err() {
            SbError::HttpStatus { status, .. } => assert_eq!(status, 404),
            other => panic!("expected HttpStatus, got: {other:?}"),
        }
    }

    // --- get_page tests ---

    #[tokio::test]
    async fn get_page_returns_content_on_200() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.fs/test-page.md"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("# Test Page\n\nSome content here."),
            )
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let result = client.get_page("test-page").await;
        assert!(result.is_ok(), "get_page should succeed on 200: {result:?}");
        let content = result.unwrap();
        assert!(
            content.contains("Test Page"),
            "content should contain page text"
        );
    }

    #[tokio::test]
    async fn get_page_returns_page_not_found_on_404() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.fs/missing.md"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let result = client.get_page("missing").await;
        assert!(result.is_err());
        match result.unwrap_err() {
            SbError::PageNotFound { name } => assert_eq!(name, "missing"),
            other => panic!("expected PageNotFound, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn get_page_returns_auth_failed_on_401() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.fs/secret.md"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "bad-token");
        let result = client.get_page("secret").await;
        assert!(result.is_err());
        match result.unwrap_err() {
            SbError::AuthFailed { status, .. } => assert_eq!(status, 401),
            other => panic!("expected AuthFailed, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn get_page_sends_x_sync_mode_and_authorization_headers() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.fs/test-page.md"))
            .and(header("X-Sync-Mode", "true"))
            .and(header("Authorization", "Bearer testtoken"))
            .respond_with(ResponseTemplate::new(200).set_body_string("content"))
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        client
            .get_page("test-page")
            .await
            .expect("get_page should succeed with correct headers");
        // MockServer verifies expect(1) on drop
    }

    // --- probe_runtime_api tests ---

    #[tokio::test]
    async fn probe_runtime_api_returns_true_on_400() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/.runtime/lua"))
            .respond_with(ResponseTemplate::new(400))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let result = client.probe_runtime_api().await;
        assert!(result.is_ok(), "probe should succeed: {result:?}");
        assert!(
            result.unwrap(),
            "400 means endpoint exists, should return true"
        );
    }

    #[tokio::test]
    async fn probe_runtime_api_returns_false_on_503() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/.runtime/lua"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let result = client.probe_runtime_api().await;
        assert!(result.is_ok(), "probe should succeed: {result:?}");
        assert!(
            !result.unwrap(),
            "503 means unavailable, should return false"
        );
    }

    #[tokio::test]
    async fn probe_runtime_api_returns_true_on_200() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/.runtime/lua"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let result = client.probe_runtime_api().await;
        assert!(result.is_ok(), "probe should succeed: {result:?}");
        assert!(result.unwrap(), "200 means available, should return true");
    }

    #[tokio::test]
    async fn post_text_sends_content_type_text_plain() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/.runtime/lua"))
            .and(header("Content-Type", "text/plain"))
            .respond_with(ResponseTemplate::new(200).set_body_string("result"))
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let result = client.post_text("/.runtime/lua", "1 + 1").await;
        assert!(result.is_ok(), "post_text should succeed: {result:?}");
        // MockServer verifies expect(1) on drop
    }

    // --- encode_path tests ---

    #[test]
    fn encode_path_leaves_simple_path_unchanged() {
        assert_eq!(
            encode_path("Journal/2026-04-05.md"),
            "Journal/2026-04-05.md"
        );
    }

    #[test]
    fn encode_path_encodes_question_mark() {
        assert_eq!(encode_path("What is Rust?.md"), "What%20is%20Rust%3F.md");
    }

    #[test]
    fn encode_path_encodes_hash() {
        assert_eq!(encode_path("Notes/#ideas.md"), "Notes/%23ideas.md");
    }

    #[test]
    fn encode_path_encodes_space() {
        assert_eq!(
            encode_path("My Notes/Some Page.md"),
            "My%20Notes/Some%20Page.md"
        );
    }

    #[test]
    fn encode_path_preserves_slash_separator() {
        // Slashes must NOT be encoded — they are path separators
        assert_eq!(encode_path("a/b/c.md"), "a/b/c.md");
    }

    #[test]
    fn encode_path_encodes_ampersand() {
        assert_eq!(encode_path("Tom & Jerry.md"), "Tom%20%26%20Jerry.md");
    }

    #[tokio::test]
    async fn get_file_encodes_question_mark_in_path() {
        let server = MockServer::start().await;
        // The server expects the percent-encoded path
        Mock::given(method("GET"))
            .and(path("/.fs/What%20is%20Rust%3F.md"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"content"))
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        client
            .get_file("What is Rust?.md")
            .await
            .expect("get_file should succeed with percent-encoded path");
        // MockServer verifies expect(1) on drop — fails if wrong URL was requested
    }

    #[tokio::test]
    async fn put_file_encodes_question_mark_in_path() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/.fs/What%20is%20Rust%3F.md"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        client
            .put_file("What is Rust?.md", bytes::Bytes::from_static(b"content"))
            .await
            .expect("put_file should succeed with percent-encoded path");
    }

    // --- send_with_retry tests ---

    #[tokio::test]
    async fn send_with_retry_succeeds_on_200_without_retry() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.ping"))
            .respond_with(ResponseTemplate::new(200).set_body_string("OK"))
            .expect(1) // must only be called once (no retry)
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let url = format!("{}/.ping", server.uri());
        let result = client
            .send_with_retry(&url, || client.inner.get(&url))
            .await;
        assert!(
            result.is_ok(),
            "200 should succeed without retry: {result:?}"
        );
        assert_eq!(result.unwrap().status().as_u16(), 200);
        // MockServer verifies expect(1) on drop
    }

    #[tokio::test]
    async fn send_with_retry_retries_500_and_succeeds_on_second_try() {
        let server = MockServer::start().await;
        // Mount 200 first (lower priority), then 500 with up_to_n_times(1) (higher priority)
        // After the 500 is consumed, the 200 takes effect
        Mock::given(method("GET"))
            .and(path("/.ping"))
            .respond_with(ResponseTemplate::new(200).set_body_string("OK"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/.ping"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let url = format!("{}/.ping", server.uri());
        let result = client
            .send_with_retry(&url, || client.inner.get(&url))
            .await;
        assert!(
            result.is_ok(),
            "should succeed on second attempt: {result:?}"
        );
        assert_eq!(result.unwrap().status().as_u16(), 200);
    }

    #[tokio::test]
    async fn send_with_retry_gives_up_after_3_retries_on_503() {
        let server = MockServer::start().await;
        // All 4 attempts (initial + 3 retries) return 503
        Mock::given(method("GET"))
            .and(path("/.ping"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let url = format!("{}/.ping", server.uri());
        let result = client
            .send_with_retry(&url, || client.inner.get(&url))
            .await;
        assert!(result.is_err(), "should fail after 3 retries exhausted");
        match result.unwrap_err() {
            SbError::HttpStatus { status, .. } => assert_eq!(status, 503),
            other => panic!("expected HttpStatus(503), got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_with_retry_does_not_retry_on_400() {
        let server = MockServer::start().await;
        // 400 is a client error — must not retry, only called once
        Mock::given(method("GET"))
            .and(path("/.ping"))
            .respond_with(ResponseTemplate::new(400))
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let url = format!("{}/.ping", server.uri());
        let result = client
            .send_with_retry(&url, || client.inner.get(&url))
            .await;
        // 400 is not a 5xx, so send_with_retry returns Ok(resp) immediately
        assert!(
            result.is_ok(),
            "400 should be returned as Ok(resp) immediately"
        );
        assert_eq!(result.unwrap().status().as_u16(), 400);
        // MockServer verifies expect(1) on drop — ensures no retry happened
    }

    #[tokio::test]
    async fn send_with_retry_does_not_retry_on_401() {
        let server = MockServer::start().await;
        // 401 is not a 5xx — returned immediately as Ok(resp)
        Mock::given(method("GET"))
            .and(path("/.ping"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let url = format!("{}/.ping", server.uri());
        let result = client
            .send_with_retry(&url, || client.inner.get(&url))
            .await;
        assert!(
            result.is_ok(),
            "401 should be returned as Ok(resp) immediately"
        );
        assert_eq!(result.unwrap().status().as_u16(), 401);
        // MockServer verifies expect(1) on drop
    }
}
