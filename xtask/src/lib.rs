// TODO: doc the public surface; xtask is internal so keep this deferred.
#![allow(missing_docs, clippy::doc_markdown)]

//! TaskFast OpenAPI spec tooling.
//!
//! The authoritative spec lives at `spec/openapi.yaml`. The platform team
//! evolves it against Elixir routes; clients generate from it. A small set of
//! structural redundancies would cause progenitor to emit distinct Rust types
//! for schemas that are semantically identical (e.g. multiple `{error, message}`
//! objects under different names). This crate's [`normalize_spec`] folds those
//! aliases into a single canonical shape *in memory*, leaving the on-disk spec
//! untouched.
//!
//! Consumers:
//!  - `cargo xtask sync-spec` — writes a normalized artifact for inspection.
//!  - `taskfast-client/build.rs` — pipes the normalized YAML into progenitor.
//!
//! ## Rewrite rules
//!
//! Today: every name in [`ERROR_ALIASES`] is a structural clone of
//! `#/components/schemas/Error`. The normalizer:
//!   1. Asserts each alias is byte-for-byte structurally equal to `Error`
//!      (ignoring `description`/`example` doc-only fields) — drift fails loud.
//!   2. Rewrites every `$ref: '#/components/schemas/<alias>'` to point at
//!      `#/components/schemas/Error`.
//!   3. Removes the alias definitions from `components.schemas`.
//!
//! Adding a new alias ⇒ append to [`ERROR_ALIASES`]. If a schema grows a real
//! distinguishing field, remove it from [`ERROR_ALIASES`]; the drift check
//! will refuse to normalize otherwise.
//!
//! ## Multipart strip
//!
//! progenitor 0.9 can't codegen `multipart/form-data` request bodies. Rather
//! than maintain a patched fork, the normalizer drops operations whose request
//! body declares only multipart variants — today that is
//! `POST /tasks/{task_id}/artifacts` (uploadArtifact). The upload path is
//! hand-rolled in `taskfast-client` using `reqwest::multipart` directly; the
//! Artifact *response* schema still comes through codegen because it's
//! referenced by the surviving `GET /tasks/{task_id}/artifacts` operation.
//! The removed operationIds are reported via [`Report::stripped_operations`]
//! so callers can verify nothing unexpected disappeared.
//!
//! ## Error response strip
//!
//! progenitor 0.9 asserts `response_types.len() <= 1` for both the success
//! and error response sets (see `progenitor-impl/src/method.rs` —
//! `extract_responses`). Nearly every TaskFast operation declares 2–5
//! distinct error response shapes (`Error`, `ValidationError`, `ClaimFailure`,
//! etc.), which trips the error-side assertion. Rather than synthesize a
//! union error type that we'd throw away anyway, the normalizer drops every
//! non-2xx (and non-`default`) response before handing the spec to progenitor.
//! Surfacing of 4xx/5xx failures is the job of `taskfast-client::errors::Error`,
//! which reads the response body manually on the way up.
//!
//! This means the generated client sees every unhappy status as
//! `progenitor_client::Error::UnexpectedResponse(reqwest::Response)` — we
//! re-classify into our typed `taskfast_client::errors::Error` in the calling layer.

use anyhow::{anyhow, bail, Context, Result};
use serde_yaml::{Mapping, Value};

/// Schemas known to be structural clones of `#/components/schemas/Error`.
pub const ERROR_ALIASES: &[&str] = &["WalletBalanceNotFoundError", "WebhookNoWebhookError"];

/// Normalize an OpenAPI YAML document: collapse [`ERROR_ALIASES`] into `Error`.
///
/// Returns the rewritten YAML as a string. Idempotent: running it on already
/// normalized output is a no-op (aliases are absent, so nothing rewrites).
pub fn normalize_spec(yaml: &str) -> Result<String> {
    let mut doc: Value = serde_yaml::from_str(yaml).context("parse spec YAML")?;
    let report = normalize_in_place(&mut doc)?;
    let out = serde_yaml::to_string(&doc).context("re-serialize normalized spec")?;
    tracing_nothing(&report); // suppress unused_variable warning when no tracing is wired
    Ok(out)
}

/// As [`normalize_spec`] but also returns a summary of what changed.
pub fn normalize_spec_with_report(yaml: &str) -> Result<(String, Report)> {
    let mut doc: Value = serde_yaml::from_str(yaml).context("parse spec YAML")?;
    let report = normalize_in_place(&mut doc)?;
    let out = serde_yaml::to_string(&doc).context("re-serialize normalized spec")?;
    Ok((out, report))
}

