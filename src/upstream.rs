//! Upstream registry proxying.
//!
//! When a package is not published locally it is looked up on a configured list
//! of upstream registries, **in order**. The semantics are deliberate:
//!
//! * `200` — that upstream wins, stop looking.
//! * `404` — this upstream does not have it; try the next one.
//! * anything else (network error, 5xx, 401…) — this upstream failed; try the
//!   next one, but remember that it failed.
//!
//! If at least one upstream authoritatively answered "no such package" and none
//! succeeded, the caller gets a 404. If **every** upstream failed, the caller
//! gets a 502 instead — reporting 404 there would tell `npm install` that a
//! package does not exist when in reality we could not find out.
//!
//! ## Why this relays instead of caching
//!
//! An earlier version fetched each upstream packument, parsed it into
//! `serde_json::Value`, and cached it locally. That is the obvious design and on
//! a normal machine it is fine, but it does not fit a small host, and measurably
//! so:
//!
//! | package      | packument JSON | resident cost once parsed |
//! | ------------ | -------------- | ------------------------- |
//! | `vue`        | 2.2 MB         | +34 MB                    |
//! | `typescript` | 15 MB          | ≈150 MB                   |
//!
//! `serde_json::Value` is a heavy representation — every string and object gets
//! its own node — so one install touching a few large packuments drove a
//! 2-core / 1.7 GB box straight into its memory ceiling. Asking for the
//! abbreviated packument (about 2× smaller) did not change the order of
//! magnitude.
//!
//! So this module relays instead: the upstream body is streamed straight through
//! to the client and never materialised. Cost is flat in package size. The price
//! is no local metadata cache — acceptable because npm keeps its own
//! content-addressed cache, so repeat installs on a developer machine never
//! reach the registry at all.
//!
//! Because packuments are relayed verbatim, their `dist.tarball` URLs keep
//! pointing at the upstream CDN. Tarball bytes never pass through this process,
//! which is the better outcome anyway on a host with a small egress budget.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::header::{self, HeaderValue};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use reqwest::header::ACCEPT;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::config::Config;
use crate::error::AppError;

const USER_AGENT_VALUE: &str = concat!("npm-registry-rust/", env!("CARGO_PKG_VERSION"));

/// Ask for the full packument. Relaying is size-independent, so there is no
/// reason to trade away `readme` / `time` / `maintainers` for a smaller payload.
const ACCEPT_JSON: &str = "application/json";

/// Outcome of walking the upstream chain.
pub enum UpstreamLookup {
    Found(Relay),
    NotFound,
}

/// An upstream response, ready to hand back to the client.
pub struct Relay {
    registry: String,
    response: Response,
}

impl Relay {
    /// Which registry served this, for logging and tests.
    pub fn registry(&self) -> &str {
        &self.registry
    }
}

impl IntoResponse for Relay {
    fn into_response(self) -> Response {
        self.response
    }
}

#[derive(Clone)]
pub struct UpstreamClient {
    http: reqwest::Client,
    registries: Vec<String>,
    timeout: Duration,
    /// Caps how many upstream responses may be in flight at once. A single
    /// `npm install` fans out into dozens of parallel lookups; unbounded, that
    /// piles connections and buffers onto a small host for no benefit.
    permits: Arc<Semaphore>,
}

/// Upstream work scales with the machine, but never without a ceiling.
fn upstream_concurrency() -> usize {
    std::thread::available_parallelism()
        .map(|cores| cores.get() * 2)
        .unwrap_or(4)
        .clamp(2, 8)
}

impl UpstreamClient {
    /// Build a client for an explicit chain. Exposed so tests can point at stub
    /// registries without going through the environment.
    pub fn for_registries(registries: Vec<String>, timeout: Duration) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT_VALUE)
            .connect_timeout(Duration::from_secs(10))
            .pool_max_idle_per_host(8)
            .build()?;

        Ok(Self {
            http,
            registries,
            timeout,
            permits: Arc::new(Semaphore::new(upstream_concurrency())),
        })
    }

    /// Build from configuration. Returns `Ok(None)` when no upstream is
    /// configured, which disables proxying entirely.
    pub fn build(config: &Config) -> anyhow::Result<Option<Self>> {
        if config.upstream_registries.is_empty() {
            return Ok(None);
        }

        Ok(Some(Self::for_registries(
            config.upstream_registries.clone(),
            Duration::from_secs(config.upstream_timeout_seconds),
        )?))
    }

    pub fn registries(&self) -> &[String] {
        &self.registries
    }

    /// Walk the upstream chain for a registry-relative path, e.g. `lodash`,
    /// `@scope%2fname`, or `lodash/1.2.3`.
    ///
    /// The winning body is streamed rather than buffered, so this costs the same
    /// whether the package has three versions or three thousand.
    pub async fn relay(&self, path: &str) -> Result<UpstreamLookup, AppError> {
        let mut permit = Some(
            self.permits
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| AppError::Internal(anyhow::anyhow!("upstream limiter closed")))?,
        );

        let mut missing_somewhere = false;
        let mut failures: Vec<String> = Vec::new();

        for registry in &self.registries {
            let url = format!("{registry}/{path}");

            let response = self
                .http
                .get(&url)
                .header(ACCEPT, ACCEPT_JSON)
                .timeout(self.timeout)
                .send()
                .await;

            match response {
                Ok(response) if response.status().is_success() => {
                    let held = permit.take().expect("permit available until relay starts");
                    let relay = Relay {
                        registry: registry.clone(),
                        response: relay_response(response, held),
                    };
                    tracing::debug!(
                        upstream = relay.registry(),
                        path = %path,
                        "relaying upstream response"
                    );
                    return Ok(UpstreamLookup::Found(relay));
                }
                Ok(response) if response.status() == StatusCode::NOT_FOUND => {
                    tracing::debug!(
                        upstream = %registry,
                        path = %path,
                        "upstream miss, trying next"
                    );
                    missing_somewhere = true;
                }
                Ok(response) => {
                    failures.push(format!("{registry}: HTTP {}", response.status()));
                }
                Err(err) => {
                    failures.push(format!("{registry}: {err}"));
                }
            }
        }

        // No relay was started, so the permit goes out of scope here.

        if missing_somewhere {
            if !failures.is_empty() {
                tracing::warn!(
                    path = %path,
                    failures = %failures.join("; "),
                    "resolved as not-found, but some upstreams errored along the way"
                );
            }
            return Ok(UpstreamLookup::NotFound);
        }

        if failures.is_empty() {
            // No upstreams configured — the caller never asked us to proxy.
            return Ok(UpstreamLookup::NotFound);
        }

        Err(AppError::Upstream(format!(
            "no upstream registry could answer for '{path}': {}",
            failures.join("; ")
        )))
    }
}

