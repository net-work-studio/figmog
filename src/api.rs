//! Figma REST client. Everything network-facing sits behind [`FigmaApi`]
//! so the rest of the crate is testable offline.

use std::time::Duration;

use serde_json::Value;

mod http;
#[cfg(test)]
pub(crate) mod test_http;
#[cfg(test)]
pub(crate) mod test_https;

// Large file/image transfers get two minutes, including headers, redirects,
// and body reads. The shared transport enforces the deadline below TLS
// record processing and clamps each redirected connection to what remains.
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Errors surfaced by [`FigmaApi`] implementations.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("rate limited; retry after {retry_after:?}")]
    RateLimited { retry_after: Duration },
    #[error(
        "authentication failed — check your saved token (`figmog auth login`), FIGMA_TOKEN, and file access"
    )]
    Auth,
    #[error("figma returned {status}: {msg}")]
    Http { status: u16, msg: String },
    #[error("network error: {0}")]
    Network(String),
    #[error("unexpected response shape: {0}")]
    Parse(String),
}

/// Subset of `GET /v1/files/:key/meta` figmog needs.
#[derive(Debug, Clone, PartialEq)]
pub struct FileMetaResp {
    pub name: String,
    /// "The UTC ISO 8601 time at which the file content was last modified."
    pub last_touched_at: String,
}

/// The calls figmog makes. `file_meta` is Tier 3 (cheap, poll it); `file`
/// is Tier 1 (expensive, call only on change); `variables_local` is Tier 2
/// and Enterprise-only, called opportunistically by `pull` (build design
/// §12).
pub trait FigmaApi {
    /// `GET /v1/files/:key/meta` — Tier 3, cheap enough to poll.
    fn file_meta(&self, key: &str) -> Result<FileMetaResp, ApiError>;
    /// `GET /v1/files/:key` — Tier 1, the full document tree. `geometry`
    /// (v0.0.2 spec §4) adds `?geometry=paths`, so vector nodes carry
    /// `fillGeometry`/`strokeGeometry` path data in the response — the
    /// caller (`cli::pull::do_pull`, `sessions`'s pull worker) is
    /// responsible for combining this call's own override with whatever
    /// geometry setting is already stored (`store::effective_geometry`)
    /// before deciding what to pass here; this trait method itself just
    /// forwards the bit to the request.
    fn file(&self, key: &str, geometry: bool) -> Result<Value, ApiError>;
    /// `GET /v1/files/:key/variables/local` — Enterprise-only. `Ok(None)`
    /// means "not available on this plan" (never an error: `pull` falls
    /// back to v1 behavior). The default implementation always returns
    /// `Ok(None)`, so test doubles that only care about `file`/`file_meta`
    /// (e.g. `watch::tests::Script`) don't need to know this call exists.
    fn variables_local(&self, key: &str) -> Result<Option<Value>, ApiError> {
        let _ = key;
        Ok(None)
    }
    /// `GET /v1/images/:key?ids=…&format=…[&scale=…][&version=…]` (v0.0.2
    /// spec §5) —
    /// node renders. Unlike `variables_local`'s "not on this plan, and
    /// that's fine" gating, an unimplemented render endpoint is a genuine
    /// capability gap for any caller that reaches it, so the default
    /// returns an error rather than `Ok(None)`: existing `FigmaApi` test
    /// doubles built before this method existed (e.g. `watch::tests::
    /// Script`) keep compiling without knowing renders exist at all, and
    /// `crate::images::fetch` surfaces this as a normal per-item manifest
    /// error rather than a panic if a caller ever did reach it.
    ///
    /// `version` pins the render to that file version (NWS-2960): callers
    /// pass the mirrored `FileMeta::version` so the bytes match what the
    /// cache row is labelled with, even if Figma has moved on since.
    fn images_render(
        &self,
        key: &str,
        ids: &[String],
        format: &str,
        scale: Option<f64>,
        version: Option<&str>,
    ) -> Result<Value, ApiError> {
        let _ = (key, ids, format, scale, version);
        Err(ApiError::Http {
            status: 501,
            msg: "images_render not implemented".into(),
        })
    }
    /// `GET /v1/files/:key/images` (v0.0.2 spec §5) — the file's fill
    /// image hash→URL map. Same default-Err reasoning as
    /// [`Self::images_render`].
    fn file_image_fills(&self, key: &str) -> Result<Value, ApiError> {
        let _ = key;
        Err(ApiError::Http {
            status: 501,
            msg: "file_image_fills not implemented".into(),
        })
    }
}