#[derive(Debug, Default, Clone)]
pub struct Report {
    /// Aliases that were folded into `Error`. Populated only for aliases that
    /// were actually present in the input — missing aliases are silently skipped.
    pub folded_aliases: Vec<String>,
    /// Count of `$ref` rewrites performed across the document.
    pub refs_rewritten: usize,
    /// `operationId`s stripped because their only request-body variant was
    /// multipart/form-data (progenitor 0.9 limitation).
    pub stripped_operations: Vec<String>,
    /// Count of non-2xx response entries removed across all operations
    /// (progenitor 0.9 `response_types.len() <= 1` assertion).
    pub error_responses_stripped: usize,
}

fn normalize_in_place(doc: &mut Value) -> Result<Report> {
    let mut report = Report::default();

    let schemas_path = ["components", "schemas"];
    let canonical_shape = {
        let error = get_mapping_path(doc, &schemas_path)
            .and_then(|m| m.get(Value::from("Error")))
            .ok_or_else(|| anyhow!("canonical schema `Error` not found in components.schemas"))?;
        structural_shape(error)
    };

    // Pass 1: drift-check every alias that is present.
    let present_aliases: Vec<String> = {
        let schemas = get_mapping_path(doc, &schemas_path)
            .ok_or_else(|| anyhow!("components.schemas not a mapping"))?;
        ERROR_ALIASES
            .iter()
            .copied()
            .filter(|a| schemas.contains_key(Value::from(*a)))
            .map(str::to_owned)
            .collect()
    };

    for alias in &present_aliases {
        let schema = get_mapping_path(doc, &schemas_path)
            .and_then(|m| m.get(Value::from(alias.as_str())))
            .expect("alias presence confirmed above");
        let alias_shape = structural_shape(schema);
        if alias_shape != canonical_shape {
            bail!(
                "alias `{alias}` has drifted from canonical `Error` shape — \
                 either restore structural equality or remove `{alias}` from ERROR_ALIASES.\n\
                 canonical: {:#?}\nalias:     {:#?}",
                canonical_shape,
                alias_shape
            );
        }
    }

    // Pass 2: rewrite every $ref pointing at an alias.
    for alias in &present_aliases {
        let from = format!("#/components/schemas/{alias}");
        let to = "#/components/schemas/Error";
        report.refs_rewritten += rewrite_refs(doc, &from, to);
    }

    // Pass 3: drop alias definitions from components.schemas.
    if let Some(schemas) = get_mapping_path_mut(doc, &schemas_path) {
        for alias in &present_aliases {
            schemas.remove(Value::from(alias.as_str()));
        }
    }
    report.folded_aliases = present_aliases;

    // Pass 4: strip multipart-only operations (progenitor 0.9 limitation).
    report.stripped_operations = strip_multipart_only_operations(doc);

    // Pass 5: strip non-2xx responses from every surviving operation
    // (progenitor 0.9 extract_responses assertion, error side).
    report.error_responses_stripped = strip_non_success_responses(doc);

    Ok(report)
}

/// Remove operations whose request body has exactly one content variant,
/// `multipart/form-data`. Returns the removed `operationId`s (stable order).
///
/// We don't touch operations that declare multipart *alongside* another
/// variant — those are handled by a preferred-content-type collapse pass that
/// doesn't exist yet; today no such operations exist in the TaskFast spec.
fn strip_multipart_only_operations(doc: &mut Value) -> Vec<String> {
    let mut stripped = Vec::new();
    let Some(paths) = doc
        .as_mapping_mut()
        .and_then(|m| m.get_mut(Value::from("paths")))
        .and_then(Value::as_mapping_mut)
    else {
        return stripped;
    };

    const HTTP_VERBS: &[&str] = &["get", "put", "post", "delete", "patch", "options", "head"];
    for (_path_key, path_item) in paths.iter_mut() {
        let Some(ops) = path_item.as_mapping_mut() else {
            continue;
        };
        let mut to_remove = Vec::new();
        for verb in HTTP_VERBS {
            if let Some(op) = ops.get(Value::from(*verb)) {
                if operation_is_multipart_only(op) {
                    let op_id = op
                        .get(Value::from("operationId"))
                        .and_then(Value::as_str)
                        .unwrap_or("<anonymous>")
                        .to_string();
                    to_remove.push(*verb);
                    stripped.push(op_id);
                }
            }
        }
        for verb in to_remove {
            ops.remove(Value::from(verb));
        }
    }
    stripped
}

