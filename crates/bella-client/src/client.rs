//! The HTTP client: construction, authentication, timeouts, error mapping
//! and retries. The typed routes are in `api.rs`.

use crate::error::ClientError;
use crate::retry::{Attempts, RetryPolicy};
use crate::tls;
use bella_core::ApiError;
use bella_core::api::API_PREFIX;
use bella_net::{ClientCredentials, ControllerUrl, Fingerprint};
use bytes::Bytes;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderName, HeaderValue};
use reqwest::{Method, Response, Url};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

/// The `user-agent` every request carries: `bella/<crate version>`.
pub const USER_AGENT: &str = concat!("bella/", env!("CARGO_PKG_VERSION"));

/// Largest JSON response body this client reads.
const MAX_JSON_BYTES: usize = 64 * 1024 * 1024;
/// How much of an error response is read to find an `ApiError`.
const MAX_ERROR_BYTES: usize = 64 * 1024;
/// How much of a non-JSON error body is quoted in an error message.
const ERROR_SNIPPET_CHARS: usize = 200;

/// Timeouts and retry behaviour of a [`Client`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientOptions {
    /// Time allowed for the TCP connection and TLS handshake. Default 10 s.
    pub connect_timeout: Duration,
    /// Time allowed for one request, from sending it to the end of its
    /// response body. Default 30 s. Long polls add their own wait on top;
    /// streamed bodies (events, objects) are bounded by
    /// `stream_idle_timeout` instead.
    pub request_timeout: Duration,
    /// Retries of requests that are safe to repeat.
    pub retry: RetryPolicy,
    /// A streamed body that delivers nothing for this long is treated as a
    /// dead connection. Default 60 s, four keep-alive intervals of the
    /// event stream. `None` waits forever.
    pub stream_idle_timeout: Option<Duration>,
    /// First wait before the event stream reconnects. It doubles up to
    /// `reconnect_max_backoff`, with jitter. Default 500 ms.
    pub reconnect_initial_backoff: Duration,
    /// Longest wait between two connection attempts of the event stream.
    /// Default 15 s.
    pub reconnect_max_backoff: Duration,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(30),
            retry: RetryPolicy::default(),
            stream_idle_timeout: Some(Duration::from_secs(60)),
            reconnect_initial_backoff: Duration::from_millis(500),
            reconnect_max_backoff: Duration::from_secs(15),
        }
    }
}

/// Typed asynchronous client for one controller.
///
/// Cheap to clone; clones share one connection pool. Every route of
/// `bella_core::api` is a method (see the crate documentation for the
/// list).
///
/// # Transport
///
/// * An `https` URL is reached with the pinned fleet certificate and
///   nothing else: the certificate must have the given fingerprint, the
///   host name is not consulted, and public certificate authorities are
///   never trusted. Without a fingerprint, construction fails.
/// * An `http` URL is reached without TLS. [`ControllerUrl`] only allows
///   that for loopback or after an explicit opt-in by the user.
/// * Redirects are not followed and no proxy is used, including the ones
///   named by environment variables, so the bearer token only ever goes to
///   the configured controller.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

struct Inner {
    http: reqwest::Client,
    url: ControllerUrl,
    base: Url,
    fingerprint: Option<Fingerprint>,
    authorization: Option<HeaderValue>,
    options: ClientOptions,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("url", &self.inner.url.to_string())
            .field("pinned", &self.inner.fingerprint.is_some())
            .field("has_token", &self.inner.authorization.is_some())
            .finish_non_exhaustive()
    }
}

impl Client {
    /// A client with the default [`ClientOptions`].
    ///
    /// `token` is the bearer token sent on every authenticated route; pass
    /// `None` for a client that only calls `health` and `enroll`.
    ///
    /// Fails with [`ClientError::Tls`] for an `https` URL without a
    /// fingerprint, and with [`ClientError::Invalid`] for a fingerprint on
    /// an `http` URL or a token that cannot be a header value. Nothing is
    /// connected until the first request.
    pub fn new(
        url: &ControllerUrl,
        fingerprint: Option<&Fingerprint>,
        token: Option<String>,
    ) -> Result<Client, ClientError> {
        Self::with_options(url, fingerprint, token, ClientOptions::default())
    }

    /// A client for stored credentials, with the default options.
    pub fn from_credentials(credentials: &ClientCredentials) -> Result<Client, ClientError> {
        Self::new(
            &credentials.url,
            credentials.fingerprint.as_ref(),
            Some(credentials.token.clone()),
        )
    }

