//! `capsule`: a thin command-line adapter over the `change_capsule` crate.
//!
//! Each subcommand maps to exactly one library operation. With `--json`,
//! successes print one JSON value to stdout and failures print one JSON object
//! to stderr carrying a stable `kind` field for programmatic branching.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use change_capsule::{
    Author, CapsuleManager, CheckpointOptions, CloseOptions, CreateOptions, EvidenceInput,
    IntegrateOptions, Policy, VerifyOptions, verify_bundle,
};
use clap::{Args, Parser, Subcommand};
use serde::Serialize;
use serde_json::json;

#[derive(Debug, Parser)]
#[command(
    name = "capsule",
    version,
    about = "Create recoverable, agent-neutral change attempts backed by Git worktrees"
)]
struct Cli {
    /// Override the owner-private state directory.
    #[arg(long, global = true, env = "CAPSULE_HOME")]
    home: Option<PathBuf>,

    /// Emit machine-readable JSON. Diff emits metadata; use --output for patch bytes.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create an isolated capsule from a pinned Git commit.
    Create(CreateArgs),
    /// List durable capsule records.
    List,
    /// Show one capsule manifest.
    Show(IdArgs),
    /// Print the capsule workspace path.
    Path(IdArgs),
    /// Inspect worktree health, changes, commits, and seal state.
    Status(IdArgs),
    /// Render the complete binary-capable patch from the pinned base.
    Diff(DiffArgs),
    /// Show the sealed result manifest.
    Result(IdArgs),
    /// Discover sealed artifacts with media types, file URIs, sizes, and digests.
    Artifacts(IdArgs),
    /// Export a self-describing sealed artifact directory.
    Export(ExportArgs),
    /// Verify an exported bundle offline, without capsule state or a workspace.
    Verify(VerifyArgs),
    /// Show structured lifecycle audit events for one capsule or all capsules.
    Audit(AuditArgs),
    /// Show an aggregate observability snapshot.
    Metrics,
    /// Read, replace, or evaluate resource and repository policy.
    Policy(PolicyArgs),
    /// Inspect or back up durable state.
    State(StateArgs),
    /// Commit the capsule's current changes as a durable checkpoint.
    Checkpoint(CheckpointArgs),
    /// Attach externally-run verification evidence to an active capsule.
    Evidence(EvidenceArgs),
    /// Seal an active capsule into an immutable result manifest and patch.
    Close(CloseArgs),
    /// Apply a sealed result to a clean worktree still at the pinned base.
    Integrate(IntegrateArgs),
    /// Remove the owned worktree while retaining its durable record and result.
    Drop(DropArgs),
    /// Reconcile interrupted create and integrate journal states.
    Recover,
}

#[derive(Debug, Args)]
struct CreateArgs {
    /// Any path inside the source Git repository.
    #[arg(long, default_value = ".")]
    repo: PathBuf,

    /// Revision to pin as the capsule base.
    #[arg(long, default_value = "HEAD")]
    base: String,

    /// Human-facing attempt label.
    #[arg(long)]
    label: Option<String>,

    /// Opaque linkage metadata, such as issue=bd-42 or run=abc. Repeatable.
    #[arg(long, value_parser = parse_link)]
    link: Vec<(String, String)>,
}

#[derive(Debug, Args)]
struct IdArgs {
    id: String,
}

#[derive(Debug, Args)]
struct DiffArgs {
    id: String,

    /// Write patch bytes to a file instead of standard output.
    #[arg(long)]
    output: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct AuditArgs {
    /// Capsule ID. Omit to read the administrative event stream.
    id: Option<String>,
}

#[derive(Debug, Args)]
struct ExportArgs {
    id: String,

    /// New directory that will receive bundle.json, result.json, and result.patch.
    #[arg(long)]
    output: PathBuf,
}

#[derive(Debug, Args)]
struct VerifyArgs {
    /// Directory containing bundle.json, result.json, and result.patch.
    bundle: PathBuf,

    /// Also confirm the pinned base exists here and the sealed patch applies to it.
    #[arg(long)]
    repo: Option<PathBuf>,

    /// Reject bundles without evidence or with any non-zero evidence exit code.
    #[arg(long)]
    require_successful_evidence: bool,
}

#[derive(Debug, Args)]
struct PolicyArgs {
    #[command(subcommand)]
    command: PolicyCommand,
}

#[derive(Debug, Subcommand)]
enum PolicyCommand {
    /// Show the effective policy; absent policy.json means permissive defaults.
    Show,
    /// Evaluate all current records and workspaces against the effective policy.
    Check,
    /// Replace policy.json from a versioned JSON document.
    Set {
        #[arg(long)]
        file: PathBuf,
    },
}

#[derive(Debug, Args)]
struct StateArgs {
    #[command(subcommand)]
    command: StateCommand,
}

#[derive(Debug, Subcommand)]
enum StateCommand {
    /// Inspect records without requiring their schema to be supported.
    Inspect,
    /// Copy durable manifests, results, patches, and policy to a new directory.
    Backup {
        #[arg(long)]
        output: PathBuf,
    },
}

#[derive(Debug, Args)]
struct CheckpointArgs {
    id: String,

