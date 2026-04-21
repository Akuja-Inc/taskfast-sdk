//! JSON output envelope — uniform across success/error/dry-run.
//!
//! Shape (success):
//! ```json
//! { "ok": true, "environment": "production", "dry_run": false, "data": {...} }
//! ```
//!
//! Shape (error):
//! ```json
//! {
//!   "ok": false, "environment": "production", "dry_run": false,
//!   "error": { "code": "rate_limited", "message": "...", "retry_after_seconds": 30 }
//! }
//! ```
//!
//! Orchestrators branch on `ok` + `error.code`; `retry_after_seconds` is
//! populated only when the server supplied a sleep hint (HTTP 429) so callers
//! don't have to regex-scrape the human message to schedule their next try.

use serde::Serialize;

use crate::cmd::CmdError;
use crate::Environment;

#[derive(Debug, Serialize)]
pub struct Envelope {
    pub ok: bool,
    pub environment: &'static str,
    pub dry_run: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorPayload>,
    /// F15: machine-readable non-fatal security signals. Always present
    /// (possibly empty) so orchestrators can unconditionally `.jq`
    /// `.security_warnings | length > 0` without needing a null check.
    /// Populated by command code via [`Self::with_warnings`].
    pub security_warnings: Vec<SecurityWarning>,
}

/// One non-fatal security observation surfaced on stderr + envelope.
///
/// `code` is a stable identifier orchestrators can gate on (e.g.
/// `"custom_api_base"`, `"custom_tempo_rpc"`, `"password_env_var"`);
/// `message` is the human-readable detail. Kept flat rather than nested
/// so a JSON consumer can key on `.code` directly.
#[derive(Debug, Clone, Serialize)]
pub struct SecurityWarning {
    pub code: &'static str,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct ErrorPayload {
    pub code: &'static str,
    pub message: String,
    /// Server-directed sleep hint in whole seconds. Present only for
    /// rate-limited errors that carried a `Retry-After` header.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u64>,
}

impl Envelope {
    pub fn success(env: Environment, dry_run: bool, data: serde_json::Value) -> Self {
        Self {
            ok: true,
            environment: env.as_str(),
            dry_run,
            data: Some(data),
            error: None,
            security_warnings: Vec::new(),
        }
    }

    pub fn error(env: Environment, dry_run: bool, err: &CmdError) -> Self {
        Self {
            ok: false,
            environment: env.as_str(),
            dry_run,
            data: None,
            error: Some(ErrorPayload {
                code: err.code(),
                message: err.to_string(),
                retry_after_seconds: err.retry_after().map(|d| d.as_secs()),
            }),
            security_warnings: Vec::new(),
        }
    }

    /// Attach non-fatal security warnings to this envelope. Idempotent —
    /// appends to whatever is already set; callers can accumulate across
    /// the request pipeline.
    pub fn with_warnings(mut self, warnings: Vec<SecurityWarning>) -> Self {
        self.security_warnings.extend(warnings);
        self
    }

    pub fn emit(&self) {
        // Flush is implicit — stdout closes on process exit.
        let _ = serde_json::to_writer(std::io::stdout().lock(), self);
        println!();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn success_envelope_omits_error_field() {
        let env = Envelope::success(
            Environment::Prod,
            false,
            serde_json::json!({"agent_id": "ag_1"}),
        );
        let v = serde_json::to_value(&env).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["environment"], "production");
        assert_eq!(v["dry_run"], false);
        assert_eq!(v["data"]["agent_id"], "ag_1");
        assert!(v.get("error").is_none(), "error must be omitted on success");
        // F15: security_warnings is always present (possibly empty).
        assert_eq!(
            v["security_warnings"].as_array().map(Vec::len),
            Some(0),
            "security_warnings must be an empty array on healthy success"
        );
    }

    #[test]
    fn with_warnings_populates_the_array() {
        let env =
            Envelope::success(Environment::Prod, false, serde_json::json!({})).with_warnings(vec![
                SecurityWarning {
                    code: "custom_api_base",
                    message: "api_base overridden via --allow-custom-endpoints".into(),
                },
            ]);
        let v = serde_json::to_value(&env).unwrap();
        let arr = v["security_warnings"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["code"], "custom_api_base");
    }

    #[test]
    fn error_envelope_omits_data_field_and_carries_code() {
        let err = CmdError::Auth("401".into());
        let env = Envelope::error(Environment::Staging, false, &err);
        let v = serde_json::to_value(&env).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["environment"], "staging");
        assert_eq!(v["error"]["code"], "auth");
        assert!(v["error"]["message"].as_str().unwrap().contains("401"));
        assert!(v.get("data").is_none(), "data must be omitted on error");
        assert!(
            v["error"].get("retry_after_seconds").is_none(),
            "retry_after_seconds must be omitted when absent"
        );
    }

    #[test]
    fn rate_limited_error_surfaces_retry_after_seconds() {
        let err = CmdError::RateLimited {
            retry_after: Duration::from_secs(30),
        };
        let env = Envelope::error(Environment::Prod, false, &err);
        let v = serde_json::to_value(&env).unwrap();
        assert_eq!(v["error"]["code"], "rate_limited");
        assert_eq!(v["error"]["retry_after_seconds"], 30);
    }

    #[test]
    fn dry_run_flag_is_serialized_verbatim() {
        let env = Envelope::success(Environment::Local, true, serde_json::json!({}));
        let v = serde_json::to_value(&env).unwrap();
        assert_eq!(v["dry_run"], true);
        assert_eq!(v["environment"], "local");
    }
}
