use serde_json::{json, Value};

/// Body for `GET /health`.
pub(crate) fn health_payload(version: &str) -> Value {
    json!({ "status": "ok", "version": version })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_reports_ok_and_version() {
        let payload = health_payload("9.9.9");
        assert_eq!(payload["status"], "ok");
        assert_eq!(payload["version"], "9.9.9");
    }
}
