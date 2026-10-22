//! The [`Client`] and its builder.

use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, RETRY_AFTER};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use url::Url;

use crate::error::{ApiError, Error, Result};
use crate::rate_limit::{RateLimit, RateLimiter};
use crate::retry::RetryPolicy;

const DEFAULT_BASE_URL: &str = "https://api.alldebrid.com";
const DEFAULT_USER_AGENT: &str = concat!("alldebrid-rs/", env!("CARGO_PKG_VERSION"));
const MAX_BODY_IN_ERROR: usize = 2048;

/// API version an endpoint lives under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiVersion {
    /// `/v4/`
    V4,
    /// `/v4.1/`
    V4_1,
}

impl ApiVersion {
    fn as_str(self) -> &'static str {
        match self {
            ApiVersion::V4 => "v4",
            ApiVersion::V4_1 => "v4.1",
        }
    }
}

/// Async AllDebrid API client.
///
/// Cheap to clone: clones share the HTTP connection pool and the rate limiter.
///
/// ```no_run
/// # async fn run() -> alldebrid::Result<()> {
/// let client = alldebrid::Client::new("my-api-key")?;
/// let uploaded = client.upload_magnets(&["magnet:?xt=urn:btih:..."]).await?;
/// # Ok(()) }
/// ```
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

struct Inner {
    http: reqwest::Client,
    base_url: Url,
    api_key: String,
    retry: RetryPolicy,
    limiter: Option<RateLimiter>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("base_url", &self.inner.base_url.as_str())
            .field("api_key", &"<redacted>")
            .field("retry", &self.inner.retry)
            .finish_non_exhaustive()
    }
}

/// Builder for [`Client`].
#[derive(Debug, Default)]
#[must_use]
pub struct ClientBuilder {
    api_key: Option<String>,
    base_url: Option<String>,
    user_agent: Option<String>,
    timeout: Option<Duration>,
    retry: Option<RetryPolicy>,
    rate_limit: Option<Option<RateLimit>>,
    http: Option<reqwest::Client>,
}

impl ClientBuilder {
    /// API key from <https://alldebrid.com/apikeys/>. Required.
    pub fn api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    /// Override the API base URL (default `https://api.alldebrid.com`). Useful for tests.
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self
    }

    /// `User-Agent` header. Ignored when a custom [`http_client`](Self::http_client) is set.
    pub fn user_agent(mut self, ua: impl Into<String>) -> Self {
        self.user_agent = Some(ua.into());
        self
    }

    /// Per-request timeout (default 30 s). Ignored when a custom
    /// [`http_client`](Self::http_client) is set.
    pub fn timeout(mut self, t: Duration) -> Self {
        self.timeout = Some(t);
        self
    }

    /// Retry policy for transient failures (default [`RetryPolicy::default`]).
    pub fn retry(mut self, policy: RetryPolicy) -> Self {
        self.retry = Some(policy);
        self
    }

    /// Client-side rate limit (default 12/s and 600/min).
    pub fn rate_limit(mut self, limit: RateLimit) -> Self {
        self.rate_limit = Some(Some(limit));
        self
    }

    /// Disable client-side rate limiting.
    pub fn no_rate_limit(mut self) -> Self {
        self.rate_limit = Some(None);
        self
    }

    /// Use a pre-configured `reqwest::Client` (proxies, custom TLS...).
    pub fn http_client(mut self, client: reqwest::Client) -> Self {
        self.http = Some(client);
        self
    }

    /// Build the client.
    pub fn build(self) -> Result<Client> {
        let api_key = self
            .api_key
            .filter(|k| !k.trim().is_empty())
            .ok_or_else(|| Error::Config("api_key is required".into()))?;

        let mut base_url = Url::parse(self.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL))
            .map_err(|e| Error::Config(format!("invalid base_url: {e}")))?;
        if !base_url.path().ends_with('/') {
            let p = format!("{}/", base_url.path());
            base_url.set_path(&p);
        }

        let http = match self.http {
            Some(c) => c,
            None => reqwest::Client::builder()
                .user_agent(self.user_agent.as_deref().unwrap_or(DEFAULT_USER_AGENT))
                .timeout(self.timeout.unwrap_or(Duration::from_secs(30)))
                .build()
                .map_err(|e| Error::Config(format!("could not build HTTP client: {e}")))?,
        };

        let limiter = self
            .rate_limit
            .unwrap_or_else(|| Some(RateLimit::default()))
            .map(|l| RateLimiter::new(&l));

        Ok(Client {
            inner: Arc::new(Inner {
                http,
                base_url,
                api_key,
                retry: self.retry.unwrap_or_default(),
                limiter,
            }),
        })
    }
}