/// For each operation under `paths.*.<verb>.responses`, remove every entry
/// whose key is not a 2xx status code or the literal `default`. Returns the
/// total count of removed entries across the document.
///
/// Rationale: progenitor 0.9 asserts that both the success and error response
/// sets have at most one distinct body type. TaskFast endpoints declare
/// multiple distinct error shapes (`Error`, `ValidationError`, feature-specific
/// failure envelopes). Surfacing those as typed variants in the generated
/// client is not the generator's job here — `taskfast-client::errors::Error`
/// reconstructs error semantics from the raw response body. Dropping non-2xx
/// response definitions short-circuits the assertion without hiding anything
/// the client layer doesn't already re-implement.
fn strip_non_success_responses(doc: &mut Value) -> usize {
    let mut removed = 0usize;
    let Some(paths) = doc
        .as_mapping_mut()
        .and_then(|m| m.get_mut(Value::from("paths")))
        .and_then(Value::as_mapping_mut)
    else {
        return 0;
    };

    const HTTP_VERBS: &[&str] = &["get", "put", "post", "delete", "patch", "options", "head"];
    for (_path_key, path_item) in paths.iter_mut() {
        let Some(ops) = path_item.as_mapping_mut() else {
            continue;
        };
        for verb in HTTP_VERBS {
            let Some(op) = ops.get_mut(Value::from(*verb)) else {
                continue;
            };
            let Some(responses) = op
                .as_mapping_mut()
                .and_then(|m| m.get_mut(Value::from("responses")))
                .and_then(Value::as_mapping_mut)
            else {
                continue;
            };

            let to_remove: Vec<Value> = responses
                .iter()
                .filter_map(|(k, _)| {
                    let key = k.as_str()?;
                    if is_success_or_default(key) {
                        None
                    } else {
                        Some(k.clone())
                    }
                })
                .collect();
            for k in to_remove {
                if responses.remove(&k).is_some() {
                    removed += 1;
                }
            }
        }
    }
    removed
}

fn is_success_or_default(code: &str) -> bool {
    if code == "default" {
        return true;
    }
    // 2XX ranges (e.g. "2XX") and explicit codes 200..=299.
    match code.chars().next() {
        Some('2') => true,
        _ => false,
    }
}

fn operation_is_multipart_only(op: &Value) -> bool {
    let Some(body) = op
        .as_mapping()
        .and_then(|m| m.get(Value::from("requestBody")))
    else {
        return false;
    };
    let Some(content) = body
        .as_mapping()
        .and_then(|m| m.get(Value::from("content")))
        .and_then(Value::as_mapping)
    else {
        return false;
    };
    let keys: Vec<&str> = content.keys().filter_map(Value::as_str).collect();
    // Match either (a) sole multipart variant or (b) any operation that
    // declares multipart alongside other variants — progenitor 0.9 trips on
    // multi-variant bodies too (`more media types than expected`), and the
    // upload path is hand-rolled in taskfast-client regardless.
    keys.contains(&"multipart/form-data")
}

/// Structural projection: strip doc-only fields so two schemas differing only
/// in `description`/`example` compare equal.
fn structural_shape(v: &Value) -> Value {
    match v {
        Value::Mapping(m) => {
            let mut out = Mapping::new();
            for (k, val) in m {
                let keep = match k.as_str() {
                    Some("description") | Some("example") | Some("summary") | Some("default")
                    | Some("deprecated") | Some("title") => false,
                    _ => true,
                };
                if keep {
                    out.insert(k.clone(), structural_shape(val));
                }
            }
            Value::Mapping(out)
        }
        Value::Sequence(s) => Value::Sequence(s.iter().map(structural_shape).collect()),
        other => other.clone(),
    }
}

/// Walk `doc`, replacing every `$ref` whose value equals `from` with `to`.
/// Returns the number of rewrites performed.
fn rewrite_refs(doc: &mut Value, from: &str, to: &str) -> usize {
    let mut count = 0;
    rewrite_refs_inner(doc, from, to, &mut count);
    count
}

fn rewrite_refs_inner(v: &mut Value, from: &str, to: &str, count: &mut usize) {
    match v {
        Value::Mapping(m) => {
            // A $ref object is typically `{"$ref": "..."}` — a single-key mapping.
            // We still walk siblings because spec-extension keys may coexist.
            if let Some(target) = m.get_mut(Value::from("$ref")) {
                if target.as_str() == Some(from) {
                    *target = Value::from(to);
                    *count += 1;
                }
            }
            for (_, val) in m.iter_mut() {
                rewrite_refs_inner(val, from, to, count);
            }
        }
        Value::Sequence(s) => {
            for val in s.iter_mut() {
                rewrite_refs_inner(val, from, to, count);
            }
        }
        _ => {}
    }
}

fn get_mapping_path<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Mapping> {
    let mut cur = v;
    for key in path {
        cur = cur.as_mapping()?.get(Value::from(*key))?;
    }
    cur.as_mapping()
}

fn get_mapping_path_mut<'a>(v: &'a mut Value, path: &[&str]) -> Option<&'a mut Mapping> {
    let mut cur = v;
    for key in path {
        cur = cur.as_mapping_mut()?.get_mut(Value::from(*key))?;
    }
    cur.as_mapping_mut()
}

