//! `cargo xtask <cmd>` — repo automation entrypoint.

#![allow(missing_docs, clippy::doc_markdown)]

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "xtask", about = "TaskFast SDK repo automation.")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Normalize the OpenAPI spec and write the result next to the input.
    ///
    /// Reads `spec/openapi.yaml`, folds structurally-identical error schemas
    /// (see `xtask::ERROR_ALIASES`) into `#/components/schemas/Error`, and
    /// writes the result to `spec/openapi.normalized.yaml`. The on-disk
    /// authoritative spec is not modified.
    SyncSpec {
        /// Path to the input spec (default: `spec/openapi.yaml` relative to cwd).
        #[arg(long, default_value = "spec/openapi.yaml")]
        input: PathBuf,
        /// Path to write the normalized output (default: `spec/openapi.normalized.yaml`).
        #[arg(long, default_value = "spec/openapi.normalized.yaml")]
        output: PathBuf,
        /// Don't write output; just report what would change. Exit 0.
        #[arg(long)]
        dry_run: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::SyncSpec {
            input,
            output,
            dry_run,
        } => run_sync_spec(&input, &output, dry_run),
    }
}

fn run_sync_spec(input: &std::path::Path, output: &std::path::Path, dry_run: bool) -> Result<()> {
    let src = std::fs::read_to_string(input)
        .with_context(|| format!("read spec from {}", input.display()))?;

    let (normalized, report) =
        xtask::normalize_spec_with_report(&src).context("normalize spec in-memory")?;

    eprintln!(
        "sync-spec: folded {} alias(es), rewrote {} $ref(s), stripped {} multipart op(s), dropped {} non-2xx response(s)",
        report.folded_aliases.len(),
        report.refs_rewritten,
        report.stripped_operations.len(),
        report.error_responses_stripped,
    );
    if !report.folded_aliases.is_empty() {
        eprintln!("  folded:    {}", report.folded_aliases.join(", "));
    }
    if !report.stripped_operations.is_empty() {
        eprintln!("  stripped:  {}", report.stripped_operations.join(", "));
    }

    if dry_run {
        eprintln!(
            "sync-spec: --dry-run, skipping write to {}",
            output.display()
        );
    } else {
        std::fs::write(output, &normalized)
            .with_context(|| format!("write normalized spec to {}", output.display()))?;
        eprintln!(
            "sync-spec: wrote {} ({} bytes)",
            output.display(),
            normalized.len()
        );
    }
    Ok(())
}