/// Wrap an upstream response so its body streams straight through, holding the
/// concurrency permit until the last byte has been sent.
fn relay_response(response: reqwest::Response, permit: OwnedSemaphorePermit) -> Response {
    let status = response.status();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .unwrap_or_else(|| "application/json".to_string());
    let content_length = response.content_length();

    let stream = response.bytes_stream().map(move |chunk| {
        // Tie the permit to the stream: the concurrency slot is released only
        // once this response has actually finished.
        let _held = &permit;
        chunk.map_err(std::io::Error::other)
    });

    let mut builder = Response::builder().status(status);

    if let Ok(value) = HeaderValue::from_str(&content_type) {
        builder = builder.header(header::CONTENT_TYPE, value);
    }
    if let Some(length) = content_length {
        builder = builder.header(header::CONTENT_LENGTH, length);
    }

    builder
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::Router;

    /// Spin up a stub registry that answers every request with the same status
    /// and body, and return its base URL.
    async fn stub(status: StatusCode, body: &'static str) -> String {
        let app = Router::new().fallback(move || async move { (status, body) });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stub registry");
        let address = listener.local_addr().expect("stub addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{address}")
    }

    fn client(registries: Vec<String>) -> UpstreamClient {
        UpstreamClient::for_registries(registries, Duration::from_secs(5)).expect("client")
    }

    const PACKUMENT: &str =
        r#"{"name":"demo","dist-tags":{"latest":"1.0.0"},"versions":{"1.0.0":{"name":"demo"}}}"#;

    async fn body_of(relay: Relay) -> (StatusCode, String) {
        let response = relay.into_response();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    #[tokio::test]
    async fn first_upstream_wins_and_the_body_is_relayed_verbatim() {
        let first = stub(StatusCode::OK, PACKUMENT).await;
        let second = stub(StatusCode::OK, PACKUMENT).await;
        let client = client(vec![first.clone(), second.clone()]);

        match client.relay("demo").await.expect("lookup") {
            UpstreamLookup::Found(relay) => {
                assert_eq!(relay.registry(), first);
                let (status, body) = body_of(relay).await;
                assert_eq!(status, StatusCode::OK);
                assert_eq!(body, PACKUMENT, "relayed bytes must be untouched");
            }
            UpstreamLookup::NotFound => panic!("expected a hit from the first upstream"),
        }
    }

    #[tokio::test]
    async fn not_found_falls_through_to_the_next_upstream() {
        let missing = stub(StatusCode::NOT_FOUND, r#"{"error":"Not found"}"#).await;
        let present = stub(StatusCode::OK, PACKUMENT).await;
        let client = client(vec![missing, present.clone()]);

        match client.relay("demo").await.expect("lookup") {
            UpstreamLookup::Found(relay) => assert_eq!(relay.registry(), present),
            UpstreamLookup::NotFound => panic!("expected the second upstream to answer"),
        }
    }

    #[tokio::test]
    async fn every_upstream_missing_is_not_found() {
        let a = stub(StatusCode::NOT_FOUND, "{}").await;
        let b = stub(StatusCode::NOT_FOUND, "{}").await;
        let client = client(vec![a, b]);

        match client.relay("nope").await.expect("lookup") {
            UpstreamLookup::NotFound => {}
            UpstreamLookup::Found(_) => panic!("expected NotFound"),
        }
    }

    /// The important one: an unreachable upstream must not be reported as
    /// "package does not exist" — `npm install` would then silently claim the
    /// package was never published.
    #[tokio::test]
    async fn every_upstream_erroring_is_a_gateway_error_not_a_404() {
        let a = stub(StatusCode::INTERNAL_SERVER_ERROR, "{}").await;
        let b = stub(StatusCode::SERVICE_UNAVAILABLE, "{}").await;
        let client = client(vec![a, b]);

        match client.relay("demo").await {
            Err(AppError::Upstream(message)) => {
                assert!(
                    message.contains("no upstream registry could answer"),
                    "{message}"
                );
            }
            Err(other) => panic!("expected an upstream error, got: {other}"),
            Ok(_) => panic!("expected an upstream error, got a successful relay"),
        }
    }

    /// A definite miss beats an unrelated failure: one upstream saying "no such
    /// package" is authoritative.
    #[tokio::test]
    async fn a_miss_alongside_a_failure_still_resolves_as_not_found() {
        let missing = stub(StatusCode::NOT_FOUND, "{}").await;
        let broken = stub(StatusCode::INTERNAL_SERVER_ERROR, "{}").await;
        let client = client(vec![missing, broken]);

        match client.relay("demo").await.expect("lookup") {
            UpstreamLookup::NotFound => {}
            UpstreamLookup::Found(_) => panic!("expected NotFound"),
        }
    }
}