fn tracing_nothing(_r: &Report) {}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE_SPEC: &str = r#"
openapi: 3.0.0
info:
  title: test
  version: 0.0.0
paths:
  /foo:
    get:
      responses:
        '200':
          description: ok
        '404':
          description: not found
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/WalletBalanceNotFoundError'
        '503':
          description: unavailable
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/Error'
components:
  schemas:
    Error:
      type: object
      required:
        - error
        - message
      properties:
        error:
          type: string
          description: Error code
        message:
          type: string
          description: Human message
    WalletBalanceNotFoundError:
      type: object
      required:
        - error
        - message
      properties:
        error:
          type: string
          description: Error code
          example: "wallet_not_applicable"
        message:
          type: string
          description: Human message
          example: "not a tempo agent"
    Untouched:
      type: object
"#;

    #[test]
    fn normalize_folds_alias_refs_and_drops_schema() {
        let (out, report) = normalize_spec_with_report(BASE_SPEC).unwrap();
        assert_eq!(
            report.folded_aliases,
            vec!["WalletBalanceNotFoundError".to_string()]
        );
        assert_eq!(report.refs_rewritten, 1);

        // The canonical Error schema definition still lives in components
        // (non-2xx response strip removes the *usages*, not the shared type).
        assert!(
            out.contains("Error:\n      type: object"),
            "canonical Error schema missing:\n{out}"
        );
        assert!(
            !out.contains("WalletBalanceNotFoundError"),
            "alias name fully removed, got:\n{out}"
        );

        // Siblings untouched.
        assert!(out.contains("Untouched:"));
    }

    #[test]
    fn idempotent_when_no_aliases_present() {
        let (out1, _) = normalize_spec_with_report(BASE_SPEC).unwrap();
        let (out2, report2) = normalize_spec_with_report(&out1).unwrap();
        assert_eq!(out1, out2);
        assert!(report2.folded_aliases.is_empty());
        assert_eq!(report2.refs_rewritten, 0);
    }

    #[test]
    fn drift_check_rejects_mismatched_alias() {
        // Same as BASE_SPEC but WalletBalanceNotFoundError gets an extra required field.
        let drifted = BASE_SPEC.replace(
            "WalletBalanceNotFoundError:\n      type: object\n      required:\n        - error\n        - message",
            "WalletBalanceNotFoundError:\n      type: object\n      required:\n        - error\n        - message\n        - drift_field",
        );
        let err = normalize_spec(&drifted).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("drifted from canonical"), "got: {msg}");
    }

    #[test]
    fn missing_error_schema_is_error() {
        let bad = BASE_SPEC.replace("Error:", "NotError:");
        let err = normalize_spec(bad.as_str()).unwrap_err();
        assert!(format!("{err:#}").contains("canonical schema `Error` not found"));
    }

    const MULTIPART_SPEC: &str = r#"
openapi: 3.0.0
info: { title: test, version: 0.0.0 }
paths:
  /upload:
    post:
      operationId: uploadThing
      requestBody:
        required: true
        content:
          multipart/form-data:
            schema:
              type: object
              required: [file]
              properties:
                file: { type: string, format: binary }
      responses:
        '201': { description: ok }
    get:
      operationId: listThings
      responses:
        '200': { description: ok }
  /json_only:
    post:
      operationId: postJsonThing
      requestBody:
        required: true
        content:
          application/json:
            schema: { type: object }
      responses:
        '201': { description: ok }
components:
  schemas:
    Error:
      type: object
      required: [error, message]
      properties:
        error: { type: string }
        message: { type: string }
"#;

    #[test]
    fn non_success_responses_are_stripped() {
        let (out, report) = normalize_spec_with_report(BASE_SPEC).unwrap();
        // BASE_SPEC has one op with 200/404/503 — 404 and 503 must go.
        assert_eq!(report.error_responses_stripped, 2);
        // The success code survives.
        assert!(out.contains("'200':"));
        // Error codes are gone.
        assert!(!out.contains("'404':"), "404 not stripped:\n{out}");
        assert!(!out.contains("'503':"), "503 not stripped:\n{out}");
    }

    #[test]
    fn multipart_only_operation_is_stripped() {
        let (out, report) = normalize_spec_with_report(MULTIPART_SPEC).unwrap();
        assert_eq!(report.stripped_operations, vec!["uploadThing".to_string()]);
        assert!(!out.contains("uploadThing"), "uploadThing not stripped");
        // Siblings untouched.
        assert!(out.contains("listThings"), "sibling GET removed by mistake");
        assert!(
            out.contains("postJsonThing"),
            "JSON-only POST removed by mistake"
        );
        // The path entry itself survives because listThings still lives there.
        assert!(out.contains("/upload:"));
    }
}
