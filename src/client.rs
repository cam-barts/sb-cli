use bytes::Bytes;
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use reqwest::{
    header::{self, HeaderMap, HeaderValue},
    Client, ClientBuilder, StatusCode,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::error::{SbError, SbResult};

/// Request timeout in seconds. The SilverBullet Runtime API defaults to 30s of
/// its own, so the two must move together: raising only the client side leaves
/// the server hanging up first, and raising only the server side leaves reqwest
/// aborting first. `--timeout` sets this once at startup and both the reqwest
/// builder and the `X-Timeout` header read it.
static TIMEOUT_SECS: AtomicU64 = AtomicU64::new(DEFAULT_TIMEOUT_SECS);

/// Matches the Runtime API's own documented default.
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Record the `--timeout` flag. Call once during startup.
pub fn set_timeout_secs(secs: u64) {
    TIMEOUT_SECS.store(secs, Ordering::Relaxed);
}

/// The configured request timeout in seconds.
pub fn timeout_secs() -> u64 {
    TIMEOUT_SECS.load(Ordering::Relaxed)
}

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

/// Build the `?limit=&since=` query string for GET /.runtime/logs.
///
/// Returns `""` when both are `None`. `since` is omitted (not sent as
/// `since=`) when `None` -- that distinction is load-bearing, see
/// `get_runtime_logs`.
fn runtime_logs_query_string(limit: Option<usize>, since: Option<i64>) -> String {
    let mut params = Vec::new();
    if let Some(limit) = limit {
        params.push(format!("limit={limit}"));
    }
    if let Some(since) = since {
        params.push(format!("since={since}"));
    }
    if params.is_empty() {
        String::new()
    } else {
        format!("?{}", params.join("&"))
    }
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

/// A single client or server log entry returned by GET /.runtime/logs.
///
/// All fields are optional because the SilverBullet runtime emits varying
/// shapes (older versions omit timestamp; some entries are plain strings
/// rather than objects, in which case the entry is captured in `message`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RuntimeLogEntry {
    #[serde(default)]
    pub level: Option<String>,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub timestamp: Option<i64>,
}

/// Logs payload returned by GET /.runtime/logs.
///
/// Splits client (browser-side) and server (Deno/headless) logs so callers can
/// label them. `extra` collects any fields the server may add in the future.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeLogs {
    #[serde(default)]
    pub client_logs: Vec<RuntimeLogEntry>,
    #[serde(default)]
    pub server_logs: Vec<RuntimeLogEntry>,
    #[serde(flatten, default)]
    pub extra: std::collections::BTreeMap<String, serde_json::Value>,
}

/// A single git-backed revision of one file, as returned within the
/// `revisions` array of GET `/.revisions/<path>`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RevisionEntry {
    pub rev: String,
    /// Unix milliseconds (matches `FileMeta` convention -- never seconds).
    pub timestamp: i64,
    pub author: String,
    pub message: String,
    #[serde(default)]
    pub added: u64,
    #[serde(default)]
    pub removed: u64,
}

