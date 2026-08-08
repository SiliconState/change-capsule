//! `capsule`: a thin command-line adapter over the `change_capsule` crate.
//!
//! Each subcommand maps to exactly one library operation. With `--json`,
//! successes print one JSON value to stdout and failures print one JSON object
//! to stderr carrying a stable `kind` field for programmatic branching.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use change_capsule::{
    Author, CapsuleManager, CheckpointOptions, CloseOptions, CreateOptions, EvidenceInput,
    IntegrateOptions, Policy, VerifyOptions, verify_authenticated_bundle, verify_bundle,
};
use clap::{Args, Parser, Subcommand};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

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
    /// Show static machine-readable protocol capabilities without touching state or Git.
    Capabilities,
    /// Create an isolated capsule from a pinned Git commit.
    Create(CreateArgs),
    /// Directly resolve one state-root-scoped idempotency key.
    Lookup(LookupArgs),
    /// List durable capsule records.
    List(ListArgs),
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
    /// Generate a raw Ed25519 private seed and matching public key.
    Keygen(KeygenArgs),
    /// Sign exact bundle.json bytes with a raw 32-byte Ed25519 private seed.
    Sign(SignArgs),
    /// Verify an exported bundle offline, without capsule state or a workspace.
    Verify(VerifyArgs),
    /// Emit an in-toto Statement for a verified receipt, for SLSA/Sigstore tooling.
    Attest(AttestArgs),
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
    /// Reconcile interrupted lifecycle transitions globally or for one capsule.
    Recover(RecoverArgs),
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

    /// Opaque state-root-scoped idempotency key. Do not put secrets here.
    #[arg(long)]
    idempotency_key: Option<String>,
}

#[derive(Debug, Args)]
struct LookupArgs {
    /// Opaque state-root-scoped idempotency key. Do not put secrets here.
    #[arg(long)]
    idempotency_key: String,
}

#[derive(Debug, Args)]
struct ListArgs {
    /// Report unreadable records instead of failing on the first one.
    #[arg(long)]
    skip_invalid: bool,
}

#[derive(Debug, Args)]
struct IdArgs {
    id: String,
}

#[derive(Debug, Args)]
struct RecoverArgs {
    /// Capsule ID. Omit to recover all records.
    id: Option<String>,
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
struct KeygenArgs {
    /// New raw 32-byte Ed25519 private seed file.
    #[arg(long)]
    private_key: PathBuf,

    /// New raw 32-byte Ed25519 public key file.
    #[arg(long)]
    public_key: PathBuf,
}

#[derive(Debug, Args)]
struct SignArgs {
    /// Directory containing bundle.json.
    bundle: PathBuf,

    /// Raw 32-byte Ed25519 private seed. Never copied into Capsule state.
    #[arg(long)]
    private_key: PathBuf,

    /// New raw 64-byte detached signature file.
    #[arg(long)]
    output: PathBuf,
}

#[derive(Debug, Args)]
struct AttestArgs {
    /// Directory containing bundle.json, result.json, and result.patch.
    bundle: PathBuf,

    /// Also require the sealed patch to apply to its pinned base here.
    ///
    /// Without this, the statement still verifies the receipt's internal
    /// integrity, but nothing has re-derived the tree from the base.
    #[arg(long)]
    repo: Option<PathBuf>,

    /// Write the statement here instead of stdout. Never overwrites.
    #[arg(long)]
    output: Option<PathBuf>,

    /// Emit only the predicate body, for `cosign attest-blob --predicate`.
    #[arg(long)]
    predicate_only: bool,
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

    /// Reject bundles without successful evidence bound to the sealed patch.
    #[arg(long)]
    require_current_successful_evidence: bool,

    /// Raw 64-byte detached Ed25519 signature file.
    #[arg(long, requires = "trusted_public_key")]
    signature: Option<PathBuf>,

