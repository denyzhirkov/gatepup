use bytes::Bytes;
use http::header::{AUTHORIZATION, CONTENT_TYPE, WWW_AUTHENTICATE};
use http::{HeaderValue, Method, Request, Response, StatusCode};
use http_body_util::Full;
use hyper::body::Incoming;
use serde::Serialize;
use serde_json::json;

use crate::health::health_payload;
use crate::AdminState;

const PROMETHEUS_CONTENT_TYPE: &str = "text/plain; version=0.0.4";

/// Route an admin request. All endpoints are read-only `GET`s.
pub(crate) fn route(state: &AdminState, req: Request<Incoming>) -> Response<Full<Bytes>> {
    if req.method() != Method::GET {
        return json(
            StatusCode::METHOD_NOT_ALLOWED,
            &json!({ "error": "method_not_allowed" }),
        );
    }

    let path = req.uri().path();

    // Bearer auth (when configured) guards everything except /health, which the
    // container HEALTHCHECK and liveness probes must reach unauthenticated.
    if let Some(token) = state.token.as_deref() {
        if path != "/health" && !authorized(&req, token) {
            return unauthorized();
        }
    }

    if Some(path) == state.metrics_path.as_deref() {
        return metrics(state);
    }

    match path {
        "/health" => json(StatusCode::OK, &health_payload(state.version)),
        "/routes" => json(
            StatusCode::OK,
            &json!({ "routes": state.snapshot.load().routes() }),
        ),
        "/upstreams" => json(
            StatusCode::OK,
            &json!({ "upstreams": state.snapshot.load().upstreams_view() }),
        ),
        "/config/effective" => response(
            StatusCode::OK,
            "application/json",
            state.effective_config.load().as_ref().clone(),
        ),
        _ => json(StatusCode::NOT_FOUND, &json!({ "error": "not_found" })),
    }
}

/// True when the request carries `Authorization: Bearer <token>` matching the
/// configured token (compared in constant time to avoid leaking it via timing).
fn authorized(req: &Request<Incoming>, token: &str) -> bool {
    let expected = format!("Bearer {token}");
    req.headers()
        .get(AUTHORIZATION)
        .map(|v| constant_time_eq(v.as_bytes(), expected.as_bytes()))
        .unwrap_or(false)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn unauthorized() -> Response<Full<Bytes>> {
    let mut resp = json(
        StatusCode::UNAUTHORIZED,
        &json!({ "error": "unauthorized" }),
    );
    resp.headers_mut().insert(
        WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer realm=\"gatepup-admin\""),
    );
    resp
}

fn metrics(state: &AdminState) -> Response<Full<Bytes>> {
    // Refresh the per-upstream health gauge from the live snapshot at scrape time.
    for (upstream, healthy) in state.snapshot.load().healthy_counts() {
        state.metrics.set_upstream_healthy(&upstream, healthy);
    }
    response(
        StatusCode::OK,
        PROMETHEUS_CONTENT_TYPE,
        state.metrics.encode(),
    )
}

fn json<T: Serialize>(status: StatusCode, value: &T) -> Response<Full<Bytes>> {
    let body = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    response(status, "application/json", body)
}

fn response(status: StatusCode, content_type: &str, body: String) -> Response<Full<Bytes>> {
    let mut resp = Response::new(Full::new(Bytes::from(body)));
    *resp.status_mut() = status;
    if let Ok(value) = HeaderValue::from_str(content_type) {
        resp.headers_mut().insert(CONTENT_TYPE, value);
    }
    resp
}