    /// [`Client::new`] with explicit timeouts and retry behaviour.
    pub fn with_options(
        url: &ControllerUrl,
        fingerprint: Option<&Fingerprint>,
        token: Option<String>,
        options: ClientOptions,
    ) -> Result<Client, ClientError> {
        let tls_config = match tls::pinned_config(url, fingerprint)? {
            Some(pinned) => (*pinned).clone(),
            None => tls::refusing_config()?,
        };
        let authorization = token.as_deref().map(bearer).transpose()?;
        let base = Url::parse(&url.to_string())
            .map_err(|e| ClientError::invalid(format!("unusable controller URL {url}: {e}")))?;
        let http = reqwest::Client::builder()
            .use_preconfigured_tls(tls_config)
            .https_only(url.is_tls())
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(options.connect_timeout)
            .tcp_keepalive(Duration::from_secs(30))
            .tcp_nodelay(true)
            .user_agent(USER_AGENT)
            .build()
            .map_err(|e| ClientError::invalid(format!("could not build the HTTP client: {e}")))?;
        Ok(Client {
            inner: Arc::new(Inner {
                http,
                url: url.clone(),
                base,
                fingerprint: fingerprint.copied(),
                authorization,
                options,
            }),
        })
    }

    /// The controller this client talks to.
    pub fn url(&self) -> &ControllerUrl {
        &self.inner.url
    }

    /// The pinned certificate fingerprint; `None` for plain HTTP.
    pub fn fingerprint(&self) -> Option<&Fingerprint> {
        self.inner.fingerprint.as_ref()
    }

    /// Whether requests carry a bearer token.
    pub fn has_token(&self) -> bool {
        self.inner.authorization.is_some()
    }

    /// The timeouts and retry behaviour in effect.
    pub fn options(&self) -> &ClientOptions {
        &self.inner.options
    }

    /// The URL of an API route. Each segment is percent-encoded, so a
    /// project name can contain any character; a segment that a URL parser
    /// would drop or reinterpret is refused.
    pub(crate) fn endpoint(&self, segments: &[&str]) -> Result<Url, ClientError> {
        let mut url = self.inner.base.clone();
        {
            let mut path = url
                .path_segments_mut()
                .map_err(|()| ClientError::invalid("the controller URL cannot carry a path"))?;
            path.clear();
            for segment in API_PREFIX.split('/').filter(|part| !part.is_empty()) {
                path.push(segment);
            }
            for segment in segments {
                if segment.is_empty() || *segment == "." || *segment == ".." {
                    return Err(ClientError::invalid(format!(
                        "`{segment}` cannot be used as a name in a request path"
                    )));
                }
                path.push(segment);
            }
        }
        Ok(url)
    }

    fn builder(&self, call: &Call) -> reqwest::RequestBuilder {
        let mut request = self
            .inner
            .http
            .request(call.method.clone(), call.url.clone());
        if call.auth
            && let Some(authorization) = &self.inner.authorization
        {
            request = request.header(AUTHORIZATION, authorization.clone());
        }
        if let Some(accept) = call.accept {
            request = request.header(ACCEPT, accept);
        }
        for (name, value) in &call.headers {
            request = request.header(name.clone(), value.clone());
        }
        if let Some(json) = &call.json {
            request = request
                .header(CONTENT_TYPE, "application/json")
                .body(json.clone());
        }
        request
    }

