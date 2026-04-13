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

use anyhow::{Context, Result, anyhow, bail};
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

    Ok(report)
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
        assert_eq!(report.folded_aliases, vec!["WalletBalanceNotFoundError".to_string()]);
        assert_eq!(report.refs_rewritten, 1);

        // The alias ref is rewritten.
        assert!(
            out.contains("#/components/schemas/Error"),
            "canonical ref still present"
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
}
