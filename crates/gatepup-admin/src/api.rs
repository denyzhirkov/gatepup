use bytes::Bytes;
use http::header::CONTENT_TYPE;
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

    if Some(path) == state.metrics_path.as_deref() {
        return metrics(state);
    }

    match path {
        "/health" => json(StatusCode::OK, &health_payload(state.version)),
        "/routes" => json(
            StatusCode::OK,
            &json!({ "routes": state.snapshot.routes() }),
        ),
        "/upstreams" => json(
            StatusCode::OK,
            &json!({ "upstreams": state.snapshot.upstreams_view() }),
        ),
        "/config/effective" => response(
            StatusCode::OK,
            "application/json",
            (*state.effective_config).clone(),
        ),
        _ => json(StatusCode::NOT_FOUND, &json!({ "error": "not_found" })),
    }
}

fn metrics(state: &AdminState) -> Response<Full<Bytes>> {
    // Refresh the per-upstream health gauge from the live snapshot at scrape time.
    for (upstream, healthy) in state.snapshot.healthy_counts() {
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