    #[arg(short = 'm', long)]
    message: String,

    #[command(flatten)]
    author: AuthorArgs,
}

#[derive(Debug, Args)]
struct EvidenceArgs {
    id: String,

    /// Exact verification command run by the caller.
    #[arg(long)]
    command: String,

    /// Exit code observed by the caller.
    #[arg(long)]
    exit_code: i32,

    /// Bounded human- or machine-generated result summary.
    #[arg(long)]
    summary: Option<String>,
}

#[derive(Debug, Args)]
struct CloseArgs {
    id: String,

    /// Refuse to seal unless evidence exists and every recorded exit code is zero.
    #[arg(long)]
    require_successful_evidence: bool,
}

#[derive(Debug, Args)]
struct IntegrateArgs {
    id: String,

    /// Any path inside the destination worktree.
    #[arg(long, default_value = ".")]
    target: PathBuf,

    /// Commit subject. Defaults to the capsule label or a generated subject.
    #[arg(short = 'm', long)]
    message: Option<String>,

    #[command(flatten)]
    author: AuthorArgs,
}

#[derive(Debug, Args)]
struct DropArgs {
    id: String,

    /// Permit cleanup of an active or interrupted capsule. Foreign paths remain protected.
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Args)]
struct AuthorArgs {
    #[arg(long, default_value = "Capsule")]
    author_name: String,

    #[arg(long, default_value = "capsule@localhost")]
    author_email: String,
}

impl From<AuthorArgs> for Author {
    fn from(value: AuthorArgs) -> Self {
        Self {
            name: value.author_name,
            email: value.author_email,
        }
    }
}

fn main() -> ExitCode {
    let arguments: Vec<_> = std::env::args_os().collect();
    let json_mode = arguments.iter().any(|argument| argument == "--json");
    let cli = match Cli::try_parse_from(&arguments) {
        Ok(cli) => cli,
        Err(error) => {
            let exit_code = u8::try_from(error.exit_code()).unwrap_or(2);
            if exit_code == 0 {
                if json_mode {
                    let payload = json!({
                        "ok": true,
                        "kind": "cli_help",
                        "output": error.to_string(),
                    });
                    println!(
                        "{}",
                        serde_json::to_string(&payload).unwrap_or_else(|_| {
                            "{\"ok\":false,\"error\":\"failed to serialize CLI help\",\"kind\":\"internal\"}"
                                .to_owned()
                        })
                    );
                } else {
                    let _ = error.print();
                }
            } else if json_mode {
                let payload = json!({
                    "ok": false,
                    "error": error.to_string(),
                    "kind": "cli",
                });
                eprintln!(
                    "{}",
                    serde_json::to_string(&payload).unwrap_or_else(|_| {
                        "{\"ok\":false,\"error\":\"failed to serialize CLI error\",\"kind\":\"internal\"}"
                            .to_owned()
                    })
                );
            } else {
                let _ = error.print();
            }
            return ExitCode::from(exit_code);
        }
    };
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if json_mode {
                let payload = json!({
                    "ok": false,
                    "error": error.to_string(),
                    "kind": error_kind(&error),
                });
                eprintln!(
                    "{}",
                    serde_json::to_string(&payload).unwrap_or_else(|_| {
                        "{\"ok\":false,\"error\":\"failed to serialize error\"}".to_owned()
                    })
                );
            } else {
                eprintln!("error: {error}");
            }
            ExitCode::FAILURE
        }
    }
}

