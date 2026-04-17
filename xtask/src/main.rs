//! `cargo xtask <cmd>` — repo automation entrypoint.

#![allow(missing_docs, clippy::doc_markdown)]

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use toml_edit::{value, DocumentMut};

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
    /// Bump the workspace version in `Cargo.toml` plus every synced inline
    /// dep ref that pins it (see `synced_sites()`), then refresh `Cargo.lock`.
    ///
    /// Affects `taskfast-cli` + `taskfast-agent` + `xtask` (all use
    /// `version.workspace = true`). Does not bump `taskfast-client` or
    /// `taskfast-chains` — they version independently.
    Bump {
        /// Which semver component to increment.
        level: BumpLevel,
        /// Skip running `cargo check` to refresh `Cargo.lock`.
        #[arg(long)]
        no_lock: bool,
        /// Print what would change without writing any file.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum BumpLevel {
    Major,
    Minor,
    Patch,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::SyncSpec {
            input,
            output,
            dry_run,
        } => run_sync_spec(&input, &output, dry_run),
        Cmd::Bump {
            level,
            no_lock,
            dry_run,
        } => run_bump(level, no_lock, dry_run),
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

/// A Cargo.toml key whose value must track the workspace version — because
/// the dep target uses `version.workspace = true` and Cargo enforces that
/// `path + version` inline deps match the target's declared version.
///
/// Keep this list exhaustive. A missed site will be caught by `cargo check`
/// the next bump, but failing loud *before* writing is friendlier.
struct SyncedSite {
    /// Path relative to workspace root.
    file: &'static str,
    /// Dotted TOML key path, e.g. `workspace.package.version`.
    toml_path: &'static [&'static str],
}

fn synced_sites() -> &'static [SyncedSite] {
    &[
        SyncedSite {
            file: "Cargo.toml",
            toml_path: &["workspace", "package", "version"],
        },
        SyncedSite {
            file: "Cargo.toml",
            toml_path: &["workspace", "dependencies", "taskfast-agent", "version"],
        },
        // taskfast-client has a build-dep on xtask; xtask inherits workspace
        // version via `version.workspace = true`, so this inline pin must match.
        SyncedSite {
            file: "crates/taskfast-client/Cargo.toml",
            toml_path: &["build-dependencies", "xtask", "version"],
        },
    ]
}

fn run_bump(level: BumpLevel, no_lock: bool, dry_run: bool) -> Result<()> {
    let workspace_root = find_workspace_root().context("locate workspace root")?;
    let sites = synced_sites();

    // Load each distinct file once. Multiple sites in the same file must share
    // one DocumentMut so sequential edits don't clobber each other on write.
    let mut docs: Vec<(PathBuf, DocumentMut)> = Vec::new();
    for site in sites {
        let path = workspace_root.join(site.file);
        if docs.iter().any(|(p, _)| p == &path) {
            continue;
        }
        let src =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let doc: DocumentMut = src
            .parse()
            .with_context(|| format!("parse {}", path.display()))?;
        docs.push((path, doc));
    }

    let doc_for = |file: &str, docs: &[(PathBuf, DocumentMut)]| -> usize {
        let target = workspace_root.join(file);
        docs.iter()
            .position(|(p, _)| p == &target)
            .expect("file preloaded")
    };

    // Validate: every site's value equals the first site's (authoritative).
    let current = read_toml_string(&docs[doc_for(sites[0].file, &docs)].1, sites[0].toml_path)
        .with_context(|| format!("{} @ {}", sites[0].file, sites[0].toml_path.join(".")))?
        .to_owned();
    for site in &sites[1..] {
        let v = read_toml_string(&docs[doc_for(site.file, &docs)].1, site.toml_path)
            .with_context(|| format!("{} @ {}", site.file, site.toml_path.join(".")))?;
        if v != current {
            bail!(
                "synced site {}@{} = {v} but workspace version = {current}; \
                 fix manually before bumping",
                site.file,
                site.toml_path.join("."),
            );
        }
    }

    let next = bump_semver(&current, level)?;

    eprintln!("bump: {current} -> {next} ({level:?})");
    for site in sites {
        eprintln!("  touched: {} @ {}", site.file, site.toml_path.join("."));
    }

    if dry_run {
        eprintln!("bump: --dry-run, no files written");
        return Ok(());
    }

    // Apply all edits in-memory, then flush each distinct doc once.
    for site in sites {
        let idx = doc_for(site.file, &docs);
        write_toml_string(&mut docs[idx].1, site.toml_path, &next);
    }
    for (path, doc) in &docs {
        std::fs::write(path, doc.to_string())
            .with_context(|| format!("write {}", path.display()))?;
        eprintln!("bump: wrote {}", path.display());
    }

    if no_lock {
        eprintln!("bump: --no-lock, skipping `cargo check`");
    } else {
        refresh_lockfile(&workspace_root).context("refresh Cargo.lock via `cargo check`")?;
    }

    eprintln!("bump: done. review: git diff");
    Ok(())
}

fn read_toml_string<'a>(doc: &'a DocumentMut, path: &[&str]) -> Result<&'a str> {
    let mut item = doc.as_item();
    for key in path {
        item = item
            .get(key)
            .with_context(|| format!("key `{key}` missing"))?;
    }
    item.as_str().context("expected string value")
}

