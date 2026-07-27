//! Behavioral test for response compression (kata 7dmx).
//!
//! The win is that text assets (app.js 243 KB, style.css 45 KB, index.html
//! 33 KB) and API JSON ship gzipped over the tailnet to a phone instead of
//! raw — ~75% off the cold-load byte transfer. This test exercises the REAL
//! `compression_layer()` (the same one `main` applies to the router) end-to-end
//! via `tower::ServiceExt::oneshot` — not a string-invariant — so a layer
//! that's absent, that doesn't actually gzip, or that strips the route headers
//! the static routes depend on, fails.
//!
//! What's pinned: with `Accept-Encoding: gzip`, the response comes back
//! `content-encoding: gzip` with a real gzip-stream body (magic `1f 8b`),
//! smaller than the raw, and `Cache-Control` + `Service-Worker-Allowed` SURVIVE
//! (tower-http must not strip them — the `no-cache`/`no-store` revalidation
//! contract and the mobile SW scope depend on these headers reaching the
//! client). Without `Accept-Encoding`, there's no `content-encoding` and the
//! body is the raw bytes (graceful fallback: clients that don't ask for gzip
//! aren't harmed).
//!
//! No `AppState` is constructed: the compression layer sits OUTSIDE the router,
//! so a minimal router with a handler returning the exact headers the real
//! static routes use is a faithful stand-in. Building `AppState` would couple a
//! perf test to all the account/token machinery — the wrong test (kata 7dmx
//! guardrail: don't gold-plate).

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, header};
use axum::response::IntoResponse;
use axum::routing::get;
use supervillain::routes::compression_layer;
use tower::ServiceExt;

// A handler returning a compressible text body with the exact headers the real
// `app_js` route uses, PLUS `Service-Worker-Allowed` (the `mobile_sw` header the
// ticket's caveat names) — so the test pins that compression preserves both the
// static-route `Cache-Control` and the SW scope header.
async fn js_like_handler() -> impl IntoResponse {
    (
        [
            ("content-type", "application/javascript; charset=utf-8"),
            ("cache-control", "no-cache"),
            ("service-worker-allowed", "/mobile/"),
        ],
        // Repeat a compressible string so gzip clearly beats raw (a short body
        // might not compress smaller than the gzip framing overhead).
        "const x = 'supervillain';\n".repeat(64),
    )
}

fn test_router() -> Router {
    Router::new()
        .route("/", get(js_like_handler))
        .layer(compression_layer())
}

#[tokio::test]
async fn gzips_text_when_accept_encoding_gzip() {
    let resp = test_router()
        .oneshot(
            Request::builder()
                .header(header::ACCEPT_ENCODING, "gzip")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let headers = resp.headers();
    assert_eq!(
        headers.get(header::CONTENT_ENCODING).unwrap(),
        "gzip",
        "gzip-accepted text must come back content-encoding: gzip"
    );
    // Cache-Control must survive the compression layer (ticket caveat: the
    // no-cache revalidation contract depends on it reaching the client).
    assert_eq!(
        headers.get(header::CACHE_CONTROL).unwrap(),
        "no-cache",
        "compression layer must not strip Cache-Control"
    );
    // Service-Worker-Allowed must survive too (mobile_sw caveat: the SW scope
    // header must reach the client uncompressed or the SW registration fails).
    assert_eq!(
        headers.get("service-worker-allowed").unwrap(),
        "/mobile/",
        "compression layer must not strip Service-Worker-Allowed"
    );
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    // A real gzip stream starts with the magic bytes 1f 8b — proves the body is
    // an actual gzip stream, not just a labeled header with raw bytes behind it.
    assert_eq!(
        &bytes[..2],
        &[0x1f, 0x8b],
        "body must be a real gzip stream"
    );
    // And the gzipped body must be smaller than the raw (gzip beats raw on
    // compressible text — if it weren't smaller, compression would be a net
    // loss and the layer shouldn't have applied it).
    let raw = "const x = 'supervillain';\n".repeat(64);
    assert!(
        bytes.len() < raw.len(),
        "gzipped body ({}) must be smaller than raw ({})",
        bytes.len(),
        raw.len()
    );
}

#[tokio::test]
async fn no_compression_when_no_accept_encoding() {
    // Graceful fallback: a client that doesn't advertise gzip (e.g. an
    // HTTP/1.0 probe, a plain curl) gets the raw bytes with no content-encoding.
    // Compression must be opt-in per request, never forced.
    let resp = test_router()
        .oneshot(Request::builder().body(Body::empty()).unwrap())
        .await
        .unwrap();
    let headers = resp.headers();
    assert!(
        headers.get(header::CONTENT_ENCODING).is_none(),
        "a request without Accept-Encoding must not get content-encoding"
    );
    assert_eq!(
        headers.get(header::CACHE_CONTROL).unwrap(),
        "no-cache",
        "headers still present without compression"
    );
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let raw = "const x = 'supervillain';\n".repeat(64);
    assert_eq!(
        bytes.len(),
        raw.len(),
        "uncompressed body must be the raw bytes"
    );
}