/// Request body.
#[derive(Clone)]
pub(crate) enum Body {
    Form(Vec<(String, String)>),
    /// `(field name, file name, bytes)`
    Multipart(Vec<(String, String, Vec<u8>)>),
}

/// Whether a request may be repeated after an ambiguous failure
/// (i.e. one where the server may already have processed it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Idempotency {
    Safe,
    /// Only retry when we know the server did not process the request
    /// (connection failures, HTTP 429).
    Unsafe,
}

#[derive(Deserialize)]
struct Envelope {
    status: String,
    data: Option<serde_json::Value>,
    error: Option<ApiError>,
    #[serde(default)]
    deprecated: bool,
}

enum Attempt {
    Done(serde_json::Value),
    Retry { err: Error, after: Option<Duration> },
    Fail(Error),
}

impl Client {
    /// Client with default settings.
    pub fn new(api_key: impl Into<String>) -> Result<Self> {
        Self::builder().api_key(api_key).build()
    }

    /// Start building a client.
    pub fn builder() -> ClientBuilder {
        ClientBuilder::default()
    }

    /// Call any endpoint and deserialize its `data` field.
    ///
    /// Escape hatch for endpoints this crate does not wrap yet. Parameters are sent
    /// as a `POST` form; repeat a key with a `[]` suffix for arrays (`("id[]", "1")`).
    pub async fn raw<T: DeserializeOwned>(
        &self,
        version: ApiVersion,
        path: &str,
        params: &[(&str, &str)],
    ) -> Result<T> {
        let form = params
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        self.call(version, path, Body::Form(form), Idempotency::Safe)
            .await
    }

    pub(crate) async fn call<T: DeserializeOwned>(
        &self,
        version: ApiVersion,
        path: &str,
        body: Body,
        idem: Idempotency,
    ) -> Result<T> {
        let data = self.call_value(version, path, body, idem).await?;
        serde_json::from_value(data.clone()).map_err(|source| Error::Decode {
            source,
            body: truncate(data.to_string()),
        })
    }

    async fn call_value(
        &self,
        version: ApiVersion,
        path: &str,
        body: Body,
        idem: Idempotency,
    ) -> Result<serde_json::Value> {
        let url = self
            .inner
            .base_url
            .join(&format!(
                "{}/{}",
                version.as_str(),
                path.trim_start_matches('/')
            ))
            .map_err(|e| Error::Config(format!("invalid endpoint {path}: {e}")))?;

        let mut attempt = 0;
        loop {
            if let Some(l) = &self.inner.limiter {
                l.acquire().await;
            }
            match self.attempt(&url, &body, idem).await {
                Attempt::Done(v) => return Ok(v),
                Attempt::Fail(e) => return Err(e),
                Attempt::Retry { err, after } => {
                    if attempt >= self.inner.retry.max_retries {
                        return Err(err);
                    }
                    let delay = after.unwrap_or_else(|| self.inner.retry.delay(attempt));
                    tracing::debug!(%url, attempt, ?delay, error = %err, "retrying AllDebrid request");
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
            }
        }
    }

    async fn attempt(&self, url: &Url, body: &Body, idem: Idempotency) -> Attempt {
        let mut headers = HeaderMap::new();
        match HeaderValue::from_str(&format!("Bearer {}", self.inner.api_key)) {
            Ok(mut v) => {
                v.set_sensitive(true);
                headers.insert(AUTHORIZATION, v);
            }
            Err(_) => {
                return Attempt::Fail(Error::Config("api_key contains invalid characters".into()))
            }
        }

        let req = self.inner.http.post(url.clone()).headers(headers);
        let req = match body {
            Body::Form(f) => req.form(f),
            Body::Multipart(files) => {
                let mut form = reqwest::multipart::Form::new();
                for (field, name, bytes) in files {
                    let part = reqwest::multipart::Part::bytes(bytes.clone())
                        .file_name(name.clone())
                        .mime_str("application/x-bittorrent")
                        .expect("static mime type is valid");
                    form = form.part(field.clone(), part);
                }
                req.multipart(form)
            }
        };

        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                let retry = idem == Idempotency::Safe || e.is_connect();
                return if retry {
                    Attempt::Retry {
                        err: Error::Network(e),
                        after: None,
                    }
                } else {
                    Attempt::Fail(Error::Network(e))
                };
            }
        };