fn write_toml_string(doc: &mut DocumentMut, path: &[&str], new: &str) {
    let (last, prefix) = path.split_last().expect("non-empty path");
    let mut item = doc.as_item_mut();
    for key in prefix {
        item = &mut item[*key];
    }
    item[*last] = value(new);
}

fn bump_semver(current: &str, level: BumpLevel) -> Result<String> {
    // Reject pre-release / build metadata — keep scope tight. Revisit if needed.
    if current.contains('-') || current.contains('+') {
        bail!(
            "pre-release/build-metadata version `{current}` not supported; \
             bump manually or extend xtask",
        );
    }
    let parts: Vec<&str> = current.split('.').collect();
    if parts.len() != 3 {
        bail!("version `{current}` is not MAJOR.MINOR.PATCH");
    }
    let parse = |s: &str, field: &str| -> Result<u64> {
        s.parse::<u64>()
            .with_context(|| format!("parse {field} of `{current}`"))
    };
    let (major, minor, patch) = (
        parse(parts[0], "major")?,
        parse(parts[1], "minor")?,
        parse(parts[2], "patch")?,
    );
    let (m, n, p) = match level {
        BumpLevel::Major => (major + 1, 0, 0),
        BumpLevel::Minor => (major, minor + 1, 0),
        BumpLevel::Patch => (major, minor, patch + 1),
    };
    Ok(format!("{m}.{n}.{p}"))
}

fn refresh_lockfile(workspace_root: &Path) -> Result<()> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    eprintln!("bump: running `cargo check --workspace` to refresh Cargo.lock");
    let status = Command::new(&cargo)
        .arg("check")
        .arg("--workspace")
        .current_dir(workspace_root)
        .status()
        .with_context(|| format!("spawn {}", cargo.to_string_lossy()))?;
    if !status.success() {
        bail!("`cargo check --workspace` failed with {status}");
    }
    Ok(())
}

fn find_workspace_root() -> Result<PathBuf> {
    let start = std::env::current_dir().context("cwd")?;
    for dir in start.ancestors() {
        let candidate = dir.join("Cargo.toml");
        if !candidate.is_file() {
            continue;
        }
        let text = std::fs::read_to_string(&candidate)
            .with_context(|| format!("read {}", candidate.display()))?;
        if text.contains("[workspace]") {
            return Ok(dir.to_path_buf());
        }
    }
    bail!(
        "no ancestor Cargo.toml with [workspace] found from {}",
        start.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bump_patch_basic() {
        assert_eq!(bump_semver("0.2.1", BumpLevel::Patch).unwrap(), "0.2.2");
    }

    #[test]
    fn bump_minor_resets_patch() {
        assert_eq!(bump_semver("0.2.5", BumpLevel::Minor).unwrap(), "0.3.0");
    }

    #[test]
    fn bump_major_resets_minor_and_patch() {
        assert_eq!(bump_semver("1.4.7", BumpLevel::Major).unwrap(), "2.0.0");
    }

    #[test]
    fn bump_handles_zero_versions() {
        assert_eq!(bump_semver("0.0.0", BumpLevel::Patch).unwrap(), "0.0.1");
        assert_eq!(bump_semver("0.0.0", BumpLevel::Minor).unwrap(), "0.1.0");
        assert_eq!(bump_semver("0.0.0", BumpLevel::Major).unwrap(), "1.0.0");
    }

    #[test]
    fn bump_rejects_prerelease() {
        let err = bump_semver("0.2.1-alpha", BumpLevel::Patch).unwrap_err();
        assert!(
            err.to_string().contains("pre-release"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn bump_rejects_build_metadata() {
        let err = bump_semver("0.2.1+build.7", BumpLevel::Patch).unwrap_err();
        assert!(
            err.to_string().contains("pre-release"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn bump_rejects_two_segment_version() {
        let err = bump_semver("1.2", BumpLevel::Patch).unwrap_err();
        assert!(
            err.to_string().contains("MAJOR.MINOR.PATCH"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn bump_rejects_non_numeric_segment() {
        let err = bump_semver("0.x.0", BumpLevel::Patch).unwrap_err();
        assert!(
            err.to_string().contains("parse minor"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn toml_roundtrip_preserves_formatting() {
        // Note: toml_edit's `value()` replaces trailing decor on the value
        // itself, so inline comments on the bumped line are not preserved.
        // Structural comments (section banners, free-floating) survive, which
        // is what actually matters for diff ergonomics.
        let src = r#"# top comment
[workspace.package]
version = "0.2.1"

# between sections
[workspace.dependencies]
taskfast-agent = { path = "crates/taskfast-agent", version = "0.2.1" }
"#;
        let mut doc: DocumentMut = src.parse().unwrap();
        assert_eq!(
            read_toml_string(&doc, &["workspace", "package", "version"]).unwrap(),
            "0.2.1"
        );
        write_toml_string(&mut doc, &["workspace", "package", "version"], "0.3.0");
        write_toml_string(
            &mut doc,
            &["workspace", "dependencies", "taskfast-agent", "version"],
            "0.3.0",
        );
        let out = doc.to_string();
        assert!(out.contains("# top comment"), "lost top comment");
        assert!(out.contains("# between sections"), "lost section comment");
        assert!(out.contains("version = \"0.3.0\""));
        assert!(!out.contains("\"0.2.1\""));
    }
}