// CLI dispatch is intentionally explicit: each public subcommand maps directly to one
// library operation, keeping automation behavior auditable in one place.
#[allow(clippy::too_many_lines)]
fn run(cli: Cli) -> change_capsule::Result<()> {
    if let Command::Verify(arguments) = &cli.command {
        let report = verify_bundle(
            &arguments.bundle,
            &VerifyOptions {
                require_successful_evidence: arguments.require_successful_evidence,
                repository: arguments.repo.clone(),
            },
        )?;
        return print_value(&report, cli.json);
    }

    let manager = match cli.home {
        Some(home) => CapsuleManager::open(home)?,
        None => CapsuleManager::open_default()?,
    };

    match cli.command {
        Command::Create(arguments) => {
            let mut options = CreateOptions::new(arguments.repo);
            options.base = arguments.base;
            options.label = arguments.label;
            let mut links = BTreeMap::new();
            for (key, value) in arguments.link {
                if links.insert(key.clone(), value).is_some() {
                    return Err(change_capsule::Error::InvalidInput(format!(
                        "duplicate link key: {key:?}"
                    )));
                }
            }
            options.links = links;
            let capsule = manager.create(options)?;
            if cli.json {
                print_json(&capsule)?;
            } else {
                println!("{}", capsule.id);
                println!("path={}", capsule.workspace_path.display());
                println!("base={}", capsule.base_commit);
            }
        }
        Command::List => {
            let capsules = manager.list()?;
            if cli.json {
                print_json(&capsules)?;
            } else if capsules.is_empty() {
                println!("No capsules.");
            } else {
                for capsule in capsules {
                    println!(
                        "{}\t{:?}\t{}\t{}",
                        capsule.id,
                        capsule.state,
                        capsule.label.as_deref().unwrap_or("-"),
                        capsule.workspace_path.display()
                    );
                }
            }
        }
        Command::Show(arguments) => print_value(&manager.show(&arguments.id)?, cli.json)?,
        Command::Path(arguments) => {
            let path = manager.workspace_path(&arguments.id)?;
            if cli.json {
                print_json(&json!({ "id": arguments.id, "path": path }))?;
            } else {
                println!("{}", path.display());
            }
        }
        Command::Status(arguments) => print_value(&manager.status(&arguments.id)?, cli.json)?,
        Command::Diff(arguments) => {
            let patch = manager.diff(&arguments.id)?;
            if let Some(output) = arguments.output {
                write_output_file(&output, &patch)?;
                if cli.json {
                    print_json(&json!({
                        "id": arguments.id,
                        "output": output,
                        "bytes": patch.len(),
                    }))?;
                } else {
                    println!("{}", output.display());
                }
            } else if cli.json {
                let changed_paths = match manager.result(&arguments.id) {
                    Ok(result) => result.changed_paths,
                    Err(change_capsule::Error::InvalidState { .. }) => {
                        manager.status(&arguments.id)?.changed_paths
                    }
                    Err(error) => return Err(error),
                };
                print_json(&json!({
                    "id": arguments.id,
                    "bytes": patch.len(),
                    "changed_paths": changed_paths,
                    "hint": "pass --output <path> to write patch bytes",
                }))?;
            } else {
                io::stdout().lock().write_all(&patch).map_err(|source| {
                    change_capsule::Error::Io {
                        path: PathBuf::from("stdout"),
                        source,
                    }
                })?;
            }
        }
        Command::Result(arguments) => {
            let result = manager.result(&arguments.id)?;
            if cli.json {
                print_json(&result)?;
            } else {
                println!("kind={:?}", result.kind);
                println!("base={}", result.base_commit);
                println!("head={}", result.head_commit);
                println!(
                    "patch={}",
                    manager.result_patch_path(&arguments.id)?.display()
                );
                for path in result.changed_paths {
                    println!("changed={path}");
                }
            }
        }
        Command::Artifacts(arguments) => {
            print_value(&manager.artifacts(&arguments.id)?, cli.json)?;
        }
        Command::Export(arguments) => {
            print_value(
                &manager.export_artifacts(&arguments.id, arguments.output)?,
                cli.json,
            )?;
        }
        Command::Verify(_) => unreachable!("verify is handled before state initialization"),
        Command::Audit(arguments) => {
            let events = match arguments.id {
                Some(id) => manager.audit_events(&id)?,
                None => manager.audit_log()?,
            };
            print_value(&events, cli.json)?;
        }
        Command::Metrics => print_value(&manager.metrics()?, cli.json)?,
        Command::Policy(arguments) => match arguments.command {
            PolicyCommand::Show => print_value(&manager.policy()?, cli.json)?,
            PolicyCommand::Check => print_value(&manager.policy_report()?, cli.json)?,
            PolicyCommand::Set { file } => {
                let policy: Policy = read_json_file(&file, 64 * 1024)?;
                print_value(&manager.set_policy(policy)?, cli.json)?;
            }
        },
        Command::State(arguments) => match arguments.command {
            StateCommand::Inspect => print_value(&manager.inspect_state()?, cli.json)?,
            StateCommand::Backup { output } => {
                print_value(&manager.backup_state(output)?, cli.json)?;
            }
        },
        Command::Checkpoint(arguments) => {
            let checkpoint = manager.checkpoint(
                &arguments.id,
                CheckpointOptions {
                    message: arguments.message,
                    author: arguments.author.into(),
                },
            )?;
            print_value(&checkpoint, cli.json)?;
        }
        Command::Evidence(arguments) => {
            let evidence = manager.add_evidence(
                &arguments.id,
                EvidenceInput {
                    command: arguments.command,
                    exit_code: arguments.exit_code,
                    summary: arguments.summary,
                },
            )?;
            print_value(&evidence, cli.json)?;
        }
        Command::Close(arguments) => {
            let result = manager.close(
                &arguments.id,
                CloseOptions {
                    require_successful_evidence: arguments.require_successful_evidence,
                },
            )?;
            print_value(&result, cli.json)?;
        }
        Command::Integrate(arguments) => {
            let capsule = manager.integrate(
                &arguments.id,
                &IntegrateOptions {
                    target: arguments.target,
                    message: arguments.message,
                    author: arguments.author.into(),
                },
            )?;
            print_value(&capsule, cli.json)?;
        }
        Command::Drop(arguments) => {
            let capsule = manager.drop_capsule(&arguments.id, arguments.force)?;
            print_value(&capsule, cli.json)?;
        }
        Command::Recover => {
            let actions = manager.recover()?;
            if cli.json {
                print_json(&actions)?;
            } else if actions.is_empty() {
                println!("No recovery actions required.");
            } else {
                for action in actions {
                    println!("{}\t{}", action.capsule_id, action.action);
                }
            }
        }
    }
    Ok(())
}