        let status = resp.status();
        let retry_after = resp
            .headers()
            .get(RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(Duration::from_secs);

        let text = match resp.text().await {
            Ok(t) => t,
            Err(e) => {
                return if idem == Idempotency::Safe {
                    Attempt::Retry {
                        err: Error::Network(e),
                        after: None,
                    }
                } else {
                    Attempt::Fail(Error::Network(e))
                }
            }
        };

        let code = status.as_u16();
        if code == 429 || code == 503 {
            let err = Error::RateLimited { retry_after };
            // 503 may come from an overloaded backend that did process the request.
            return if code == 429 || idem == Idempotency::Safe {
                Attempt::Retry {
                    err,
                    after: retry_after,
                }
            } else {
                Attempt::Fail(err)
            };
        }

        match serde_json::from_str::<Envelope>(&text) {
            Ok(env) => {
                if env.deprecated {
                    tracing::warn!(%url, "AllDebrid reports this endpoint as deprecated");
                }
                match (env.status.as_str(), env.data, env.error) {
                    ("success", Some(data), _) => Attempt::Done(data),
                    ("success", None, _) => Attempt::Done(serde_json::Value::Null),
                    (_, _, Some(api)) => {
                        let retryable = api.code.is_retryable();
                        if retryable {
                            Attempt::Retry {
                                err: Error::Api(api),
                                after: retry_after,
                            }
                        } else {
                            Attempt::Fail(Error::Api(api))
                        }
                    }
                    (other, _, None) => Attempt::Fail(Error::UnexpectedResponse(format!(
                        "status {other:?} without error object"
                    ))),
                }
            }
            Err(source) => {
                if status.is_success() {
                    Attempt::Fail(Error::Decode {
                        source,
                        body: truncate(text),
                    })
                } else {
                    let err = Error::Http {
                        status: code,
                        body: truncate(text),
                    };
                    if matches!(code, 502 | 504) && idem == Idempotency::Safe {
                        Attempt::Retry {
                            err,
                            after: retry_after,
                        }
                    } else {
                        Attempt::Fail(err)
                    }
                }
            }
        }
    }
}

fn truncate(mut s: String) -> String {
    if s.len() > MAX_BODY_IN_ERROR {
        let mut end = MAX_BODY_IN_ERROR;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
        s.push('…');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_requires_api_key() {
        assert!(matches!(Client::builder().build(), Err(Error::Config(_))));
        assert!(matches!(Client::new("  "), Err(Error::Config(_))));
    }

    #[test]
    fn debug_redacts_key() {
        let c = Client::new("secret-key").unwrap();
        assert!(!format!("{c:?}").contains("secret-key"));
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        let s = "é".repeat(MAX_BODY_IN_ERROR);
        let t = truncate(s);
        assert!(t.len() <= MAX_BODY_IN_ERROR + '…'.len_utf8());
    }
}
