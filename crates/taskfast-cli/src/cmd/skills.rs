//! `taskfast skills` — install the bundled TaskFast agent skill locally.
//!
//! The command copies the embedded `taskfast-agent` skill tree into both
//! `./.claude/skills/taskfast-agent/` and `./.agents/skills/taskfast-agent/`
//! under the current working directory.
//!
//! Mutation guard:
//! * `--dry-run` reports the install plan and never prompts or writes.
//! * `--yes` skips the prompt and installs immediately.
//! * Interactive TTY without `--yes` prompts for confirmation.
//! * Non-interactive without `--yes` fails closed with a usage error.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use clap::Parser;
use dialoguer::{theme::ColorfulTheme, Confirm};
use serde::Serialize;
use serde_json::json;

use super::{CmdError, CmdResult, Ctx};
use crate::envelope::Envelope;

const SKILL_NAME: &str = "taskfast-agent";
const SOURCE_KIND: &str = "embedded";
const DEST_ROOTS: &[&str] = &[".claude/skills", ".agents/skills"];

macro_rules! bundled_file {
    ($path:literal) => {
        BundledFile {
            relative_path: $path,
            contents: include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../skills/taskfast-agent/",
                $path
            )),
        }
    };
}

#[derive(Debug, Parser)]
/// Install the bundled TaskFast agent skill into local tool folders.
pub struct Args {
    /// Approve installation without a TTY confirmation prompt.
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, Clone, Copy)]
struct BundledFile {
    relative_path: &'static str,
    contents: &'static str,
}

const BUNDLED_FILES: &[BundledFile] = &[
    bundled_file!("SKILL.md"),
    bundled_file!("reference/BOOT.md"),
    bundled_file!("reference/POSTER.md"),
    bundled_file!("reference/SETUP.md"),
    bundled_file!("reference/STATES.md"),
    bundled_file!("reference/TROUBLESHOOTING.md"),
    bundled_file!("reference/WORKER.md"),
];

#[derive(Debug, Serialize)]
struct InstallTarget {
    path: PathBuf,
    status: &'static str,
}

trait ConfirmPrompter {
    fn confirm_install(&self, skill_name: &str, destinations: &[PathBuf]) -> io::Result<bool>;
}

struct DialoguerConfirmPrompter;

impl ConfirmPrompter for DialoguerConfirmPrompter {
    fn confirm_install(&self, skill_name: &str, destinations: &[PathBuf]) -> io::Result<bool> {
        eprintln!("Install bundled skill `{skill_name}` into:");
        for destination in destinations {
            eprintln!("  - {}", destination.display());
        }

        Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt("Proceed")
            .default(true)
            .interact()
            .map_err(dialoguer_err)
    }
}

fn dialoguer_err(error: dialoguer::Error) -> io::Error {
    match error {
        dialoguer::Error::IO(io) => io,
    }
}

#[allow(clippy::unused_async)]
/// Install the embedded skill tree into the current working directory.
///
/// # Errors
///
/// Returns [`CmdError::Usage`] when the current directory cannot be resolved,
/// confirmation cannot be collected, or filesystem writes fail.
pub async fn run(ctx: &Ctx, args: Args) -> CmdResult {
    let cwd = std::env::current_dir().map_err(|error| {
        CmdError::Usage(format!("failed to resolve current directory: {error}"))
    })?;
    let interactive = !args.yes && crate::cmd::init_tui::is_interactive();
    run_with_prompter(ctx, args, &DialoguerConfirmPrompter, interactive, &cwd)
}

fn run_with_prompter<P: ConfirmPrompter>(
    ctx: &Ctx,
    args: Args,
    prompter: &P,
    interactive: bool,
    cwd: &Path,
) -> CmdResult {
    let destinations = destination_paths(cwd);
    if ctx.dry_run {
        return Ok(status_envelope(
            ctx,
            cwd,
            "dry_run",
            "no",
            "would_install",
            &destinations,
        ));
    }

    let (prompted, approval) = if args.yes {
        ("no", "yes_flag")
    } else if interactive {
        let approved = prompter
            .confirm_install(SKILL_NAME, &destinations)
            .map_err(|error| CmdError::Usage(format!("install confirmation failed: {error}")))?;
        if !approved {
            return Ok(status_envelope(
                ctx,
                cwd,
                "declined",
                "yes",
                "skipped",
                &destinations,
            ));
        }
        ("yes", "prompt")
    } else {
        return Err(CmdError::Usage(
            "refusing to install bundled skills without confirmation: rerun with --yes or use an interactive TTY".into(),
        ));
    };

    for destination in &destinations {
        install_into(destination)?;
    }

    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        status_data(cwd, approval, prompted, "installed", &destinations),
    ))
}