    /// Sends the request once and reads the whole response body, all within
    /// `timeout`.
    async fn fetch_once(&self, call: &Call, timeout: Duration) -> Result<Bytes, Failure> {
        let response = self
            .builder(call)
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| Failure::from(ClientError::from_reqwest(e)))?;
        let mut response = check_status(response, timeout).await?;
        read_body(&mut response, MAX_JSON_BYTES)
            .await
            .map_err(Failure::from)
    }

    /// Sends the request and returns the response body, retrying as the
    /// call and the policy allow.
    pub(crate) async fn fetch(&self, call: &Call) -> Result<Bytes, ClientError> {
        let timeout = call.timeout.unwrap_or(self.inner.options.request_timeout);
        self.with_retries(call.retry, || self.fetch_once(call, timeout))
            .await
    }

    /// Sends the request and decodes its JSON response.
    pub(crate) async fn json<T: DeserializeOwned>(&self, call: &Call) -> Result<T, ClientError> {
        let body = self.fetch(call).await?;
        decode_json(&body, call.url.path())
    }

    /// One try of [`Client::json`], classified for a caller that runs its
    /// own retry loop.
    pub(crate) async fn json_once<T: DeserializeOwned>(&self, call: &Call) -> Result<T, Failure> {
        let timeout = call.timeout.unwrap_or(self.inner.options.request_timeout);
        let body = self.fetch_once(call, timeout).await?;
        decode_json(&body, call.url.path()).map_err(Failure::from)
    }

    /// Sends the request and returns the response once its status line and
    /// headers have arrived and the status is 2xx. The body is left to the
    /// caller and has no overall time limit. Retries cover the part up to
    /// the headers.
    pub(crate) async fn open(&self, call: &Call) -> Result<Response, ClientError> {
        self.with_retries(call.retry, || self.open_once(call)).await
    }

    /// One try of [`Client::open`], classified for a caller that runs its
    /// own retry loop.
    pub(crate) async fn open_once(&self, call: &Call) -> Result<Response, Failure> {
        let timeout = call.timeout.unwrap_or(self.inner.options.request_timeout);
        let response = tokio::time::timeout(timeout, self.builder(call).send())
            .await
            .map_err(|_| Failure::from(ClientError::Timeout))?
            .map_err(|e| Failure::from(ClientError::from_reqwest(e)))?;
        check_status(response, timeout).await
    }

    /// Sends a request with a streamed body once. There is no time limit on
    /// the upload; the response headers and an error body are read as they
    /// arrive.
    pub(crate) async fn upload_once(
        &self,
        call: &Call,
        body: reqwest::Body,
    ) -> Result<Response, Failure> {
        let response = self
            .builder(call)
            .body(body)
            .send()
            .await
            .map_err(|e| Failure::from(ClientError::from_reqwest(e)))?;
        check_status(response, self.inner.options.request_timeout).await
    }

    /// Runs `operation` until it succeeds, fails for good, or the retry
    /// budget is spent. With `retry` false it runs exactly once.
    pub(crate) async fn with_retries<T, F, Fut>(
        &self,
        retry: bool,
        mut operation: F,
    ) -> Result<T, ClientError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, Failure>>,
    {
        let mut attempts = Attempts::new(&self.inner.options.retry);
        loop {
            let failure = match operation().await {
                Ok(value) => return Ok(value),
                Err(failure) => failure,
            };
            if !(retry && failure.transient) {
                return Err(failure.error);
            }
            let Some(delay) = attempts.next_delay() else {
                return Err(failure.error);
            };
            tracing::debug!(error = %failure.error, ?delay, "retrying a controller request");
            tokio::time::sleep(delay).await;
        }
    }
}

/// An error together with whether another try could succeed.
#[derive(Debug)]
pub(crate) struct Failure {
    pub(crate) error: ClientError,
    pub(crate) transient: bool,
}

impl From<ClientError> for Failure {
    fn from(error: ClientError) -> Self {
        let transient = error.is_transient();
        Failure { error, transient }
    }
}

/// One request to the controller.
#[derive(Debug)]
pub(crate) struct Call {
    pub(crate) method: Method,
    pub(crate) url: Url,
    pub(crate) json: Option<Bytes>,
    pub(crate) headers: Vec<(HeaderName, HeaderValue)>,
    pub(crate) accept: Option<&'static str>,
    pub(crate) timeout: Option<Duration>,
    pub(crate) retry: bool,
    pub(crate) auth: bool,
}

impl Call {
    /// An authenticated request that is sent once. `GET` requests are
    /// marked safe to repeat; everything else opts in with [`Call::retry`].
    pub(crate) fn new(method: Method, url: Url) -> Self {
        let retry = method == Method::GET || method == Method::HEAD;
        Self {
            method,
            url,
            json: None,
            headers: Vec::new(),
            accept: Some("application/json"),
            timeout: None,
            retry,
            auth: true,
        }
    }

    /// Marks the request as safe to repeat.
    pub(crate) fn retry(mut self) -> Self {
        self.retry = true;
        self
    }

    pub(crate) fn unauthenticated(mut self) -> Self {
        self.auth = false;
        self
    }

    pub(crate) fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub(crate) fn accept(mut self, accept: &'static str) -> Self {
        self.accept = Some(accept);
        self
    }