/// GET `/.revisions/<path>` (no `rev`) response: one file's revision history.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileRevisions {
    /// "managed" | "unmanaged" -- unmanaged spaces can still have a (possibly
    /// empty) revision list; that is not an error.
    pub mode: String,
    /// True when the file on disk differs from the last commit.
    pub uncommitted: bool,
    pub revisions: Vec<RevisionEntry>,
    /// True when more revisions exist beyond `limit` (page further back with
    /// `before`).
    pub more: bool,
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
            .timeout(Duration::from_secs(timeout_secs()))
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
        // Same diagnostic as `get_file`. This is the cheapest way to ask a
        // running deployment whether conditional writes survive the path to it:
        // the same `/.fs` endpoint, reachable via
        // `sb --verbose page read <name> --remote` with no sync involved.
        log_etag_outcome(resp.headers(), name);
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
    /// Returns the bytes plus the server's `ETag` for that content, or `None`
    /// when the server does not send one (SilverBullet <= 2.10.0). The ETag is a
    /// SHA-256 digest chosen by the server — it is NOT the blake3 hash the sync
    /// engine computes locally, and the two must never be compared or assigned
    /// to each other.
    ///
    /// Returns `SbError::PageNotFound` on 404.
    pub async fn get_file(&self, path: &str) -> SbResult<(Bytes, Option<String>)> {
        let url = format!("{}/.fs/{}", self.base_url, encode_path(path));
        let resp = self.send_with_retry(&url, || self.inner.get(&url)).await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Err(SbError::PageNotFound {
                name: path.to_string(),
            });
        }
        let resp = Self::check_response(resp, &url).await?;
        let status = resp.status();
        let etag = etag_from_headers(resp.headers());
        // Whether conditional writes are in play is not discoverable from the
        // server's config, and every ETag path degrades silently when they are
        // not. Log what we saw so `--verbose` can answer it without a capture.
        log_etag_outcome(resp.headers(), path);
        let bytes = resp.bytes().await.map_err(|e| SbError::HttpStatus {
            status: status.as_u16(),
            url,
            body: format!("failed to read file content: {e}"),
        })?;
        Ok((bytes, etag))
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
    ///
    /// When `if_match` is `Some`, sends it as an `If-Match` header so the server
    /// rejects the write with 412 (`SbError::PreconditionFailed`) if its copy
    /// changed since we last saw that ETag. Pass `None` for an unconditional
    /// last-write-wins PUT — which is what happens against a server that never
    /// gave us an ETag.
    ///
    /// Returns the ETag of the newly stored content, or `None` if the server did
    /// not send one.
    pub async fn put_file(
        &self,
        path: &str,
        content: bytes::Bytes,
        if_match: Option<&str>,
    ) -> SbResult<Option<String>> {
        let url = format!("{}/.fs/{}", self.base_url, encode_path(path));
        let content_type = if path.ends_with(".md") {
            "text/markdown"
        } else {
            "application/octet-stream"
        };
        let resp = self
            .send_retrying(
                &url,
                || {
                    let req = self
                        .inner
                        .put(&url)
                        .header(header::CONTENT_TYPE, content_type)
                        .body(content.clone());
                    match if_match {
                        Some(etag) => req.header(header::IF_MATCH, etag),
                        None => req,
                    }
                },
                if_match.is_none(),
            )
            .await?;
        let resp = Self::check_response_write(resp, &url).await?;
        Ok(etag_from_headers(resp.headers()))
    }

    /// DELETE `/.fs/<path>` — delete a file from the server.
    ///
    /// Returns `SbError::PageNotFound` on 404.
    /// When `if_match` is `Some`, sends it as an `If-Match` header so a server
    /// whose copy changed answers 412 (`SbError::PreconditionFailed`) instead of
    /// deleting a version we have never seen.
    pub async fn delete_file(&self, path: &str, if_match: Option<&str>) -> SbResult<()> {
        let url = format!("{}/.fs/{}", self.base_url, encode_path(path));
        let resp = self
            .send_retrying(
                &url,
                || {
                    let req = self.inner.delete(&url);
                    match if_match {
                        Some(etag) => req.header(header::IF_MATCH, etag),
                        None => req,
                    }
                },
                if_match.is_none(),
            )
            .await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Err(SbError::PageNotFound {
                name: path.to_string(),
            });
        }
        Self::check_response_write(resp, &url).await?;
        Ok(())
    }

    /// POST `<endpoint>` with a `text/plain` body.
    ///
    /// The `endpoint` is a path like `/.runtime/lua` (NOT a full URL).
    /// Returns the raw Response so callers can inspect status and body.
    /// Uses `send_with_retry` internally for timeout resilience.
    ///
    /// Sends `X-Timeout`, which the Runtime API honors for its Lua endpoints,
    /// so the server gives up at the same moment reqwest does rather than one
    /// silently racing the other.
    pub async fn post_text(&self, endpoint: &str, body: &str) -> SbResult<reqwest::Response> {
        let url = format!("{}{}", self.base_url, endpoint);
        let body_bytes = bytes::Bytes::from(body.to_string());
        let timeout = timeout_secs();
        self.send_with_retry(&url, || {
            self.inner
                .post(&url)
                .header(header::CONTENT_TYPE, "text/plain")
                .header("X-Timeout", timeout)
                .body(body_bytes.clone())
        })
        .await
    }

    /// POST `<endpoint>` with a `text/plain` body, WITHOUT the 5xx retry.
    ///
    /// `send_with_retry` treats every 5xx as transient and, after exhausting its
    /// backoff, discards the response body. That is wrong for the Runtime API,
    /// whose failures are deterministic and describe themselves in the body: a
    /// Lua error is a 500 carrying `{"error":..., "code":"script_error"}`, and
    /// retrying it four times over seven seconds only turns a precise message
    /// ("attempt to index a nil value") into a bare "server returned 500".
    ///
    /// Callers that need the error envelope use this and classify it themselves.
    pub async fn post_text_once(&self, endpoint: &str, body: &str) -> SbResult<reqwest::Response> {
        let url = format!("{}{}", self.base_url, endpoint);
        self.inner
            .post(&url)
            .header(header::CONTENT_TYPE, "text/plain")
            .header("X-Timeout", timeout_secs())
            .body(bytes::Bytes::from(body.to_string()))
            .send()
            .await
            .map_err(|e| SbError::Network {
                url,
                source: Box::new(e),
            })
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

    /// GET `/.runtime/logs` — fetch buffered client and server logs.
    ///
    /// `limit` caps the number of entries the server returns (server default
    /// 100, retains up to 1000). `since` is a unix millisecond timestamp;
    /// only entries newer than it are returned. Passing `since: None` omits
    /// the parameter entirely, which the server treats as "include every
    /// entry, including those without timestamps" -- distinct from any
    /// concrete value.
    ///
    /// Returns `RuntimeLogs` with parsed `client_logs` and `server_logs`.
    /// Returns `SbError::HttpStatus { status: 503, .. }` when the Runtime API
    /// is not running so callers can map it to a friendlier message.
    pub async fn get_runtime_logs(
        &self,
        limit: Option<usize>,
        since: Option<i64>,
    ) -> SbResult<RuntimeLogs> {
        let url = format!(
            "{}/.runtime/logs{}",
            self.base_url,
            runtime_logs_query_string(limit, since)
        );
        let resp = self.send_with_retry(&url, || self.inner.get(&url)).await?;
        if resp.status() == StatusCode::SERVICE_UNAVAILABLE {
            return Err(SbError::HttpStatus {
                status: 503,
                url,
                body: "Runtime API not available".into(),
            });
        }
        let resp = Self::check_response(resp, &url).await?;
        let status = resp.status();
        resp.json::<RuntimeLogs>()
            .await
            .map_err(|e| SbError::HttpStatus {
                status: status.as_u16(),
                url,
                body: format!("failed to parse runtime logs: {e}"),
            })
    }

    /// GET `/.runtime/screenshot` — fetch a PNG screenshot of the headless
    /// browser's current state.
    ///
    /// Returns the raw PNG bytes. Returns `SbError::HttpStatus { status: 503 }`
    /// when the Runtime API is not running.
    pub async fn get_runtime_screenshot(&self) -> SbResult<Bytes> {
        let url = format!("{}/.runtime/screenshot", self.base_url);
        let resp = self.send_with_retry(&url, || self.inner.get(&url)).await?;
        if resp.status() == StatusCode::SERVICE_UNAVAILABLE {
            return Err(SbError::HttpStatus {
                status: 503,
                url,
                body: "Runtime API not available".into(),
            });
        }
        let resp = Self::check_response(resp, &url).await?;
        let status = resp.status();
        resp.bytes().await.map_err(|e| SbError::HttpStatus {
            status: status.as_u16(),
            url,
            body: format!("failed to read screenshot bytes: {e}"),
        })
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
        self.send_retrying(url, build, true).await
    }

    /// `send_with_retry`, but `retry: false` sends exactly once.
    ///
    /// A conditional write must not be replayed. If a `PUT ... If-Match` times
    /// out client-side the server may already have applied it, and the retry
    /// carries the now-stale `If-Match`, which a conditional-write server
    /// answers 412. That turns one slow request into a fabricated conflict and
    /// a stash file for a write that actually succeeded. The same reasoning
    /// covers a 5xx from an intermediary, which can equally sit in front of a
    /// write that landed. Failing the push is honest and self-corrects on the
    /// next sync.
    async fn send_retrying(
        &self,
        url: &str,
        build: impl Fn() -> reqwest::RequestBuilder,
        retry: bool,
    ) -> SbResult<reqwest::Response> {
        let max_retries = if retry { 3u32 } else { 0 };
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
    /// On 401, returns `SbError::AuthFailed`; on 403, `SbError::ReadOnly`;
    /// on 412, `SbError::PreconditionFailed`.
    /// On any other non-2xx, reads the body and returns `SbError::HttpStatus`.
    /// On 2xx, returns the response so the caller can read its body.
    async fn check_response(resp: reqwest::Response, url: &str) -> SbResult<reqwest::Response> {
        Self::check_response_inner(resp, url, false).await
    }

    /// `check_response` for a request that WRITES (PUT/DELETE), where a 403
    /// genuinely means the path is read-only rather than the caller being
    /// unauthenticated. See `status_error`.
    async fn check_response_write(
        resp: reqwest::Response,
        url: &str,
    ) -> SbResult<reqwest::Response> {
        Self::check_response_inner(resp, url, true).await
    }

    async fn check_response_inner(
        resp: reqwest::Response,
        url: &str,
        write: bool,
    ) -> SbResult<reqwest::Response> {
        let status = resp.status();
        if let Some(err) = status_error(status, url, write) {
            return Err(err);
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
        if let Some(err) = status_error(status, url, false) {
            return Err(err);
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

    /// Map a 404 from a `/.revisions` endpoint to `SbError`.
    ///
    /// Every `/.revisions` endpoint answers 404 identically when revisions
    /// are disabled for the space, with body `{"error": "revisions
    /// disabled"}`. That is distinguished here from an ordinary not-found
    /// (unknown path) by sniffing the body, so callers get the friendly
    /// message instead of a bare 404.
    fn revisions_disabled_or_http_status(url: &str, body: String) -> SbError {
        if body.contains("revisions disabled") {
            SbError::RevisionsDisabled
        } else {
            SbError::HttpStatus {
                status: 404,
                url: url.to_string(),
                body,
            }
        }
    }

    /// GET `/.revisions/<path>` -- a single file's git-backed revision history.
    ///
    /// `before` pages backward from that revision hash; `limit` is clamped
    /// server-side to 200. Returns `SbError::RevisionsDisabled` when the space
    /// has revisions turned off. An enabled-but-empty history (e.g. an
    /// unmanaged space that has never been snapshotted) is a normal `Ok` with
    /// an empty `revisions` vec, not an error.
    pub async fn get_file_revisions(
        &self,
        path: &str,
        before: Option<&str>,
        limit: usize,
    ) -> SbResult<FileRevisions> {
        let url = format!("{}/.revisions/{}", self.base_url, encode_path(path));
        let limit_str = limit.to_string();
        let before_owned = before.map(|s| s.to_string());
        let resp = self
            .send_with_retry(&url, || {
                let mut query = vec![("limit", limit_str.as_str())];
                if let Some(b) = before_owned.as_deref() {
                    query.push(("before", b));
                }
                self.inner.get(&url).query(&query)
            })
            .await?;
        if resp.status() == StatusCode::NOT_FOUND {
            let body = resp.text().await.unwrap_or_default();
            return Err(Self::revisions_disabled_or_http_status(&url, body));
        }
        let resp = Self::check_response(resp, &url).await?;
        let status = resp.status();
        resp.json::<FileRevisions>()
            .await
            .map_err(|e| SbError::HttpStatus {
                status: status.as_u16(),
                url,
                body: format!("failed to parse revision history: {e}"),
            })
    }

    /// GET `/.revisions/<path>?format=diff[&rev=<hash>]` -- a unified diff.
    ///
    /// With `rev` set, the diff is what that revision changed versus its
    /// parent. With `rev` omitted, it is the uncommitted change: HEAD versus
    /// what is on disk.
    ///
    /// Returns `Ok(None)` for the two documented "nothing to diff" 404s --
    /// a revision with no parent to diff against (a merge commit), and an
    /// uncommitted diff that turns out to match HEAD after all -- neither of
    /// which is an error condition. Returns `SbError::RevisionsDisabled` when
    /// revisions are off for the space.
    pub async fn get_revision_diff(
        &self,
        path: &str,
        rev: Option<&str>,
    ) -> SbResult<Option<String>> {
        let url = format!("{}/.revisions/{}", self.base_url, encode_path(path));
        let rev_owned = rev.map(|s| s.to_string());
        let resp = self
            .send_with_retry(&url, || {
                let mut query = vec![("format", "diff")];
                if let Some(r) = rev_owned.as_deref() {
                    query.push(("rev", r));
                }
                self.inner.get(&url).query(&query)
            })
            .await?;
        if resp.status() == StatusCode::NOT_FOUND {
            let body = resp.text().await.unwrap_or_default();
            if body.contains("revisions disabled") {
                return Err(SbError::RevisionsDisabled);
            }
            return Ok(None);
        }
        let resp = Self::check_response(resp, &url).await?;
        let status = resp.status();
        resp.text()
            .await
            .map(Some)
            .map_err(|e| SbError::HttpStatus {
                status: status.as_u16(),
                url,
                body: format!("failed to read diff body: {e}"),
            })
    }

    /// GET `/.revisions/<path>?rev=<hash>` -- a file's content as of that
    /// revision, served with the file's own content type.
    ///
    /// Returns `SbError::RevisionsDisabled` when revisions are off for the
    /// space. A 404 for an unknown revision (or a path that did not exist at
    /// it) surfaces as `SbError::HttpStatus`.
    pub async fn get_revision_content(&self, path: &str, rev: &str) -> SbResult<Bytes> {
        let url = format!("{}/.revisions/{}", self.base_url, encode_path(path));
        let rev = rev.to_string();
        let resp = self
            .send_with_retry(&url, || {
                self.inner.get(&url).query(&[("rev", rev.as_str())])
            })
            .await?;
        if resp.status() == StatusCode::NOT_FOUND {
            let body = resp.text().await.unwrap_or_default();
            return Err(Self::revisions_disabled_or_http_status(&url, body));
        }
        let resp = Self::check_response(resp, &url).await?;
        let status = resp.status();
        resp.bytes().await.map_err(|e| SbError::HttpStatus {
            status: status.as_u16(),
            url,
            body: format!("failed to read revision content: {e}"),
        })
    }
}

/// Map the HTTP statuses that carry a specific meaning to their `SbError`.
///
/// 401 is a real authentication failure. 403 is NOT: since SilverBullet 2.10.0
/// it means the path is read-only (a bundled `Library/Std` page, or a server run
/// with `SB_READ_ONLY`), which no amount of re-authenticating will fix.
/// 412 is a rejected `If-Match` conditional write.
/// Returns `None` for every other status, success included.
fn status_error(status: StatusCode, url: &str, write: bool) -> Option<SbError> {
    match status {
        StatusCode::UNAUTHORIZED => Some(SbError::AuthFailed {
            url: url.to_string(),
            status: status.as_u16(),
        }),
        // Only a refused WRITE means "this path is read-only". 403 is not
        // exclusively SilverBullet's: an auth proxy in front of it (oauth2-proxy,
        // Authelia, Cloudflare Access) answers 403 for a rejected identity, and
        // telling that user "your token is fine" sends them the wrong way. On a
        // read, treat it as the auth failure it almost certainly is.
        StatusCode::FORBIDDEN if write => Some(SbError::ReadOnly {
            url: url.to_string(),
        }),
        StatusCode::FORBIDDEN => Some(SbError::AuthFailed {
            url: url.to_string(),
            status: status.as_u16(),
        }),
        StatusCode::PRECONDITION_FAILED => Some(SbError::PreconditionFailed {
            url: url.to_string(),
        }),
        _ => None,
    }
}

/// Extract a reusable `ETag` from a response's headers.
///
/// The value is kept verbatim (quotes included) so it can be echoed straight
/// back in `If-Match` without re-quoting. Returns `None` when the header is
/// absent (any SilverBullet without conditional writes), empty, not valid
/// ASCII, or a weak validator (`W/"..."`), which HTTP forbids in `If-Match`.
///
/// Whatever comes back is the server's own digest (`"sha256:<hex>"` today). It is
/// never interchangeable with the blake3 `local_hash`/`remote_hash` the sync
/// engine computes.
fn etag_from_headers(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::ETAG)?.to_str().ok()?.trim();
    if raw.is_empty() || raw.starts_with("W/") {
        return None;
    }
    Some(raw.to_string())
}

/// Log why conditional writes are or are not in play for this response.
///
/// Deliberately distinguishes "no `ETag` at all" from "an `ETag` we refused",
/// because the two have completely different fixes and collapsing them into one
/// message sent this author chasing a server-version problem that did not
/// exist. A CDN that rewrites `ETag: "x"` into `ETag: W/"x"` (Cloudflare does
/// this to any response it might transform) looks identical to a server with no
/// conditional-write support unless the message says which happened.
fn log_etag_outcome(headers: &HeaderMap, path: &str) {
    match headers.get(header::ETAG).and_then(|v| v.to_str().ok()) {
        Some(raw) if raw.trim().starts_with("W/") => tracing::debug!(
            path,
            etag = %raw.trim(),
            "server sent a WEAK ETag; refusing it for If-Match (RFC 9110 requires \
             strong comparison). Something between here and the origin weakened it \
             -- conditional writes are off until that stops"
        ),
        Some(raw) if raw.trim().is_empty() => {
            tracing::debug!(
                path,
                "server sent an empty ETag; conditional writes are off"
            )
        }
        Some(raw) => tracing::debug!(path, etag = %raw.trim(), "server sent a strong ETag"),
        None => tracing::debug!(path, "server sent no ETag; conditional writes are off"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // Helper: build a client pointing at the wiremock server
    fn make_client(base_url: &str, token: &str) -> SbClient {
        SbClient::new(base_url, token).expect("SbClient::new should succeed")
    }

    /// A weak validator must never reach `If-Match`: RFC 9110 requires strong
    /// comparison there, and a CDN that rewrites `"x"` to `W/"x"` is asserting
    /// the body may have been transformed. Accepting it would make a
    /// conditional write match content that is not byte-identical, which is the
    /// exact guarantee the feature exists to provide.
    #[test]
    fn a_weak_etag_is_refused_for_conditional_writes() {
        let mut h = HeaderMap::new();
        h.insert(
            header::ETAG,
            HeaderValue::from_static("W/\"sha256:deadbeef\""),
        );
        assert_eq!(etag_from_headers(&h), None);

        // ...while the same tag unweakened is accepted verbatim, quotes and all.
        let mut h = HeaderMap::new();
        h.insert(
            header::ETAG,
            HeaderValue::from_static("\"sha256:deadbeef\""),
        );
        assert_eq!(
            etag_from_headers(&h),
            Some("\"sha256:deadbeef\"".to_string())
        );
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
        let (bytes, _etag) = result.unwrap();
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
            .put_file("notes/page.md", bytes::Bytes::from(content), None)
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
            .put_file("notes/page.md", bytes::Bytes::from(content), None)
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
        let result = client.delete_file("notes/page.md", None).await;
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
        let result = client.delete_file("missing.md", None).await;
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

    /// A conditional write is sent exactly once. Replaying it would carry a
    /// stale `If-Match` past a write that may already have landed, and the 412
    /// that follows fabricates a conflict (and a stash file) for a PUT that
    /// actually succeeded.
    #[tokio::test]
    async fn a_conditional_put_is_never_replayed() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/.fs/note.md"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "t");
        let _ = client
            .put_file("note.md", Bytes::from_static(b"x"), Some("\"sha256:abc\""))
            .await;
        // Mock::expect is verified on drop.
    }

    /// The unconditional path keeps its retry: with nothing to go stale, a
    /// transient 5xx is worth another attempt.
    #[tokio::test]
    async fn an_unconditional_put_still_retries() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/.fs/note.md"))
            .respond_with(ResponseTemplate::new(500))
            .expect(4) // initial attempt plus three retries
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "t");
        let _ = client
            .put_file("note.md", Bytes::from_static(b"x"), None)
            .await;
    }

    /// A refused WRITE is a read-only path, not a bad token: it must NOT be
    /// AuthFailed and must not exit 3, or agents retry auth for a problem auth
    /// cannot fix.
    #[tokio::test]
    async fn a_403_on_a_write_produces_read_only_not_auth_failed() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/.fs/Library/Std/Config.md"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "good-token");
        let err = client
            .put_file("Library/Std/Config.md", Bytes::from_static(b"x"), None)
            .await
            .unwrap_err();
        match err {
            SbError::ReadOnly { url } => assert!(url.contains("Library/Std/Config.md")),
            other => panic!("expected ReadOnly, got: {other:?}"),
        }
    }

    /// The mirror image. A 403 on a READ is not evidence of a read-only path:
    /// an auth proxy in front of SilverBullet answers 403 for a rejected
    /// identity, and "your token is fine" would send that user the wrong way.
    #[tokio::test]
    async fn a_403_on_a_read_stays_an_auth_failure() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.config"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "good-token");
        let err = client.get_config().await.unwrap_err();
        assert_eq!(err.exit_code(), 3, "a refused read stays exit 3 (auth)");
        match err {
            SbError::AuthFailed { status, .. } => assert_eq!(status, 403),
            other => panic!("expected AuthFailed, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn request_returning_412_produces_precondition_failed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.config"))
            .respond_with(ResponseTemplate::new(412))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        match client.get_config().await.unwrap_err() {
            SbError::PreconditionFailed { url } => assert!(url.contains("/.config")),
            other => panic!("expected PreconditionFailed, got: {other:?}"),
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
            .put_file(
                "What is Rust?.md",
                bytes::Bytes::from_static(b"content"),
                None,
            )
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

    // --- get_runtime_logs tests ---

    #[tokio::test]
    async fn get_runtime_logs_parses_client_and_server_arrays() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.runtime/logs"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"clientLogs":[{"level":"log","message":"hello","timestamp":1700000000000}],"serverLogs":[{"level":"error","message":"boom"}]}"#,
            ))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let logs = client
            .get_runtime_logs(None, None)
            .await
            .expect("get_runtime_logs should succeed");
        assert_eq!(logs.client_logs.len(), 1);
        assert_eq!(logs.client_logs[0].message, "hello");
        assert_eq!(logs.client_logs[0].level.as_deref(), Some("log"));
        assert_eq!(logs.client_logs[0].timestamp, Some(1700000000000));
        assert_eq!(logs.server_logs.len(), 1);
        assert_eq!(logs.server_logs[0].message, "boom");
        assert_eq!(logs.server_logs[0].level.as_deref(), Some("error"));
    }

    #[tokio::test]
    async fn get_runtime_logs_returns_503_when_runtime_unavailable() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.runtime/logs"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let err = client
            .get_runtime_logs(None, None)
            .await
            .expect_err("503 should error");
        match err {
            SbError::HttpStatus { status, .. } => assert_eq!(status, 503),
            other => panic!("expected HttpStatus(503), got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn get_runtime_logs_handles_missing_arrays() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.runtime/logs"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let logs = client
            .get_runtime_logs(None, None)
            .await
            .expect("empty payload should deserialize to empty vecs");
        assert!(logs.client_logs.is_empty());
        assert!(logs.server_logs.is_empty());
    }

    // --- runtime_logs_query_string (pure logic) ---

    #[test]
    fn runtime_logs_query_string_empty_when_both_none() {
        assert_eq!(runtime_logs_query_string(None, None), "");
    }

    #[test]
    fn runtime_logs_query_string_sends_limit_only() {
        assert_eq!(runtime_logs_query_string(Some(50), None), "?limit=50");
    }

    #[test]
    fn runtime_logs_query_string_sends_since_only() {
        assert_eq!(
            runtime_logs_query_string(None, Some(1700000000000)),
            "?since=1700000000000"
        );
    }

    #[test]
    fn runtime_logs_query_string_sends_both() {
        assert_eq!(
            runtime_logs_query_string(Some(50), Some(123)),
            "?limit=50&since=123"
        );
    }

    #[tokio::test]
    async fn get_runtime_logs_sends_limit_as_query_param() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.runtime/logs"))
            .and(query_param("limit", "20"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        client
            .get_runtime_logs(Some(20), None)
            .await
            .expect("mock only matches the request carrying limit=20");
    }

    #[tokio::test]
    async fn get_runtime_logs_first_poll_omits_since() {
        let server = MockServer::start().await;
        // Constraining the mock on the missing param IS the assertion: a request
        // carrying `since` at all would 404 against this mock.
        Mock::given(method("GET"))
            .and(path("/.runtime/logs"))
            .and(query_param_is_missing("since"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        client
            .get_runtime_logs(None, None)
            .await
            .expect("first poll must not send since");
    }

    #[tokio::test]
    async fn get_runtime_logs_sends_since_once_high_water_known() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.runtime/logs"))
            .and(query_param("since", "1700000000000"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        client
            .get_runtime_logs(None, Some(1700000000000))
            .await
            .expect("mock only matches the request carrying since");
    }

    // --- get_runtime_screenshot tests ---

    #[tokio::test]
    async fn get_runtime_screenshot_returns_png_bytes() {
        let server = MockServer::start().await;
        let png_magic = b"\x89PNG\r\n\x1a\nfake-image-bytes";
        Mock::given(method("GET"))
            .and(path("/.runtime/screenshot"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "image/png")
                    .set_body_bytes(png_magic.to_vec()),
            )
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let bytes = client
            .get_runtime_screenshot()
            .await
            .expect("screenshot should succeed");
        assert_eq!(&bytes[..8], &png_magic[..8]);
    }

    #[tokio::test]
    async fn get_runtime_screenshot_returns_503_when_runtime_unavailable() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.runtime/screenshot"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let err = client
            .get_runtime_screenshot()
            .await
            .expect_err("503 should error");
        match err {
            SbError::HttpStatus { status, .. } => assert_eq!(status, 503),
            other => panic!("expected HttpStatus(503), got: {other:?}"),
        }
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

    // --- get_file_revisions tests ---

    #[tokio::test]
    async fn get_file_revisions_renders_an_enabled_but_empty_history() {
        // Mirrors the real target server: mode "unmanaged", no commits yet.
        // "enabled but empty" must come back Ok, not an error.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.revisions/index.md"))
            .and(query_param("limit", "50"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"mode":"unmanaged","more":false,"revisions":[],"uncommitted":true}"#,
            ))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let history = client
            .get_file_revisions("index.md", None, 50)
            .await
            .expect("empty-but-enabled history should be Ok");
        assert_eq!(history.mode, "unmanaged");
        assert!(history.revisions.is_empty());
        assert!(history.uncommitted);
        assert!(!history.more);
    }

    #[tokio::test]
    async fn get_file_revisions_parses_revision_list_and_sends_before_param() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.revisions/note.md"))
            .and(query_param("limit", "10"))
            .and(query_param("before", "a".repeat(40)))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                r#"{{"mode":"managed","uncommitted":false,"more":true,"revisions":[{{"rev":"{}","timestamp":1700000000000,"author":"alice","message":"edit","added":1,"removed":0}}]}}"#,
                "b".repeat(40)
            )))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let history = client
            .get_file_revisions("note.md", Some(&"a".repeat(40)), 10)
            .await
            .expect("history should parse");
        assert_eq!(history.revisions.len(), 1);
        assert_eq!(history.revisions[0].rev, "b".repeat(40));
        assert_eq!(history.revisions[0].author, "alice");
        assert!(history.more);
    }

    #[tokio::test]
    async fn get_file_revisions_maps_disabled_body_to_revisions_disabled_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.revisions/note.md"))
            .respond_with(
                ResponseTemplate::new(404).set_body_string(r#"{"error": "revisions disabled"}"#),
            )
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let err = client
            .get_file_revisions("note.md", None, 50)
            .await
            .expect_err("disabled body should error");
        assert!(matches!(err, SbError::RevisionsDisabled));
    }

    #[tokio::test]
    async fn get_file_revisions_plain_404_is_http_status_not_disabled() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.revisions/note.md"))
            .respond_with(ResponseTemplate::new(404).set_body_string("no repository"))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let err = client
            .get_file_revisions("note.md", None, 50)
            .await
            .expect_err("plain 404 should error");
        match err {
            SbError::HttpStatus { status, .. } => assert_eq!(status, 404),
            other => panic!("expected HttpStatus(404), got: {other:?}"),
        }
    }

    // --- get_revision_diff tests ---

    #[tokio::test]
    async fn get_revision_diff_with_rev_sends_rev_and_format_params() {
        let server = MockServer::start().await;
        let rev = "c".repeat(40);
        Mock::given(method("GET"))
            .and(path("/.revisions/note.md"))
            .and(query_param("format", "diff"))
            .and(query_param("rev", rev.clone()))
            .respond_with(ResponseTemplate::new(200).set_body_string("@@ -1 +1 @@\n-a\n+b\n"))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let diff = client
            .get_revision_diff("note.md", Some(&rev))
            .await
            .expect("diff should succeed")
            .expect("diff should be Some");
        assert!(diff.contains("@@"));
    }

    #[tokio::test]
    async fn get_revision_diff_without_rev_omits_rev_param() {
        let server = MockServer::start().await;
        // Constraining the mock to format=diff with no rev param IS the
        // assertion that the uncommitted-diff path omits `rev`.
        Mock::given(method("GET"))
            .and(path("/.revisions/note.md"))
            .and(query_param("format", "diff"))
            .respond_with(ResponseTemplate::new(200).set_body_string("+uncommitted\n"))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let diff = client
            .get_revision_diff("note.md", None)
            .await
            .expect("diff should succeed")
            .expect("diff should be Some");
        assert!(diff.contains("uncommitted"));
    }

    #[tokio::test]
    async fn get_revision_diff_404_with_empty_body_is_ok_none() {
        // Nothing to diff (matches HEAD, or a root/merge commit) -- not an error.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.revisions/note.md"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let diff = client
            .get_revision_diff("note.md", None)
            .await
            .expect("a benign 404 should not be an Err");
        assert!(diff.is_none());
    }

    #[tokio::test]
    async fn get_revision_diff_disabled_body_is_revisions_disabled_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.revisions/note.md"))
            .respond_with(
                ResponseTemplate::new(404).set_body_string(r#"{"error": "revisions disabled"}"#),
            )
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let err = client
            .get_revision_diff("note.md", None)
            .await
            .expect_err("disabled body should error even for diff");
        assert!(matches!(err, SbError::RevisionsDisabled));
    }

    // --- get_revision_content tests ---

    #[tokio::test]
    async fn get_revision_content_sends_rev_param_and_returns_bytes() {
        let server = MockServer::start().await;
        let rev = "d".repeat(40);
        Mock::given(method("GET"))
            .and(path("/.revisions/note.md"))
            .and(query_param("rev", rev.clone()))
            .respond_with(ResponseTemplate::new(200).set_body_string("old content"))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let bytes = client
            .get_revision_content("note.md", &rev)
            .await
            .expect("content fetch should succeed");
        assert_eq!(&bytes[..], b"old content");
    }

    #[tokio::test]
    async fn get_revision_content_unknown_rev_is_http_status_404() {
        let server = MockServer::start().await;
        let rev = "e".repeat(40);
        Mock::given(method("GET"))
            .and(path("/.revisions/note.md"))
            .and(query_param("rev", rev.clone()))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let err = client
            .get_revision_content("note.md", &rev)
            .await
            .expect_err("unknown rev should error");
        match err {
            SbError::HttpStatus { status, .. } => assert_eq!(status, 404),
            other => panic!("expected HttpStatus(404), got: {other:?}"),
        }
    }

    // --- ETag / conditional writes ---

    fn headers_with(name: &str, value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            reqwest::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
        h
    }

    #[test]
    fn etag_is_taken_verbatim_so_it_can_be_echoed_back() {
        let h = headers_with("ETag", "\"sha256:deadbeef\"");
        assert_eq!(
            etag_from_headers(&h),
            Some("\"sha256:deadbeef\"".to_string()),
            "quotes are part of the validator and must survive the round trip"
        );
    }

    #[test]
    fn missing_etag_header_yields_none() {
        // The 2.10.0 server: no ETag, so nothing to send back as If-Match.
        assert_eq!(etag_from_headers(&HeaderMap::new()), None);
    }

    #[test]
    fn empty_and_weak_etags_are_rejected() {
        assert_eq!(etag_from_headers(&headers_with("ETag", "")), None);
        assert_eq!(
            etag_from_headers(&headers_with("ETag", "W/\"sha256:abc\"")),
            None,
            "a weak validator is not usable in If-Match"
        );
    }

    /// A 403 means "read-only path" only when we were writing. On a read it is
    /// almost certainly an auth proxy rejecting the identity, and reporting
    /// "your token is fine" would send the user the wrong way.
    #[test]
    fn a_403_is_read_only_only_for_writes() {
        let url = "http://localhost:3000/.fs/page.md";
        assert!(matches!(
            status_error(StatusCode::FORBIDDEN, url, true),
            Some(SbError::ReadOnly { .. })
        ));
        assert!(matches!(
            status_error(StatusCode::FORBIDDEN, url, false),
            Some(SbError::AuthFailed { status: 403, .. })
        ));
        // The read mapping keeps the auth exit code a caller can branch on.
        assert_eq!(
            status_error(StatusCode::FORBIDDEN, url, false)
                .unwrap()
                .exit_code(),
            3
        );
    }

    #[test]
    fn status_error_maps_401_403_412_and_nothing_else() {
        let url = "http://localhost:3000/.fs/page.md";
        assert!(matches!(
            status_error(StatusCode::UNAUTHORIZED, url, true),
            Some(SbError::AuthFailed { status: 401, .. })
        ));
        assert!(matches!(
            status_error(StatusCode::FORBIDDEN, url, true),
            Some(SbError::ReadOnly { .. })
        ));
        assert!(matches!(
            status_error(StatusCode::PRECONDITION_FAILED, url, true),
            Some(SbError::PreconditionFailed { .. })
        ));
        for other in [
            StatusCode::OK,
            StatusCode::NOT_FOUND,
            StatusCode::CONFLICT,
            StatusCode::INTERNAL_SERVER_ERROR,
        ] {
            assert!(
                status_error(other, url, true).is_none(),
                "{other} should not map"
            );
        }
    }

    #[tokio::test]
    async fn get_file_returns_the_servers_etag_alongside_the_bytes() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.fs/notes/page.md"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("# Page")
                    .insert_header("ETag", "\"sha256:abc123\""),
            )
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let (bytes, etag) = client.get_file("notes/page.md").await.expect("get_file");
        assert_eq!(&bytes[..], b"# Page");
        assert_eq!(etag, Some("\"sha256:abc123\"".to_string()));
    }

    #[tokio::test]
    async fn get_file_returns_no_etag_from_a_server_that_sends_none() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.fs/notes/page.md"))
            .respond_with(ResponseTemplate::new(200).set_body_string("# Page"))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let (_bytes, etag) = client.get_file("notes/page.md").await.expect("get_file");
        assert_eq!(etag, None);
    }

    #[tokio::test]
    async fn put_file_sends_if_match_when_given_one() {
        let server = MockServer::start().await;
        // Only a PUT carrying this exact If-Match matches; anything else 404s.
        Mock::given(method("PUT"))
            .and(path("/.fs/page.md"))
            .and(header("If-Match", "\"sha256:old\""))
            .respond_with(ResponseTemplate::new(200).insert_header("ETag", "\"sha256:new\""))
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let new_etag = client
            .put_file(
                "page.md",
                bytes::Bytes::from_static(b"hi"),
                Some("\"sha256:old\""),
            )
            .await
            .expect("conditional put should succeed");
        assert_eq!(new_etag, Some("\"sha256:new\"".to_string()));
    }

    #[tokio::test]
    async fn put_file_sends_no_if_match_when_none() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/.fs/page.md"))
            .and(wiremock::matchers::header_exists("Content-Type"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        // Any request that did carry an If-Match would match this instead.
        Mock::given(method("PUT"))
            .and(path("/.fs/page.md"))
            .and(wiremock::matchers::header_exists("If-Match"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let etag = client
            .put_file("page.md", bytes::Bytes::from_static(b"hi"), None)
            .await
            .expect("unconditional put should succeed");
        assert_eq!(etag, None, "server sent no ETag, so we store none");
    }

    #[tokio::test]
    async fn put_file_returns_precondition_failed_on_412() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/.fs/page.md"))
            .respond_with(ResponseTemplate::new(412))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let err = client
            .put_file(
                "page.md",
                bytes::Bytes::from_static(b"hi"),
                Some("\"sha256:stale\""),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, SbError::PreconditionFailed { .. }));
        assert_eq!(err.exit_code(), 5, "412 is a conflict");
    }

    #[tokio::test]
    async fn put_file_returns_read_only_on_403() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/.fs/Library/Std/page.md"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        let err = client
            .put_file(
                "Library/Std/page.md",
                bytes::Bytes::from_static(b"hi"),
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, SbError::ReadOnly { .. }));
        assert_ne!(err.exit_code(), 3, "read-only is not an auth failure");
    }

    #[tokio::test]
    async fn delete_file_sends_if_match_when_given_one() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/.fs/page.md"))
            .and(header("If-Match", "\"sha256:known\""))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri(), "testtoken");
        client
            .delete_file("page.md", Some("\"sha256:known\""))
            .await
            .expect("conditional delete should succeed");
    }
}