pub(crate) fn parse_meta_response(v: &Value) -> Result<FileMetaResp, ApiError> {
    let file = v
        .get("file")
        .ok_or_else(|| ApiError::Parse("no `file` object".into()))?;
    let get = |k: &str| {
        file.get(k)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| ApiError::Parse(format!("meta missing `{k}`")))
    };
    Ok(FileMetaResp {
        name: get("name")?,
        last_touched_at: get("last_touched_at")?,
    })
}

/// `GET /v1/files/:key` request path, with `?geometry=paths` appended when
/// `geometry` (v0.0.2 spec §4). Split out from [`UreqApi::file`] as a pure
/// function so query-parameter construction can be checked without an
/// HTTP request (spec §4's stickiness tests lean on this for the "what
/// would this pull have asked for" half; the from-file path never issues
/// the request at all).
pub(crate) fn file_url(key: &str, geometry: bool) -> String {
    if geometry {
        format!("/v1/files/{key}?geometry=paths")
    } else {
        format!("/v1/files/{key}")
    }
}

/// `GET /v1/images/:key` request path (v0.0.2 spec §5) — comma-joined
/// `ids`, `format`, `scale` when given, and `version` when given
/// (NWS-2960: pins the render to the mirrored file version; percent-encoded
/// since it's an opaque upstream string). Split out as a pure function for
/// the same reason as [`file_url`]: unit-testable request-shape coverage
/// without a live HTTP call.
pub(crate) fn images_render_url(
    key: &str,
    ids: &[String],
    format: &str,
    scale: Option<f64>,
    version: Option<&str>,
) -> String {
    let ids_param = ids.join(",");
    let mut url = format!("/v1/images/{key}?ids={ids_param}&format={format}");
    if let Some(s) = scale {
        url.push_str(&format!("&scale={s}"));
    }
    if let Some(v) = version {
        url.push_str("&version=");
        url.push_str(&encode_query_value(v));
    }
    url
}