    pub(crate) fn header(mut self, name: HeaderName, value: HeaderValue) -> Self {
        self.headers.push((name, value));
        self
    }

    pub(crate) fn json<B: Serialize>(mut self, body: &B) -> Result<Self, ClientError> {
        let bytes = serde_json::to_vec(body)
            .map_err(|e| ClientError::invalid(format!("the request cannot be encoded: {e}")))?;
        self.json = Some(Bytes::from(bytes));
        Ok(self)
    }

    /// Adds the fields of a query struct as URL query parameters. Absent
    /// (`None`) fields are left out; a field that is not a string, number
    /// or boolean is refused.
    pub(crate) fn query<Q: Serialize>(mut self, query: &Q) -> Result<Self, ClientError> {
        let value = serde_json::to_value(query)
            .map_err(|e| ClientError::invalid(format!("the query cannot be encoded: {e}")))?;
        let fields = match value {
            serde_json::Value::Object(fields) => fields,
            serde_json::Value::Null => return Ok(self),
            _ => {
                return Err(ClientError::invalid(
                    "a query must be a set of named fields",
                ));
            }
        };
        let mut pairs = Vec::with_capacity(fields.len());
        for (name, value) in fields {
            let text = match value {
                serde_json::Value::Null => continue,
                serde_json::Value::String(text) => text,
                serde_json::Value::Bool(flag) => flag.to_string(),
                serde_json::Value::Number(number) => number.to_string(),
                serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
                    return Err(ClientError::invalid(format!(
                        "query field `{name}` is not a single value"
                    )));
                }
            };
            pairs.push((name, text));
        }
        if !pairs.is_empty() {
            self.url.query_pairs_mut().extend_pairs(pairs);
        }
        Ok(self)
    }
}

/// `Bearer <token>` as a header value that is never logged.
pub(crate) fn bearer(token: &str) -> Result<HeaderValue, ClientError> {
    if token.is_empty() {
        return Err(ClientError::invalid("the token is empty"));
    }
    let mut value = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
        ClientError::invalid("the token contains characters that cannot be sent in a header")
    })?;
    value.set_sensitive(true);
    Ok(value)
}

/// Turns a non-2xx response into an error: [`ClientError::Api`] when the
/// body is an `ApiError`, [`ClientError::Http`] otherwise.
async fn check_status(mut response: Response, timeout: Duration) -> Result<Response, Failure> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    // The status already decides the outcome; a body that cannot be read
    // only costs the message.
    let body = tokio::time::timeout(timeout, read_body(&mut response, MAX_ERROR_BYTES))
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    let error = error_from_response(status.as_u16(), &body);
    let transient = error.is_transient() || matches!(status.as_u16(), 502..=504);
    Err(Failure { error, transient })
}

/// The error for a non-2xx status and the body that came with it.
pub(crate) fn error_from_response(status: u16, body: &[u8]) -> ClientError {
    match serde_json::from_slice::<ApiError>(body) {
        Ok(api) => ClientError::Api(api),
        Err(_) => {
            let text = String::from_utf8_lossy(body);
            let snippet: String = text.trim().chars().take(ERROR_SNIPPET_CHARS).collect();
            let message = if snippet.is_empty() {
                "no error details in the response".to_owned()
            } else {
                snippet
            };
            ClientError::Http { status, message }
        }
    }
}

/// Reads a response body, refusing one longer than `limit`.
async fn read_body(response: &mut Response, limit: usize) -> Result<Bytes, ClientError> {
    let mut body: Vec<u8> = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if body.len() + chunk.len() > limit {
                    return Err(ClientError::decode(format!(
                        "the response is longer than {limit} bytes"
                    )));
                }
                body.extend_from_slice(&chunk);
            }
            Ok(None) => return Ok(Bytes::from(body)),
            Err(e) => return Err(ClientError::from_reqwest(e)),
        }
    }
}