    /// Raw 32-byte trusted Ed25519 public key supplied out of band.
    #[arg(long, requires = "signature")]
    trusted_public_key: Option<PathBuf>,
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
    /// Validate or apply the explicit schema-v3 to schema-v4 migration.
    Migrate {
        /// Apply only after creating the required backup.
        #[arg(long, conflicts_with = "dry_run")]
        apply: bool,
        /// Validate and report without mutation (the default).
        #[arg(long, conflicts_with = "apply")]
        dry_run: bool,
        /// New external backup directory; required with --apply and invalid for dry-run.
        #[arg(long, required_if_eq("apply", "true"), requires = "apply")]
        backup: Option<PathBuf>,
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

    /// Refuse to seal unless successful evidence binds to the current complete patch.
    #[arg(long)]
    require_current_successful_evidence: bool,
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
        Self::new(value.author_name, value.author_email)
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
    if matches!(cli.command, Command::Capabilities) {
        return print_value(&change_capsule::Capabilities::current(), cli.json);
    }
    if let Command::Lookup(arguments) = &cli.command {
        let state_root = match &cli.home {
            Some(home) => home.clone(),
            None => change_capsule::default_state_root()?,
        };
        return print_value(
            &CapsuleManager::lookup_idempotency_key_at(&state_root, &arguments.idempotency_key)?,
            cli.json,
        );
    }
    if let Command::Keygen(arguments) = &cli.command {
        let keys = change_capsule::generate_keypair()?;
        write_new_key_file(&arguments.public_key, &keys.public_key(), false).map_err(|error| {
            change_capsule::Error::InvalidInput(format!(
                "public key publication failed at {}; no private key was published: {error}; remove the public key before retrying if it exists",
                arguments.public_key.display()
            ))
        })?;
        write_new_key_file(&arguments.private_key, keys.private_seed(), true).map_err(|error| {
            change_capsule::Error::InvalidInput(format!(
                "public key was published at {}; private key publication failed at {}: {error}; remove the public key before retrying if desired",
                arguments.public_key.display(),
                arguments.private_key.display()
            ))
        })?;
        return print_value(
            &json!({ "private_key": arguments.private_key, "public_key": arguments.public_key }),
            cli.json,
        );
    }
    if let Command::Sign(arguments) = &cli.command {
        let key = read_private_seed(&arguments.private_key)?;
        change_capsule::sign_bundle(&arguments.bundle, &key, &arguments.output)?;
        return print_value(
            &json!({ "bundle": arguments.bundle, "signature": arguments.output }),
            cli.json,
        );
    }
    if let Command::Attest(arguments) = &cli.command {
        let statement = change_capsule::attest_bundle(
            &arguments.bundle,
            &VerifyOptions::new(false, false, arguments.repo.clone()),
        )?;
        let mut rendered = if arguments.predicate_only {
            serde_json::to_vec_pretty(&statement.predicate)
        } else {
            serde_json::to_vec_pretty(&statement)
        }
        .map_err(|source| change_capsule::Error::Json {
            path: PathBuf::from("attestation"),
            source,
        })?;
        // One byte-identical document on both paths, so `--output` and a shell
        // redirection of stdout produce the same file.
        rendered.push(b'\n');
        if let Some(path) = &arguments.output {
            return write_new_file(path, &rendered);
        }
        print!("{}", String::from_utf8_lossy(&rendered));
        return Ok(());
    }
    if let Command::Verify(arguments) = &cli.command {
        let report = if let (Some(signature), Some(public_key)) =
            (&arguments.signature, &arguments.trusted_public_key)
        {
            let signature = read_exact_file::<64>(signature, "Ed25519 signature")?;
            let key = read_exact_file::<32>(public_key, "trusted Ed25519 public key")?;
            verify_authenticated_bundle(
                &arguments.bundle,
                &signature,
                &key,
                &VerifyOptions::new(
                    arguments.require_successful_evidence,
                    arguments.require_current_successful_evidence,
                    arguments.repo.clone(),
                ),
            )?
        } else {
            verify_bundle(
                &arguments.bundle,
                &VerifyOptions::new(
                    arguments.require_successful_evidence,
                    arguments.require_current_successful_evidence,
                    arguments.repo.clone(),
                ),
            )?
        };
        return print_value(&report, cli.json);
    }

    let manager = match cli.home {
        Some(home) => CapsuleManager::open(home)?,
        None => CapsuleManager::open_default()?,
    };

    match cli.command {
        Command::Capabilities => {
            unreachable!("capabilities is handled before state initialization")
        }
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
            let capsule = match arguments.idempotency_key {
                Some(key) => manager.create_idempotent(options, &key)?,
                None => manager.create(options)?,
            };
            if cli.json {
                print_json(&capsule)?;
            } else {
                println!("{}", capsule.id);
                println!("path={}", capsule.workspace_path.display());
                println!("base={}", capsule.base_commit);
            }
        }
        Command::Lookup(_) => {
            unreachable!("lookup is handled before state initialization")
        }
        Command::List(arguments) if arguments.skip_invalid => {
            let listing = manager.list_reporting()?;
            if cli.json {
                print_json(&listing)?;
            } else {
                for capsule in &listing.capsules {
                    println!("{} {:?} {}", capsule.id, capsule.state, capsule.base_commit);
                }
                for record in &listing.unreadable {
                    println!("{} UNREADABLE {}", record.id, record.error);
                }
            }
        }
        Command::List(_) => {
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
                        "patch_sha256": hex::encode(Sha256::digest(&patch)),
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
                    "patch_sha256": hex::encode(Sha256::digest(&patch)),
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
        Command::Keygen(_) | Command::Sign(_) | Command::Verify(_) | Command::Attest(_) => {
            unreachable!("keygen, sign, verify, and attest run before state initialization")
        }
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
            StateCommand::Migrate {
                apply,
                dry_run: _,
                backup,
            } => print_value(
                &manager.migrate_state_v3(backup.as_deref(), apply)?,
                cli.json,
            )?,
        },
        Command::Checkpoint(arguments) => {
            let checkpoint = manager.checkpoint(
                &arguments.id,
                CheckpointOptions::new(arguments.message, arguments.author.into()),
            )?;
            print_value(&checkpoint, cli.json)?;
        }
        Command::Evidence(arguments) => {
            let evidence = manager.add_evidence(&arguments.id, {
                let mut input = EvidenceInput::new(arguments.command, arguments.exit_code);
                input.summary = arguments.summary;
                input
            })?;
            print_value(&evidence, cli.json)?;
        }
        Command::Close(arguments) => {
            let result = manager.close(
                &arguments.id,
                CloseOptions::new(
                    arguments.require_successful_evidence,
                    arguments.require_current_successful_evidence,
                ),
            )?;
            print_value(&result, cli.json)?;
        }
        Command::Integrate(arguments) => {
            let capsule = manager.integrate(&arguments.id, &{
                let mut options = IntegrateOptions::new(arguments.target, arguments.author.into());
                options.message = arguments.message;
                options
            })?;
            print_value(&capsule, cli.json)?;
        }
        Command::Drop(arguments) => {
            let capsule = manager.drop_capsule(&arguments.id, arguments.force)?;
            print_value(&capsule, cli.json)?;
        }
        Command::Recover(arguments) => {
            let actions: Vec<_> = match arguments.id {
                Some(id) => manager.recover_capsule(&id)?.into_iter().collect(),
                None => manager.recover()?,
            };
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

fn parent_directory(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

/// Publish a new non-secret file without ever overwriting an existing path.
///
/// Attestations are inputs to signing and gating, so a silent overwrite could
/// swap the document a later `cosign` step signs.
fn write_new_file(path: &Path, bytes: &[u8]) -> change_capsule::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|source| change_capsule::Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|source| change_capsule::Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
    sync_parent_directory(parent_directory(path))
}

fn write_new_key_file(path: &Path, bytes: &[u8], private: bool) -> change_capsule::Result<()> {
    let parent = parent_directory(path);
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).map_err(|source| change_capsule::Error::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|source| change_capsule::Error::Io {
                path: temporary.path().to_path_buf(),
                source,
            })?;
    }
    let _ = private;
    temporary
        .write_all(bytes)
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|source| change_capsule::Error::Io {
            path: temporary.path().to_path_buf(),
            source,
        })?;
    temporary.persist_noclobber(path).map_err(|error| {
        if error.error.kind() == io::ErrorKind::AlreadyExists {
            change_capsule::Error::InvalidInput(format!(
                "refusing to overwrite key file: {}",
                path.display()
            ))
        } else {
            change_capsule::Error::Io {
                path: path.to_path_buf(),
                source: error.error,
            }
        }
    })?;
    sync_parent_directory(parent)
}

fn read_private_seed(path: &Path) -> change_capsule::Result<Zeroizing<[u8; 32]>> {
    Ok(Zeroizing::new(read_exact_file::<32>(
        path,
        "Ed25519 private seed",
    )?))
}

fn open_readonly_input(path: &Path) -> change_capsule::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let flags = rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK;
        options.custom_flags(
            i32::try_from(flags.bits())
                .expect("O_NOFOLLOW | O_NONBLOCK fits platform custom_flags"),
        );
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options
        .open(path)
        .map_err(|source| change_capsule::Error::Io {
            path: path.to_path_buf(),
            source,
        })
}

