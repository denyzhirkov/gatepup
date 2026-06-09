//! Response compression (gzip / brotli), negotiated via `Accept-Encoding`.
//!
//! Streaming: the upstream response body is piped through an async encoder
//! frame-by-frame (no full-body buffering). Skips already-encoded responses,
//! non-eligible content types, and small bodies of known length.

use async_compression::tokio::bufread::{BrotliEncoder, GzipEncoder};
use futures_util::TryStreamExt;
use http::header::{CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, VARY};
use http::{HeaderMap, HeaderValue, Response};
use http_body_util::{BodyExt, BodyStream, StreamBody};
use tokio::io::AsyncRead;
use tokio_util::io::{ReaderStream, StreamReader};

use crate::proxy::ResponseBody;
use crate::snapshot::Compression;
use crate::BoxError;

#[derive(Clone, Copy)]
enum Encoding {
    Gzip,
    Br,
}

impl Encoding {
    fn token(self) -> &'static str {
        match self {
            Encoding::Gzip => "gzip",
            Encoding::Br => "br",
        }
    }
}

/// Compress the response per the client's `Accept-Encoding`, if eligible.
/// Returns the response unchanged when compression doesn't apply.
pub(crate) fn maybe_compress(
    resp: Response<ResponseBody>,
    accept_encoding: &str,
    comp: &Compression,
) -> Response<ResponseBody> {
    if !should_compress(resp.headers(), comp) {
        return resp;
    }
    match choose_encoding(accept_encoding, comp) {
        Some(enc) => compress(resp, enc),
        None => resp,
    }
}

/// Pick an encoding the client accepts and we have enabled, preferring brotli.
/// q-values are not parsed; presence of the token (or `*`) means acceptable.
fn choose_encoding(accept_encoding: &str, comp: &Compression) -> Option<Encoding> {
    let mut br = false;
    let mut gzip = false;
    let mut any = false;
    for token in accept_encoding.split(',') {
        // Drop any ";q=..." parameter and normalize.
        let name = token
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        match name.as_str() {
            "br" => br = true,
            "gzip" => gzip = true,
            "*" => any = true,
            _ => {}
        }
    }
    if comp.br && (br || any) {
        Some(Encoding::Br)
    } else if comp.gzip && (gzip || any) {
        Some(Encoding::Gzip)
    } else {
        None
    }
}

/// Eligible when not already encoded, the content type is allowed, and (if the
/// length is known) it is at least `min_bytes`.
fn should_compress(headers: &HeaderMap, comp: &Compression) -> bool {
    if headers.contains_key(CONTENT_ENCODING) {
        return false;
    }
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        })
        .unwrap_or_default();
    if !comp.types.contains(&content_type) {
        return false;
    }
    if let Some(len) = headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
    {
        if len < comp.min_bytes {
            return false;
        }
    }
    true
}

fn compress(resp: Response<ResponseBody>, enc: Encoding) -> Response<ResponseBody> {
    let (mut parts, body) = resp.into_parts();

    // Upstream body frames -> data bytes -> an AsyncRead the encoder consumes.
    let byte_stream = BodyStream::new(body)
        .try_filter_map(|frame| async move { Ok(frame.into_data().ok()) })
        .map_err(std::io::Error::other);
    let reader = StreamReader::new(byte_stream);

    let out = match enc {
        Encoding::Gzip => reader_to_body(GzipEncoder::new(reader)),
        Encoding::Br => reader_to_body(BrotliEncoder::new(reader)),
    };

    // The encoded length is unknown up front; drop Content-Length and let the
    // body stream (chunked). Mark the encoding and vary on Accept-Encoding.
    parts.headers.remove(CONTENT_LENGTH);
    parts
        .headers
        .insert(CONTENT_ENCODING, HeaderValue::from_static(enc.token()));
    append_vary_accept_encoding(&mut parts.headers);

    Response::from_parts(parts, out)
}

fn reader_to_body<R: AsyncRead + Send + 'static>(reader: R) -> ResponseBody {
    let stream = ReaderStream::new(reader)
        .map_ok(hyper::body::Frame::data)
        .map_err(|e| Box::new(e) as BoxError);
    StreamBody::new(stream).boxed_unsync()
}

fn append_vary_accept_encoding(headers: &mut HeaderMap) {
    let already = headers
        .get(VARY)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_ascii_lowercase().contains("accept-encoding"))
        .unwrap_or(false);
    if !already {
        headers.append(VARY, HeaderValue::from_static("Accept-Encoding"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comp(gzip: bool, br: bool, min_bytes: u64) -> Compression {
        Compression {
            gzip,
            br,
            min_bytes,
            types: ["text/html".to_string()].into_iter().collect(),
        }
    }

    #[test]
    fn prefers_brotli_when_both_offered_and_enabled() {
        assert!(matches!(
            choose_encoding("gzip, br", &comp(true, true, 0)),
            Some(Encoding::Br)
        ));
    }

    #[test]
    fn falls_back_to_gzip_when_br_disabled() {
        assert!(matches!(
            choose_encoding("gzip, br", &comp(true, false, 0)),
            Some(Encoding::Gzip)
        ));
    }

    #[test]
    fn none_when_client_accepts_nothing_enabled() {
        assert!(choose_encoding("identity", &comp(true, true, 0)).is_none());
        assert!(choose_encoding("gzip", &comp(false, true, 0)).is_none());
    }

    #[test]
    fn wildcard_accepts_preferred() {
        assert!(matches!(
            choose_encoding("*", &comp(true, true, 0)),
            Some(Encoding::Br)
        ));
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn skips_already_encoded() {
        let h = headers(&[("content-type", "text/html"), ("content-encoding", "gzip")]);
        assert!(!should_compress(&h, &comp(true, true, 0)));
    }

    #[test]
    fn skips_disallowed_type() {
        let h = headers(&[("content-type", "application/octet-stream")]);
        assert!(!should_compress(&h, &comp(true, true, 0)));
    }

    #[test]
    fn skips_small_known_length() {
        let h = headers(&[("content-type", "text/html"), ("content-length", "100")]);
        assert!(!should_compress(&h, &comp(true, true, 1024)));
    }

    #[test]
    fn compresses_eligible_type_with_charset_and_unknown_length() {
        let h = headers(&[("content-type", "text/html; charset=utf-8")]);
        assert!(should_compress(&h, &comp(true, true, 1024)));
    }
}