fn status_envelope(
    ctx: &Ctx,
    cwd: &Path,
    approval: &'static str,
    prompted: &'static str,
    status: &'static str,
    destinations: &[PathBuf],
) -> Envelope {
    Envelope::success(
        ctx.environment,
        ctx.dry_run,
        status_data(cwd, approval, prompted, status, destinations),
    )
}

fn status_data(
    cwd: &Path,
    approval: &'static str,
    prompted: &'static str,
    status: &'static str,
    destinations: &[PathBuf],
) -> serde_json::Value {
    json!({
        "skill": SKILL_NAME,
        "source": SOURCE_KIND,
        "cwd": cwd.display().to_string(),
        "approval": approval,
        "prompted": prompted,
        "targets": targets_with_status(destinations, status),
    })
}

fn destination_paths(cwd: &Path) -> Vec<PathBuf> {
    DEST_ROOTS
        .iter()
        .map(|root| cwd.join(root).join(SKILL_NAME))
        .collect()
}

fn targets_with_status(paths: &[PathBuf], status: &'static str) -> Vec<InstallTarget> {
    paths
        .iter()
        .map(|path| InstallTarget {
            path: path.clone(),
            status,
        })
        .collect()
}

fn install_into(destination: &Path) -> Result<(), CmdError> {
    fs::create_dir_all(destination)
        .map_err(|error| io_error("create install directory", destination, error))?;

    for file in BUNDLED_FILES {
        let path = destination.join(file.relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| io_error("create install directory", parent, error))?;
        }
        fs::write(&path, file.contents)
            .map_err(|error| io_error("write bundled skill file", &path, error))?;
    }
    Ok(())
}