pub(crate) fn decode_json<T: DeserializeOwned>(body: &[u8], what: &str) -> Result<T, ClientError> {
    serde_json::from_slice(body)
        .map_err(|e| ClientError::decode(format!("the response of {what} is not valid: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bella_core::ErrorCode;
    use bella_core::api::{ListJobsQuery, LogQuery, WaitQuery};

    fn client() -> Client {
        let url = ControllerUrl::parse("http://127.0.0.1:7420", false).unwrap();
        Client::new(&url, None, Some("bella_dev_test".into())).unwrap()
    }

    #[test]
    fn endpoints_live_under_the_api_prefix_and_encode_names() {
        let client = client();
        assert_eq!(
            client.endpoint(&["jobs"]).unwrap().as_str(),
            "http://127.0.0.1:7420/api/v1/jobs"
        );
        assert_eq!(
            client
                .endpoint(&["projects", "my app/v2?x#y"])
                .unwrap()
                .as_str(),
            "http://127.0.0.1:7420/api/v1/projects/my%20app%2Fv2%3Fx%23y"
        );
        for bad in ["", ".", ".."] {
            assert!(matches!(
                client.endpoint(&["projects", bad]),
                Err(ClientError::Invalid { .. })
            ));
        }
    }

    #[test]
    fn query_structs_become_query_parameters() {
        let client = client();
        let url = client.endpoint(&["jobs"]).unwrap();

        let none = Call::new(Method::GET, url.clone())
            .query(&ListJobsQuery::default())
            .unwrap();
        assert_eq!(none.url.query(), None);

        let some = Call::new(Method::GET, url.clone())
            .query(&ListJobsQuery {
                project: Some("web & api".into()),
                session: None,
                active: Some(true),
                limit: Some(25),
            })
            .unwrap();
        let pairs: Vec<(String, String)> = some.url.query_pairs().into_owned().collect();
        assert!(pairs.contains(&("project".into(), "web & api".into())));
        assert!(pairs.contains(&("active".into(), "true".into())));
        assert!(pairs.contains(&("limit".into(), "25".into())));
        assert_eq!(pairs.len(), 3);

        let wait = Call::new(Method::GET, url.clone())
            .query(&WaitQuery {
                timeout_ms: Some(1500),
                after_event: Some(7),
            })
            .unwrap();
        let pairs: Vec<(String, String)> = wait.url.query_pairs().into_owned().collect();
        assert!(pairs.contains(&("timeout_ms".into(), "1500".into())));
        assert!(pairs.contains(&("after_event".into(), "7".into())));

        let logs = Call::new(Method::GET, url)
            .query(&LogQuery {
                step: Some("build".into()),
                cursor: Some("a=b&c".into()),
                tail: Some(false),
                limit_bytes: None,
            })
            .unwrap();
        let pairs: Vec<(String, String)> = logs.url.query_pairs().into_owned().collect();
        assert!(pairs.contains(&("cursor".into(), "a=b&c".into())));
        assert!(pairs.contains(&("tail".into(), "false".into())));
    }

    #[test]
    fn only_reads_are_repeatable_by_default() {
        let url = client().endpoint(&["jobs"]).unwrap();
        assert!(Call::new(Method::GET, url.clone()).retry);
        assert!(Call::new(Method::HEAD, url.clone()).retry);
        assert!(!Call::new(Method::POST, url.clone()).retry);
        assert!(!Call::new(Method::PUT, url.clone()).retry);
        assert!(!Call::new(Method::PATCH, url.clone()).retry);
        assert!(Call::new(Method::POST, url).retry().retry);
    }

    #[test]
    fn error_bodies_map_to_api_or_http_errors() {
        let api = ApiError::new(ErrorCode::InvalidState, "already finished");
        let body = serde_json::to_vec(&api).unwrap();
        assert_eq!(error_from_response(409, &body), ClientError::Api(api));

        assert_eq!(
            error_from_response(502, b"<html>Bad Gateway</html>"),
            ClientError::Http {
                status: 502,
                message: "<html>Bad Gateway</html>".into()
            }
        );
        assert!(matches!(
            error_from_response(404, b""),
            ClientError::Http { status: 404, .. }
        ));
        // Valid JSON that is not an ApiError is not mistaken for one.
        assert!(matches!(
            error_from_response(500, br#"{"error":"boom"}"#),
            ClientError::Http { status: 500, .. }
        ));
    }

    #[test]
    fn tokens_must_be_usable_as_header_values() {
        assert!(bearer("bella_dev_abc").unwrap().is_sensitive());
        assert!(matches!(bearer(""), Err(ClientError::Invalid { .. })));
        assert!(matches!(
            bearer("line\nbreak"),
            Err(ClientError::Invalid { .. })
        ));
    }

    #[test]
    fn debug_output_does_not_contain_the_token() {
        let text = format!("{:?}", client());
        assert!(!text.contains("bella_dev_test"));
        assert!(text.contains("127.0.0.1"));
    }
}