fn read_exact_file<const N: usize>(
    path: &Path,
    description: &str,
) -> change_capsule::Result<[u8; N]> {
    let mut file = open_readonly_input(path)?;
    validate_opened_exact_file(&file, path, description, N)?;
    let mut bytes = [0_u8; N];
    file.read_exact(&mut bytes)
        .map_err(|source| change_capsule::Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
    let mut trailing = [0_u8; 1];
    if file
        .read(&mut trailing)
        .map_err(|source| change_capsule::Error::Io {
            path: path.to_path_buf(),
            source,
        })?
        != 0
    {
        return Err(invalid_exact_file(path, description, N));
    }
    validate_opened_exact_file(&file, path, description, N)?;
    Ok(bytes)
}

fn validate_opened_exact_file(
    file: &fs::File,
    path: &Path,
    description: &str,
    size: usize,
) -> change_capsule::Result<()> {
    let metadata = file
        .metadata()
        .map_err(|source| change_capsule::Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
    if metadata_is_reparse_point(&metadata) || !metadata.is_file() || metadata.len() != size as u64
    {
        return Err(invalid_exact_file(path, description, size));
    }
    Ok(())
}

fn metadata_is_reparse_point(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
    }
    #[cfg(not(windows))]
    {
        let _ = metadata;
        false
    }
}

fn invalid_exact_file(path: &Path, description: &str, size: usize) -> change_capsule::Error {
    change_capsule::Error::InvalidInput(format!(
        "{description} must be a regular non-link file containing exactly {size} raw bytes: {}",
        path.display()
    ))
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> change_capsule::Result<()> {
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| change_capsule::Error::Io {
            path: path.to_path_buf(),
            source,
        })
}

/// Windows offers no portable directory-sync equivalent, so publication relies
/// on the file sync plus the atomic rename already performed by the caller.
#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
fn sync_parent_directory(_path: &Path) -> change_capsule::Result<()> {
    Ok(())
}

fn read_json_file<T: serde::de::DeserializeOwned>(
    path: &Path,
    cap: u64,
) -> change_capsule::Result<T> {
    let mut file = open_readonly_input(path)?;
    let metadata = file
        .metadata()
        .map_err(|source| change_capsule::Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
    if metadata_is_reparse_point(&metadata) || !metadata.is_file() || metadata.len() > cap {
        return Err(change_capsule::Error::InvalidInput(format!(
            "JSON input must be a regular non-link file no larger than {cap} bytes: {}",
            path.display()
        )));
    }
    let mut bytes = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take(cap + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| change_capsule::Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() as u64 > cap {
        return Err(change_capsule::Error::InvalidInput(format!(
            "JSON input exceeds {cap} bytes: {}",
            path.display()
        )));
    }
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
    let parent = parent_directory(path);
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

/// Map a library error to its stable CLI `kind`.
///
/// `Error` is `#[non_exhaustive]`, so this cannot be a compiler-checked
/// exhaustive match. Every new variant MUST be given an explicit arm above the
/// fallback; landing in the fallback is a bug, not a design.
#[allow(clippy::match_same_arms)]
fn error_kind(error: &change_capsule::Error) -> &'static str {
    match error {
        change_capsule::Error::NotRepository(_) => "not_repository",
        change_capsule::Error::NotFound(_) => "not_found",
        change_capsule::Error::InvalidId(_) | change_capsule::Error::InvalidInput(_) => {
            "invalid_input"
        }
        change_capsule::Error::IdempotencyConflict => "idempotency_conflict",
        change_capsule::Error::IdempotencyNotFound => "idempotency_not_found",
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
        _ => "internal",
    }
}