fn io_error(action: &str, path: &Path, error: io::Error) -> CmdError {
    CmdError::Usage(format!("{action} `{}`: {error}", path.display()))
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::Environment;
    use std::cell::Cell;
    use tempfile::TempDir;

    struct MockPrompter {
        answer: bool,
        calls: Cell<usize>,
    }

    impl MockPrompter {
        fn approving() -> Self {
            Self {
                answer: true,
                calls: Cell::new(0),
            }
        }

        fn declining() -> Self {
            Self {
                answer: false,
                calls: Cell::new(0),
            }
        }
    }

    impl ConfirmPrompter for MockPrompter {
        fn confirm_install(
            &self,
            _skill_name: &str,
            _destinations: &[PathBuf],
        ) -> io::Result<bool> {
            self.calls.set(self.calls.get() + 1);
            Ok(self.answer)
        }
    }

    fn ctx(dry_run: bool) -> Ctx {
        Ctx {
            environment: Environment::Local,
            config_path: PathBuf::from("/dev/null"),
            dry_run,
            quiet: true,
            ..Default::default()
        }
    }

    fn args(yes: bool) -> Args {
        Args { yes }
    }

    fn target_path(tmp: &TempDir, root: &str) -> PathBuf {
        tmp.path().join(root).join(SKILL_NAME)
    }

    fn skill_file(root: &Path, relative_path: &str) -> PathBuf {
        root.join(relative_path)
    }

    fn envelope_value(envelope: &Envelope) -> serde_json::Value {
        serde_json::to_value(envelope).expect("serialize envelope")
    }

    #[test]
    fn run_with_prompter_should_fail_closed_when_non_interactive_and_unapproved() {
        let tmp = TempDir::new().expect("tempdir");
        let prompter = MockPrompter::approving();

        let error = run_with_prompter(&ctx(false), args(false), &prompter, false, tmp.path())
            .expect_err("non-interactive install without --yes must fail");

        match error {
            CmdError::Usage(message) => assert!(message.contains("--yes"), "{message}"),
            other => panic!("expected usage error, got {other:?}"),
        }
        assert_eq!(prompter.calls.get(), 0);
    }

    #[test]
    fn run_with_prompter_should_report_dry_run_without_prompting_or_writing() {
        let tmp = TempDir::new().expect("tempdir");
        let prompter = MockPrompter::approving();

        let envelope = run_with_prompter(&ctx(true), args(false), &prompter, true, tmp.path())
            .expect("dry run");

        let value = envelope_value(&envelope);
        assert_eq!(value["dry_run"], true);
        assert_eq!(value["data"]["approval"], "dry_run");
        assert_eq!(value["data"]["targets"][0]["status"], "would_install");
        assert_eq!(prompter.calls.get(), 0);
        assert!(!target_path(&tmp, ".claude/skills").exists());
        assert!(!target_path(&tmp, ".agents/skills").exists());
    }

    #[test]
    fn run_with_prompter_should_skip_install_when_prompt_is_declined() {
        let tmp = TempDir::new().expect("tempdir");
        let prompter = MockPrompter::declining();

        let envelope = run_with_prompter(&ctx(false), args(false), &prompter, true, tmp.path())
            .expect("cancelled");

        let value = envelope_value(&envelope);
        assert_eq!(value["ok"], true);
        assert_eq!(value["data"]["approval"], "declined");
        assert_eq!(value["data"]["targets"][1]["status"], "skipped");
        assert_eq!(prompter.calls.get(), 1);
        assert!(!target_path(&tmp, ".claude/skills").exists());
        assert!(!target_path(&tmp, ".agents/skills").exists());
    }

    #[test]
    fn run_with_prompter_should_install_bundled_files_into_both_skill_roots() {
        let tmp = TempDir::new().expect("tempdir");
        let prompter = MockPrompter::approving();

        let envelope = run_with_prompter(&ctx(false), args(true), &prompter, false, tmp.path())
            .expect("install");

        let value = envelope_value(&envelope);
        assert_eq!(value["data"]["approval"], "yes_flag");
        assert_eq!(value["data"]["targets"][0]["status"], "installed");
        assert_eq!(prompter.calls.get(), 0);

        for root in [".claude/skills", ".agents/skills"] {
            let skill_root = target_path(&tmp, root);
            for bundled in BUNDLED_FILES {
                let installed = fs::read_to_string(skill_file(&skill_root, bundled.relative_path))
                    .expect("installed bundled file");
                assert_eq!(installed, bundled.contents);
            }
        }
    }

    #[test]
    fn bundled_files_should_match_skill_tree_on_disk() {
        let skill_root =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../skills/taskfast-agent");

        let mut on_disk: Vec<String> = Vec::new();
        if skill_root.join("SKILL.md").is_file() {
            on_disk.push("SKILL.md".into());
        }
        let reference_dir = skill_root.join("reference");
        for entry in fs::read_dir(&reference_dir).expect("read skill reference dir") {
            let entry = entry.expect("skill reference dir entry");
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("md") {
                let name = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .expect("reference filename")
                    .to_string();
                on_disk.push(format!("reference/{name}"));
            }
        }
        on_disk.sort();

        let mut bundled: Vec<String> = BUNDLED_FILES
            .iter()
            .map(|file| file.relative_path.to_string())
            .collect();
        bundled.sort();

        assert_eq!(
            bundled, on_disk,
            "BUNDLED_FILES drifted from skills/taskfast-agent/ on disk — update \
             BUNDLED_FILES in crates/taskfast-cli/src/cmd/skills.rs to match"
        );
    }

    #[test]
    fn run_with_prompter_should_overwrite_bundled_files_and_preserve_unrelated_files() {
        let tmp = TempDir::new().expect("tempdir");
        let skill_root = target_path(&tmp, ".claude/skills");
        let bundled_skill = skill_root.join("SKILL.md");
        let local_note = skill_root.join("local-note.md");
        let sibling = tmp.path().join(".claude/skills/other-skill/SKILL.md");

        fs::create_dir_all(skill_root.join("reference")).expect("skill dir");
        fs::create_dir_all(sibling.parent().expect("sibling parent")).expect("sibling dir");
        fs::write(&bundled_skill, "stale").expect("seed bundled file");
        fs::write(&local_note, "keep").expect("seed local file");
        fs::write(&sibling, "other").expect("seed sibling file");

        run_with_prompter(
            &ctx(false),
            args(true),
            &MockPrompter::approving(),
            false,
            tmp.path(),
        )
        .expect("install");

        assert_eq!(
            fs::read_to_string(&bundled_skill).expect("updated bundled file"),
            BUNDLED_FILES[0].contents
        );
        assert_eq!(fs::read_to_string(&local_note).expect("local note"), "keep");
        assert_eq!(
            fs::read_to_string(&sibling).expect("sibling skill"),
            "other"
        );
    }
}