/// Percent-encode a query-parameter value: RFC 3986 unreserved bytes pass
/// through, every other byte becomes `%XX` (uppercase hex).
fn encode_query_value(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for b in v.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `GET /v1/files/:key/images` request path (v0.0.2 spec §5) — the fill
/// image hash→URL map, no query parameters.
pub(crate) fn file_image_fills_url(key: &str) -> String {
    format!("/v1/files/{key}/images")
}

pub(crate) fn error_from_status(status: u16, retry_after: Option<&str>, msg: String) -> ApiError {
    match status {
        429 => ApiError::RateLimited {
            retry_after: Duration::from_secs(
                retry_after
                    .and_then(|s| s.trim().parse().ok())
                    .unwrap_or(60),
            ),
        },
        401 | 403 => ApiError::Auth,
        _ => ApiError::Http { status, msg },
    }
}

/// Debug builds only: base URL for serve's sessions instead of
/// `https://api.figma.com` (tests' synthetic Figma server).
pub(crate) const API_BASE_ENV: &str = "FIGMOG_API_BASE";

/// Blocking `ureq` implementation against api.figma.com.
pub struct UreqApi {
    token: String,
    base_url: String,
    http: http::Client,
}

impl UreqApi {
    /// Client against the real `api.figma.com`, authenticated with a
    /// personal access token (saved credential or `FIGMA_TOKEN`).
    pub fn new(token: String) -> Self {
        Self::with_base_url(token, "https://api.figma.com".into())
    }
    /// The client serve's sessions use: the real API, except that debug
    /// builds honor [`API_BASE_ENV`] so end-to-end tests can point a
    /// detached background owner (NWS-2953) at a synthetic Figma server.
    /// Release builds ignore it: a token is never sent elsewhere by an
    /// environment variable.
    pub(crate) fn for_sessions(token: String) -> Self {
        #[cfg(debug_assertions)]
        if let Ok(base) = std::env::var(API_BASE_ENV)
            && !base.is_empty()
        {
            return Self::with_base_url(token, base);
        }
        Self::new(token)
    }
    /// `base_url` override for tests / proxies.
    pub fn with_base_url(token: String, base_url: String) -> Self {
        UreqApi {
            token,
            base_url,
            http: http::Client::new(HTTP_REQUEST_TIMEOUT),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_timeout(mut self, timeout: Duration) -> Self {
        self.http.timeout = timeout;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_tls_config(
        mut self,
        config: std::sync::Arc<ureq::rustls::ClientConfig>,
    ) -> Self {
        self.http.tls = config;
        self
    }

    fn get_json(&self, path: &str) -> Result<Value, ApiError> {
        let url = format!("{}{}", self.base_url, path);
        self.http
            .get(&url, Some(&self.token))?
            .into_json()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::TimedOut {
                    ApiError::Network(e.to_string())
                } else {
                    ApiError::Parse(e.to_string())
                }
            })
    }
}

impl FigmaApi for UreqApi {
    fn file_meta(&self, key: &str) -> Result<FileMetaResp, ApiError> {
        parse_meta_response(&self.get_json(&format!("/v1/files/{key}/meta"))?)
    }
    fn file(&self, key: &str, geometry: bool) -> Result<Value, ApiError> {
        self.get_json(&file_url(key, geometry))
    }
    fn variables_local(&self, key: &str) -> Result<Option<Value>, ApiError> {
        match self.get_json(&format!("/v1/files/{key}/variables/local")) {
            Ok(v) => Ok(Some(v)),
            Err(e) if variables_local_is_gated(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }
    fn images_render(
        &self,
        key: &str,
        ids: &[String],
        format: &str,
        scale: Option<f64>,
        version: Option<&str>,
    ) -> Result<Value, ApiError> {
        self.get_json(&images_render_url(key, ids, format, scale, version))
    }
    fn file_image_fills(&self, key: &str) -> Result<Value, ApiError> {
        self.get_json(&file_image_fills_url(key))
    }
}

/// Plain byte download for an already-resolved image URL (v0.0.2 spec
/// §5): the URLs `images_render`/`file_image_fills` return are pre-signed
/// S3 links, not `api.figma.com` — no `X-Figma-Token` header (S3 rejects
/// an unexpected auth header on a pre-signed URL), and no `base_url`
/// override (these are always absolute URLs from Figma's own response,
/// never a path this crate constructs). A free function rather than a
/// `FigmaApi` method: it isn't a Figma API call at all, and keeping it
/// free lets `crate::images::fetch` accept it as a plain
/// `&dyn Fn(&str) -> Result<Vec<u8>, ApiError>`, injectable by tests
/// without a `FigmaApi` implementor standing in for a plain HTTP GET.
pub fn download_bytes(url: &str) -> Result<Vec<u8>, ApiError> {
    download_bytes_with_timeout(url, HTTP_REQUEST_TIMEOUT)
}

fn download_bytes_with_timeout(url: &str, timeout: Duration) -> Result<Vec<u8>, ApiError> {
    download_bytes_with_client(url, &http::Client::new(timeout))
}

fn download_bytes_with_client(url: &str, client: &http::Client) -> Result<Vec<u8>, ApiError> {
    let resp = client.get(url, None)?;
    let mut buf = Vec::new();
    use std::io::Read;
    resp.into_reader()
        .read_to_end(&mut buf)
        .map_err(|e| ApiError::Network(e.to_string()))?;
    Ok(buf)
}

/// Whether a `variables_local` failure means "this plan can't see
/// variables" (403/404 — skip silently, v1 behavior holds) rather than a
/// real failure `pull` should propagate. By the time `variables_local` is
/// called, `file()` has already succeeded against the same token, so a 401/
/// 403 here means plan gating, not a bad token — `error_from_status` maps
/// both to `ApiError::Auth`.
fn variables_local_is_gated(e: &ApiError) -> bool {
    matches!(e, ApiError::Auth) || matches!(e, ApiError::Http { status: 404, .. })
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_http::{Peer, RESPONSE_BOUND, Reply, TEST_TIMEOUT};

    #[test]
    fn tls_handshake_trickles_cannot_renew_the_request_deadline() {
        for download in [false, true] {
            let peer = test_https::TlsPeer::new(vec![test_https::Reply::HandshakeTrickle]);
            let api = UreqApi::with_base_url("synthetic-token".into(), peer.url.clone())
                .with_timeout(TEST_TIMEOUT);
            let start = std::time::Instant::now();
            let err = if download {
                download_bytes_with_timeout(&peer.url, TEST_TIMEOUT).unwrap_err()
            } else {
                api.file("ABC123", false).unwrap_err()
            };
            assert!(
                start.elapsed() < Duration::from_secs(1),
                "TLS exceeded budget: {:?}",
                start.elapsed()
            );
            assert!(matches!(err, ApiError::Network(_)), "{err:?}");
        }
    }

    #[test]
    fn encrypted_body_fragments_cannot_renew_the_request_deadline() {
        for download in [false, true] {
            let peer = test_https::TlsPeer::new(vec![test_https::Reply::EncryptedTrickle(
                "HTTP/1.1 200 OK\r\nContent-Length: 10000\r\nConnection: close\r\n\r\n",
            )]);
            let api = UreqApi::with_base_url("synthetic-token".into(), peer.url.clone())
                .with_timeout(TEST_TIMEOUT)
                .with_tls_config(test_https::client_config());
            let start = std::time::Instant::now();
            let err = if download {
                download_bytes_with_client(&peer.url, &api.http).unwrap_err()
            } else {
                api.file("ABC123", false).unwrap_err()
            };
            // Prove a trusted handshake and HTTP headers completed: a
            // certificate rejection must not masquerade as a timeout.
            peer.requests
                .recv_timeout(Duration::from_secs(1))
                .unwrap_or_else(|e| panic!("HTTPS request not received: {e}; client: {err:?}"));
            assert!(
                start.elapsed() < Duration::from_secs(1),
                "encrypted body exceeded budget: {:?}",
                start.elapsed()
            );
            assert!(matches!(err, ApiError::Network(_)), "{err:?}");
        }
    }

    #[test]
    fn redirected_connect_uses_the_remaining_request_budget() {
        let budget = Duration::from_secs(1);
        for download in [false, true] {
            let blocked = test_http::StalledConnect::new();
            let peer = Peer::new(vec![Reply::Redirect {
                location: blocked.url.clone(),
                after: Duration::from_millis(700),
            }]);
            let api = UreqApi::with_base_url("synthetic-token".into(), peer.url.clone())
                .with_timeout(budget);
            let start = std::time::Instant::now();
            let err = if download {
                download_bytes_with_timeout(&peer.url, budget).unwrap_err()
            } else {
                api.file("ABC123", false).unwrap_err()
            };
            assert!(
                start.elapsed() < Duration::from_millis(1400),
                "redirect exceeded budget: {:?}",
                start.elapsed()
            );
            // Resetting even a one-second connect budget after this
            // redirect would take 1.7s, beyond the assertion above.
            assert!(start.elapsed() >= Duration::from_millis(900));
            assert!(matches!(err, ApiError::Network(_)), "{err:?}");
        }
    }

    #[test]
    fn encrypted_error_body_timeout_preserves_retry_after() {
        for download in [false, true] {
            let peer = test_https::TlsPeer::new(vec![test_https::Reply::EncryptedTrickle(
                "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 17\r\nContent-Length: 10000\r\nConnection: close\r\n\r\n",
            )]);
            let api = UreqApi::with_base_url("synthetic-token".into(), peer.url.clone())
                .with_timeout(TEST_TIMEOUT)
                .with_tls_config(test_https::client_config());
            let start = std::time::Instant::now();
            let err = if download {
                download_bytes_with_client(&peer.url, &api.http).unwrap_err()
            } else {
                api.file("ABC123", false).unwrap_err()
            };
            assert!(start.elapsed() < Duration::from_secs(1));
            assert!(
                matches!(err, ApiError::RateLimited { retry_after }
                if retry_after == Duration::from_secs(17)),
                "{err:?}"
            );
        }
    }

    #[test]
    fn https_keeps_certificate_verification_and_successful_body_reads() {
        for trusted in [false, true] {
            let peer = test_https::TlsPeer::new(vec![test_https::Reply::Complete(
                "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{\"ok\":true}",
            )]);
            let mut api = UreqApi::with_base_url("synthetic-token".into(), peer.url.clone())
                .with_timeout(TEST_TIMEOUT);
            if trusted {
                api = api.with_tls_config(test_https::client_config());
            }
            match api.file("ABC123", false) {
                Ok(value) => {
                    assert!(trusted);
                    assert_eq!(value, serde_json::json!({"ok":true}));
                }
                Err(ApiError::Network(message)) => {
                    assert!(!trusted, "{message}");
                    assert!(message.contains("certificate"), "{message}");
                }
                other => panic!("unexpected HTTPS result: {other:?}"),
            }
        }
        let peer = test_https::TlsPeer::new(vec![test_https::Reply::Complete(
            "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nPNG",
        )]);
        let api = UreqApi::with_base_url("synthetic-token".into(), peer.url.clone())
            .with_timeout(TEST_TIMEOUT)
            .with_tls_config(test_https::client_config());
        assert_eq!(
            download_bytes_with_client(&peer.url, &api.http).unwrap(),
            b"PNG"
        );
        assert!(
            !peer
                .requests
                .recv_timeout(RESPONSE_BOUND)
                .unwrap()
                .to_ascii_lowercase()
                .contains("x-figma-token")
        );
    }

    #[test]
    fn relative_redirects_and_redirect_limits_are_preserved() {
        let peer = Peer::new(vec![
            Reply::Redirect {
                location: "../redirected".into(),
                after: Duration::ZERO,
            },
            Reply::Complete("HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{}"),
        ]);
        let api = UreqApi::with_base_url("synthetic-token".into(), peer.url.clone())
            .with_timeout(TEST_TIMEOUT);
        assert_eq!(api.file("ABC123", false).unwrap(), serde_json::json!({}));
        let _ = peer.requests.recv_timeout(RESPONSE_BOUND).unwrap();
        let redirected = peer.requests.recv_timeout(RESPONSE_BOUND).unwrap();
        assert!(redirected.starts_with("GET /v1/redirected "));
        assert!(
            redirected
                .to_ascii_lowercase()
                .contains("x-figma-token: synthetic-token")
        );

        let peer = Peer::new(
            (0..5)
                .map(|_| Reply::Redirect {
                    location: "/again".into(),
                    after: Duration::ZERO,
                })
                .collect(),
        );
        let api = UreqApi::with_base_url("synthetic-token".into(), peer.url.clone())
            .with_timeout(Duration::from_secs(2));
        assert!(
            matches!(api.file("ABC123", false), Err(ApiError::Network(message))
            if message.contains("max redirects"))
        );
    }

    #[test]
    fn rest_withheld_headers_time_out() {
        let peer = Peer::new(vec![Reply::Stall("")]);
        let api = UreqApi::with_base_url("synthetic-token".into(), peer.url.clone())
            .with_timeout(TEST_TIMEOUT);
        let start = std::time::Instant::now();
        let result = api.file("ABC123", false);
        assert!(
            start.elapsed() < RESPONSE_BOUND,
            "stalled headers exceeded budget"
        );
        assert!(matches!(result, Err(ApiError::Network(_))), "{result:?}");
    }

    #[test]
    fn rest_stalled_success_body_is_a_network_error() {
        let peer = Peer::new(vec![Reply::Stall(
            "HTTP/1.1 200 OK\r\nContent-Length: 10000\r\nConnection: close\r\n\r\n{",
        )]);
        let api = UreqApi::with_base_url("synthetic-token".into(), peer.url.clone())
            .with_timeout(TEST_TIMEOUT);
        let start = std::time::Instant::now();
        let result = api.file("ABC123", false);
        assert!(
            start.elapsed() < RESPONSE_BOUND,
            "stalled body exceeded budget"
        );
        assert!(matches!(result, Err(ApiError::Network(_))), "{result:?}");
    }

    #[test]
    fn image_stalled_body_times_out() {
        let peer = Peer::new(vec![Reply::Stall(
            "HTTP/1.1 200 OK\r\nContent-Length: 10000\r\nConnection: close\r\n\r\nPNG",
        )]);
        let start = std::time::Instant::now();
        let result = download_bytes_with_timeout(&peer.url, TEST_TIMEOUT);
        assert!(
            start.elapsed() < RESPONSE_BOUND,
            "image body exceeded budget"
        );
        assert!(matches!(result, Err(ApiError::Network(_))), "{result:?}");
    }

    #[test]
    fn image_withheld_headers_time_out() {
        let peer = Peer::new(vec![Reply::Stall("")]);
        let start = std::time::Instant::now();
        let result = download_bytes_with_timeout(&peer.url, TEST_TIMEOUT);
        assert!(start.elapsed() < RESPONSE_BOUND);
        assert!(matches!(result, Err(ApiError::Network(_))), "{result:?}");
    }

    #[test]
    fn slow_trickles_cannot_renew_rest_or_image_deadlines() {
        for download in [false, true] {
            let peer = Peer::new(vec![Reply::Trickle(
                "HTTP/1.1 200 OK\r\nContent-Length: 10000\r\nConnection: close\r\n\r\n",
            )]);
            let api = UreqApi::with_base_url("synthetic-token".into(), peer.url.clone())
                .with_timeout(TEST_TIMEOUT);
            let start = std::time::Instant::now();
            let result = if download {
                download_bytes_with_timeout(&peer.url, TEST_TIMEOUT).map(|_| ())
            } else {
                api.file("ABC123", false).map(|_| ())
            };
            assert!(
                start.elapsed() < RESPONSE_BOUND,
                "trickle renewed the budget"
            );
            assert!(matches!(result, Err(ApiError::Network(_))), "{result:?}");
        }
    }

    #[test]
    fn stalled_error_bodies_preserve_status_and_retry_after() {
        for download in [false, true] {
            for (response, status) in [
                (
                    "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 17\r\nContent-Length: 10000\r\nConnection: close\r\n\r\n",
                    429,
                ),
                (
                    "HTTP/1.1 403 Forbidden\r\nContent-Length: 10000\r\nConnection: close\r\n\r\n",
                    403,
                ),
                (
                    "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 10000\r\nConnection: close\r\n\r\n",
                    500,
                ),
            ] {
                let peer = Peer::new(vec![Reply::Stall(response)]);
                let api = UreqApi::with_base_url("synthetic-token".into(), peer.url.clone())
                    .with_timeout(TEST_TIMEOUT);
                let start = std::time::Instant::now();
                let err = if download {
                    download_bytes_with_timeout(&peer.url, TEST_TIMEOUT).unwrap_err()
                } else {
                    api.file("ABC123", false).unwrap_err()
                };
                assert!(
                    start.elapsed() < RESPONSE_BOUND,
                    "error body exceeded budget"
                );
                match status {
                    429 => assert!(matches!(err, ApiError::RateLimited { retry_after }
                        if retry_after == Duration::from_secs(17))),
                    403 => assert!(matches!(err, ApiError::Auth)),
                    500 => assert!(matches!(err, ApiError::Http { status: 500, .. })),
                    _ => unreachable!(),
                }
            }
        }
    }

    #[test]
    fn trickling_error_bodies_also_keep_the_overall_deadline() {
        for download in [false, true] {
            let peer = Peer::new(vec![Reply::Trickle(
                "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 19\r\nContent-Length: 10000\r\nConnection: close\r\n\r\n",
            )]);
            let api = UreqApi::with_base_url("synthetic-token".into(), peer.url.clone())
                .with_timeout(TEST_TIMEOUT);
            let start = std::time::Instant::now();
            let err = if download {
                download_bytes_with_timeout(&peer.url, TEST_TIMEOUT).unwrap_err()
            } else {
                api.file("ABC123", false).unwrap_err()
            };
            assert!(start.elapsed() < RESPONSE_BOUND);
            assert!(matches!(err, ApiError::RateLimited { retry_after }
                if retry_after == Duration::from_secs(19)));
        }
    }

    #[test]
    fn completed_responses_keep_json_bytes_and_auth_behavior() {
        let peer = Peer::new(vec![Reply::Complete(
            "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{\"file\":{\"name\":\"Fixture\",\"last_touched_at\":\"t\"}}",
        )]);
        let api = UreqApi::with_base_url("synthetic-token".into(), peer.url.clone())
            .with_timeout(TEST_TIMEOUT);
        assert_eq!(api.file_meta("ABC123").unwrap().name, "Fixture");
        let request = peer.requests.recv_timeout(RESPONSE_BOUND).unwrap();
        assert!(request.starts_with("GET /v1/files/ABC123/meta "));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("x-figma-token: synthetic-token")
        );

        let peer = Peer::new(vec![Reply::Complete(
            "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nPNG",
        )]);
        assert_eq!(download_bytes(&peer.url).unwrap(), b"PNG");
        let request = peer.requests.recv_timeout(RESPONSE_BOUND).unwrap();
        assert!(!request.to_ascii_lowercase().contains("x-figma-token"));
    }

    #[test]
    fn malformed_json_stays_a_parse_error() {
        let peer = Peer::new(vec![Reply::Complete(
            "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nnot json",
        )]);
        let api = UreqApi::with_base_url("synthetic-token".into(), peer.url.clone())
            .with_timeout(TEST_TIMEOUT);
        assert!(matches!(api.file("ABC123", false), Err(ApiError::Parse(_))));
    }

    #[test]
    fn parses_meta_envelope() {
        let v = serde_json::json!({
            "file": {"name": "F", "last_touched_at": "2026-08-01T00:00:00Z", "folder_name": "x"}
        });
        let m = parse_meta_response(&v).unwrap();
        assert_eq!(m.name, "F");
        assert_eq!(m.last_touched_at, "2026-08-01T00:00:00Z");
    }

    #[test]
    fn meta_missing_fields_is_parse_error() {
        assert!(matches!(
            parse_meta_response(&serde_json::json!({"file": {}})),
            Err(ApiError::Parse(_))
        ));
    }

    #[test]
    fn error_from_status_maps_429_and_403() {
        assert!(matches!(
            error_from_status(429, Some("30"), "slow down".into()),
            ApiError::RateLimited { retry_after } if retry_after == std::time::Duration::from_secs(30)
        ));
        // absent/garbage Retry-After falls back to 60s
        assert!(matches!(
            error_from_status(429, None, String::new()),
            ApiError::RateLimited { retry_after } if retry_after == std::time::Duration::from_secs(60)
        ));
        assert!(matches!(
            error_from_status(403, None, String::new()),
            ApiError::Auth
        ));
        assert!(matches!(
            error_from_status(500, None, "boom".into()),
            ApiError::Http { status: 500, .. }
        ));
    }

    #[test]
    fn variables_local_gated_on_403_and_404_only() {
        assert!(variables_local_is_gated(&ApiError::Auth));
        assert!(variables_local_is_gated(&ApiError::Http {
            status: 404,
            msg: String::new()
        }));
        assert!(!variables_local_is_gated(&ApiError::Http {
            status: 500,
            msg: "boom".into()
        }));
        assert!(!variables_local_is_gated(&ApiError::Network("down".into())));
        assert!(!variables_local_is_gated(&ApiError::RateLimited {
            retry_after: Duration::from_secs(1)
        }));
    }

    /// A test double that only implements the two calls it needs — proving
    /// the trait's default `variables_local` (used by e.g.
    /// `watch::tests::Script`) is `Ok(None)` without requiring every
    /// `FigmaApi` implementor to know the Enterprise endpoint exists.
    struct NoVariablesOverride;
    impl FigmaApi for NoVariablesOverride {
        fn file_meta(&self, _key: &str) -> Result<FileMetaResp, ApiError> {
            unimplemented!("not exercised by this test")
        }
        fn file(&self, _key: &str, _geometry: bool) -> Result<Value, ApiError> {
            unimplemented!("not exercised by this test")
        }
    }

    #[test]
    fn default_variables_local_is_none() {
        assert!(matches!(
            NoVariablesOverride.variables_local("ABC123"),
            Ok(None)
        ));
    }

    // ---- sticky vector geometry (v0.0.2 spec §4) ----

    #[test]
    fn file_url_adds_geometry_paths_only_when_requested() {
        assert_eq!(file_url("ABC123", false), "/v1/files/ABC123");
        assert_eq!(file_url("ABC123", true), "/v1/files/ABC123?geometry=paths");
    }

    // ---- image bytes (v0.0.2 spec §5) ----

    #[test]
    fn images_render_url_joins_ids_and_omits_scale_when_absent() {
        let ids = vec!["1:2".to_string(), "1:3".to_string()];
        assert_eq!(
            images_render_url("ABC123", &ids, "png", None, None),
            "/v1/images/ABC123?ids=1:2,1:3&format=png"
        );
    }

    #[test]
    fn images_render_url_appends_scale_when_given() {
        let ids = vec!["1:2".to_string()];
        assert_eq!(
            images_render_url("ABC123", &ids, "svg", Some(2.0), None),
            "/v1/images/ABC123?ids=1:2&format=svg&scale=2"
        );
    }

    /// NWS-2960: the render request carries the mirrored file version, so
    /// Figma renders that version rather than the file's latest.
    #[test]
    fn images_render_url_carries_the_pinned_version() {
        let ids = vec!["1:2".to_string()];
        assert_eq!(
            images_render_url("ABC123", &ids, "png", None, Some("1234567890")),
            "/v1/images/ABC123?ids=1:2&format=png&version=1234567890"
        );
        assert_eq!(
            images_render_url("ABC123", &ids, "png", Some(2.0), Some("v 1&x=2")),
            "/v1/images/ABC123?ids=1:2&format=png&scale=2&version=v%201%26x%3D2"
        );
    }

    #[test]
    fn file_image_fills_url_has_no_query() {
        assert_eq!(file_image_fills_url("ABC123"), "/v1/files/ABC123/images");
    }

    /// Default (unimplemented) `images_render`/`file_image_fills`
    /// return an error, not `Ok(None)` — see this trait's own doc comment
    /// on why that differs from `variables_local`'s default.
    #[test]
    fn default_images_render_and_file_image_fills_are_errors() {
        assert!(
            NoVariablesOverride
                .images_render("ABC", &[], "png", None, Some("1"))
                .is_err()
        );
        assert!(NoVariablesOverride.file_image_fills("ABC").is_err());
    }
}