fn parse_link(value: &str) -> Result<(String, String), String> {
    let (key, value) = value
        .split_once('=')
        .ok_or_else(|| "links must use KEY=VALUE".to_owned())?;
    Ok((key.to_owned(), value.to_owned()))
}

fn read_json_file<T: serde::de::DeserializeOwned>(
    path: &Path,
    cap: u64,
) -> change_capsule::Result<T> {
    let metadata = fs::symlink_metadata(path).map_err(|source| change_capsule::Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > cap {
        return Err(change_capsule::Error::InvalidInput(format!(
            "JSON input must be a regular non-symlink file no larger than {cap} bytes: {}",
            path.display()
        )));
    }
    let bytes = fs::read(path).map_err(|source| change_capsule::Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_slice(&bytes).map_err(|source| {
        change_capsule::Error::InvalidInput(format!(
            "invalid JSON input at {}: {source}",
            path.display()
        ))
    })
}

fn print_value<T: Serialize + std::fmt::Debug>(
    value: &T,
    json_mode: bool,
) -> change_capsule::Result<()> {
    if json_mode {
        print_json(value)
    } else {
        println!("{value:#?}");
        Ok(())
    }
}

fn print_json<T: Serialize>(value: &T) -> change_capsule::Result<()> {
    let rendered =
        serde_json::to_string_pretty(value).map_err(|source| change_capsule::Error::Json {
            path: PathBuf::from("stdout"),
            source,
        })?;
    println!("{rendered}");
    Ok(())
}

fn write_output_file(path: &Path, bytes: &[u8]) -> change_capsule::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| change_capsule::Error::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).map_err(|source| change_capsule::Error::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    temporary
        .write_all(bytes)
        .map_err(|source| change_capsule::Error::Io {
            path: temporary.path().to_path_buf(),
            source,
        })?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|source| change_capsule::Error::Io {
            path: temporary.path().to_path_buf(),
            source,
        })?;
    temporary.persist_noclobber(path).map_err(|error| {
        if error.error.kind() == io::ErrorKind::AlreadyExists {
            change_capsule::Error::InvalidInput(format!(
                "refusing to overwrite output file: {}",
                path.display()
            ))
        } else {
            change_capsule::Error::Io {
                path: path.to_path_buf(),
                source: error.error,
            }
        }
    })?;
    Ok(())
}

fn error_kind(error: &change_capsule::Error) -> &'static str {
    match error {
        change_capsule::Error::NotRepository(_) => "not_repository",
        change_capsule::Error::NotFound(_) => "not_found",
        change_capsule::Error::InvalidId(_) | change_capsule::Error::InvalidInput(_) => {
            "invalid_input"
        }
        change_capsule::Error::InvalidState { .. } => "invalid_state",
        change_capsule::Error::PolicyViolation(_) => "policy",
        change_capsule::Error::ArtifactNotFound(_) => "artifact_not_found",
        change_capsule::Error::Verification(_) => "verification",
        change_capsule::Error::UnsafeState(_) | change_capsule::Error::ForeignWorktree(_) => {
            "safety"
        }
        change_capsule::Error::UnsealedChanges(_) | change_capsule::Error::ResultDrift(_) => {
            "unsealed_result"
        }
        change_capsule::Error::DirtyIntegrationTarget(_) => "dirty_target",
        change_capsule::Error::Git { .. } | change_capsule::Error::GitOutputTooLarge { .. } => {
            "git"
        }
        change_capsule::Error::NonUtf8Path(_) => "unsupported_path",
        change_capsule::Error::SchemaVersion { .. } => "schema_version",
        change_capsule::Error::Io { .. }
        | change_capsule::Error::Json { .. }
        | change_capsule::Error::CaptureWorker => "internal",
    }
}
