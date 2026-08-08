use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use ulid::Ulid;

use crate::artifact::{ArtifactReader, ArtifactSink, PublishedArtifact};
use crate::capabilities::{
    IDEMPOTENCY_RECORD_SCHEMA_VERSION, LABEL_BYTES_LIMIT, LINK_KEY_BYTES_LIMIT,
    LINK_VALUE_BYTES_LIMIT, LINKS_LIMIT,
};
use crate::error::{Error, Result, io};
use crate::git::{CommitPatch, Git, Repository};
use crate::idempotency::{
    IdempotencyLookup, IdempotencyRecord, IdempotencyStatus, canonical_request_sha256, key_sha256,
};
use crate::model::{
    ArtifactBundle, ArtifactDescriptor, ArtifactKind, BUNDLE_SCHEMA_VERSION, Capsule,
    CapsuleHealth, CapsuleListing, CapsuleResult, CapsuleState, CapsuleStatus, CapsuleSummary,
    Checkpoint, CheckpointJournal, Cleanup, Evidence, ExportReport, Integration, RecoveryAction,
    ResultKind, ResultRef, SCHEMA_VERSION,
};
use crate::state::{StateStore, default_state_root, project_key};

const LABEL_CAP: usize = LABEL_BYTES_LIMIT;
const LINK_KEY_CAP: usize = LINK_KEY_BYTES_LIMIT;
const LINK_VALUE_CAP: usize = LINK_VALUE_BYTES_LIMIT;
const MESSAGE_CAP: usize = 16 * 1024;
const EVIDENCE_COMMAND_CAP: usize = 16 * 1024;
const EVIDENCE_SUMMARY_CAP: usize = 64 * 1024;
const EVIDENCE_COUNT_CAP: usize = 64;
const EVIDENCE_TOTAL_BYTES_CAP: usize = 256 * 1024;
const CHECKPOINT_COUNT_CAP: usize = 128;
const DEFAULT_AUTHOR_NAME: &str = "Capsule";
const DEFAULT_AUTHOR_EMAIL: &str = "capsule@localhost";

/// Commit identity used for checkpoints and integration commits.
///
/// Always explicit: this crate never reads the ambient Git configuration to
/// decide who authored a change it creates.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Author {
    /// Name recorded as both author and committer.
    pub name: String,
    /// Email recorded as both author and committer. Must contain `@`.
    pub email: String,
}

impl Author {
    /// Identity to record as both author and committer.
    ///
    /// The email must contain `@`; it is validated at the operation boundary.
    pub fn new(name: impl Into<String>, email: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            email: email.into(),
        }
    }
}

impl Default for Author {
    fn default() -> Self {
        Self {
            name: DEFAULT_AUTHOR_NAME.to_owned(),
            email: DEFAULT_AUTHOR_EMAIL.to_owned(),
        }
    }
}

/// Inputs for creating a capsule.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct CreateOptions {
    /// Any path inside the source Git repository.
    pub repository: PathBuf,
    /// Revision to pin, resolved to an immutable commit before work starts.
    pub base: String,
    /// Optional human-facing description of the attempt.
    pub label: Option<String>,
    /// Opaque caller metadata, at most 32 entries. No key is privileged.
    pub links: BTreeMap<String, String>,
}

impl CreateOptions {
    /// Options for a capsule based on the repository's current `HEAD`.
    pub fn new(repository: impl Into<PathBuf>) -> Self {
        Self {
            repository: repository.into(),
            base: "HEAD".to_owned(),
            label: None,
            links: BTreeMap::new(),
        }
    }

    /// Pin a different revision as the base.
    #[must_use]
    pub fn with_base(mut self, base: impl Into<String>) -> Self {
        self.base = base.into();
        self
    }

    /// Set the human-facing attempt label.
    #[must_use]
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Replace the opaque caller links.
    #[must_use]
    pub fn with_links(mut self, links: BTreeMap<String, String>) -> Self {
        self.links = links;
        self
    }
}

/// Inputs for committing the workspace's current state as a checkpoint.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct CheckpointOptions {
    /// Message for the checkpoint commit.
    pub message: String,
    /// Identity to record on the commit.
    pub author: Author,
}

impl CheckpointOptions {
    /// Options for a checkpoint commit with an explicit identity.
    pub fn new(message: impl Into<String>, author: Author) -> Self {
        Self {
            message: message.into(),
            author,
        }
    }
}

/// How a verification record is produced.
///
/// The two variants are deliberately separate types rather than a flag, because
/// everything a verifier can conclude from the resulting [`Evidence`] depends on
/// which one was used.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum EvidenceInput {
    /// Run this argument vector in the capsule workspace and record what happened.
    ///
    /// Capsule spawns the program directly, with no shell, and observes the exit
    /// status and output itself. The resulting record has `executed: true`.
    Run {
        /// Program and arguments. The first element is the program.
        argv: Vec<String>,
        /// Optional summary. Defaults to the tail of the captured output.
        summary: Option<String>,
        /// Kill the command and fail if it runs longer than this.
        timeout: Option<std::time::Duration>,
    },
    /// Record a caller-supplied claim. Capsule executes nothing and vouches for nothing.
    Claim {
        /// Exact command line the caller reports having run.
        command: String,
        /// Exit status the caller reports having observed.
        exit_code: i32,
        /// Optional bounded summary of what happened.
        summary: Option<String>,
    },
}

impl EvidenceInput {
    /// Have Capsule execute `argv` in the capsule workspace.
    ///
    /// This produces the only evidence a verifier can treat as fact.
    pub fn run<I, S>(argv: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::Run {
            argv: argv.into_iter().map(Into::into).collect(),
            summary: None,
            timeout: None,
        }
    }

    /// Record that `command` was run elsewhere and exited with `exit_code`.
    ///
    /// Capsule records the claim and never executes it.
    pub fn claim(command: impl Into<String>, exit_code: i32) -> Self {
        Self::Claim {
            command: command.into(),
            exit_code,
            summary: None,
        }
    }

    /// Attach a bounded summary.
    #[must_use]
    pub fn with_summary(mut self, text: impl Into<String>) -> Self {
        match &mut self {
            Self::Run { summary, .. } | Self::Claim { summary, .. } => {
                *summary = Some(text.into());
            }
        }
        self
    }

    /// Kill an executed command that runs longer than this. Ignored for a claim.
    #[must_use]
    pub fn with_timeout(mut self, limit: std::time::Duration) -> Self {
        if let Self::Run { timeout, .. } = &mut self {
            *timeout = Some(limit);
        }
        self
    }

    /// The command line this record will carry.
    ///
    /// For an executed run this renders `argv` with POSIX quoting, so an
    /// argument containing spaces stays one argument to anyone reading the
    /// receipt. It is a faithful rendering of what ran, not a string that was
    /// ever handed to a shell: Capsule spawns the program directly.
    pub fn command_line(&self) -> String {
        match self {
            Self::Run { argv, .. } => argv
                .iter()
                .map(|argument| render_argument(argument))
                .collect::<Vec<_>>()
                .join(" "),
            Self::Claim { command, .. } => command.clone(),
        }
    }
}

/// Render one argument so a reader can tell where it starts and ends.
fn render_argument(argument: &str) -> String {
    let plain = !argument.is_empty()
        && argument.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'-' | b'_' | b'.' | b'/' | b'=' | b':' | b'+' | b'@' | b','
                )
        });
    if plain {
        argument.to_owned()
    } else {
        format!("'{}'", argument.replace('\'', r"'\''"))
    }
}

/// Requirements a capsule must satisfy before its result is sealed.
///
/// These are independent, not a ladder. In particular
/// [`Self::require_successful_evidence`] asks something the other two do not:
/// that **no** record anywhere on the capsule failed. That deliberately forbids
/// the ordinary agent loop of running tests, fixing what broke, and running them
/// again, so combine it with the others only when you really mean it.
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct CloseOptions {
    /// Refuse to seal unless evidence exists and *every* record's exit code is zero.
    ///
    /// A single earlier failure blocks sealing, even if the capsule has since
    /// been fixed and re-verified.
    pub require_successful_evidence: bool,
    /// Refuse to seal unless some successful record is bound to the patch being sealed.
    pub require_current_successful_evidence: bool,
    /// Refuse to seal unless Capsule itself ran a command that passed on this patch.
    ///
    /// This is the strongest useful requirement, and it subsumes the one above:
    /// the record it looks for is executed, passing, and bound to this patch.
    pub require_executed_evidence: bool,
}

impl CloseOptions {
    /// Seal with an explicit combination of requirements.
    #[must_use]
    pub fn requiring(
        require_successful_evidence: bool,
        require_current_successful_evidence: bool,
        require_executed_evidence: bool,
    ) -> Self {
        Self {
            require_successful_evidence,
            require_current_successful_evidence,
            require_executed_evidence,
        }
    }

    /// Seal only when Capsule ran a passing command against the exact sealed patch.
    ///
    /// This is the level a merge gate should use. It does not also demand a
    /// spotless history: an attempt whose tests failed and were then fixed
    /// still seals, because what matters is the state of the patch being sealed.
    #[must_use]
    pub fn executed() -> Self {
        Self {
            require_successful_evidence: false,
            require_current_successful_evidence: false,
            require_executed_evidence: true,
        }
    }
}

/// Inputs for applying a sealed result to a target worktree.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct IntegrateOptions {
    /// Any path inside the destination worktree.
    ///
    /// It must belong to the same repository, be clean, and still be at the
    /// capsule's pinned base.
    pub target: PathBuf,
    /// Commit subject. Defaults to the capsule label, then a generated subject.
    pub message: Option<String>,
    /// Identity to record on the integration commit.
    pub author: Author,
}

impl IntegrateOptions {
    /// Options for applying a sealed result to `target`.
    ///
    /// Set [`Self::message`] to override the generated commit subject.
    pub fn new(target: impl Into<PathBuf>, author: Author) -> Self {
        Self {
            target: target.into(),
            message: None,
            author,
        }
    }

    /// Override the generated commit subject.
    #[must_use]
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }
}

/// Entry point for every capsule operation.
///
/// A manager owns one state directory and resolves the Git executable once when
/// opened. Instances are cheap; operations serialize across processes through
/// file locks, so several managers may safely share a state root.
///
/// # Example
///
/// ```no_run
/// use change_capsule::{CapsuleManager, CreateOptions};
///
/// let manager = CapsuleManager::open_default()?;
/// let capsule = manager.create(CreateOptions::new("."))?;
/// println!("work in {}", capsule.workspace_path.display());
/// # Ok::<(), change_capsule::Error>(())
/// ```
#[derive(Debug)]
#[non_exhaustive]
pub struct CapsuleManager {
    store: StateStore,
    git: Git,
}

mod artifacts;
mod create;
mod lifecycle;
mod query;
mod recover;

impl CapsuleManager {
    /// Open a manager on the default state directory.
    ///
    /// Honours `CAPSULE_HOME`, then the platform state directory.
    pub fn open_default() -> Result<Self> {
        Self::open(default_state_root()?)
    }

    /// Open a manager on an explicit state directory, creating it if needed.
    pub fn open(state_root: impl Into<PathBuf>) -> Result<Self> {
        let state_root = state_root.into();
        Ok(Self {
            store: StateStore::open(&state_root)?,
            git: Git::discover()?,
        })
    }

    /// Directory holding this manager's durable state.
    pub fn state_root(&self) -> &Path {
        self.store.root()
    }
}

fn artifact_descriptor(
    kind: ArtifactKind,
    name: &str,
    media_type: &str,
    path: &Path,
    digest: &str,
    bytes: u64,
) -> Result<ArtifactDescriptor> {
    Ok(ArtifactDescriptor {
        kind,
        name: name.to_owned(),
        media_type: media_type.to_owned(),
        uri: file_uri(path)?,
        content_address: format!("sha256:{digest}"),
        sha256: digest.to_owned(),
        bytes,
    })
}

fn file_uri(path: &Path) -> Result<String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| io(".", error))?
            .join(path)
    };
    let text = absolute
        .to_str()
        .ok_or_else(|| Error::NonUtf8Path(absolute.clone()))?;
    #[cfg(windows)]
    let (prefix, normalized) = {
        let normalized = text.replace('\\', "/");
        if normalized.starts_with("//") {
            ("file:", normalized)
        } else {
            ("file:///", normalized)
        }
    };
    #[cfg(not(windows))]
    let (prefix, normalized) = ("file://", text.to_owned());
    let mut encoded = String::with_capacity(normalized.len() + prefix.len());
    encoded.push_str(prefix);
    for byte in normalized.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b':' | b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            write!(encoded, "%{byte:02X}")
                .map_err(|_| Error::UnsafeState("failed to encode artifact URI".to_owned()))?;
        }
    }
    Ok(encoded)
}

fn safe_ignored_relative(ignored: &crate::model::GitPath) -> Result<PathBuf> {
    let relative = ignored.to_path_buf().ok_or_else(|| {
        Error::InvalidInput(format!(
            "ignored path cannot be represented here: {ignored}"
        ))
    })?;
    if relative.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(Error::UnsafeState(format!(
            "Git returned an unsafe ignored path: {ignored}"
        )));
    }
    Ok(relative)
}

#[derive(Debug)]
struct CloseSnapshotTransaction {
    clean: bool,
    snapshot: crate::git::Snapshot,
    head: String,
    ignored: IgnoredContentInventory,
}

#[derive(Debug, PartialEq, Eq)]
struct IgnoredContentInventory {
    paths: Vec<crate::model::GitPath>,
    bytes: u64,
    content_sha256: String,
}

fn ignored_content_inventory(
    workspace: &Path,
    ignored_paths: Vec<crate::model::GitPath>,
) -> Result<IgnoredContentInventory> {
    let mut digest = Sha256::new();
    digest.update(b"change-capsule ignored-content inventory v2\0");
    let mut total = 0_u64;
    let mut seen = BTreeSet::new();
    for ignored in &ignored_paths {
        let relative = safe_ignored_relative(ignored)?;
        inventory_path(workspace, &relative, &mut seen, &mut total, &mut digest)?;
    }
    Ok(IgnoredContentInventory {
        paths: ignored_paths,
        bytes: total,
        content_sha256: hex::encode(digest.finalize()),
    })
}

fn inventory_path(
    workspace: &Path,
    relative: &Path,
    seen: &mut BTreeSet<PathBuf>,
    total: &mut u64,
    digest: &mut Sha256,
) -> Result<()> {
    if !seen.insert(relative.to_path_buf()) {
        return Ok(());
    }
    let path = workspace.join(relative);
    let metadata = fs::symlink_metadata(&path).map_err(|error| io(&path, error))?;
    update_native_path(digest, b"relative", relative)?;
    if metadata.file_type().is_symlink() {
        digest.update(b"link");
        let target = fs::read_link(&path).map_err(|error| io(&path, error))?;
        update_native_path(digest, b"target", &target)?;
        *total = total
            .checked_add(metadata.len())
            .ok_or_else(|| Error::UnsafeState("ignored byte count overflowed".to_owned()))?;
        return Ok(());
    }
    if metadata.is_file() {
        let mut file = open_ignored_inventory_file(&path)?;
        let opened = file.metadata().map_err(|error| io(&path, error))?;
        if !opened_regular_file(&opened) || opened.len() != metadata.len() {
            return Err(Error::UnsafeState(format!(
                "ignored file changed while it was inspected: {}",
                path.display()
            )));
        }
        let expected = opened.len();
        digest.update(b"file");
        digest.update(expected.to_be_bytes());
        let mut observed = 0_u64;
        let mut buffer = vec![0_u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer).map_err(|error| io(&path, error))?;
            if read == 0 {
                break;
            }
            observed = observed
                .checked_add(read as u64)
                .ok_or_else(|| Error::UnsafeState("ignored byte count overflowed".to_owned()))?;
            if observed > expected {
                return Err(Error::UnsafeState(format!(
                    "ignored file changed while it was inspected: {}",
                    path.display()
                )));
            }
            digest.update(&buffer[..read]);
        }
        if observed != expected
            || file.metadata().map_err(|error| io(&path, error))?.len() != expected
        {
            return Err(Error::UnsafeState(format!(
                "ignored file changed while it was inspected: {}",
                path.display()
            )));
        }
        *total = total
            .checked_add(observed)
            .ok_or_else(|| Error::UnsafeState("ignored byte count overflowed".to_owned()))?;
        return Ok(());
    }
    if metadata.is_dir() {
        digest.update(b"dir");
        let mut entries = fs::read_dir(&path)
            .map_err(|error| io(&path, error))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| io(&path, error))?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            inventory_path(
                workspace,
                &relative.join(entry.file_name()),
                seen,
                total,
                digest,
            )?;
        }
        return Ok(());
    }
    Err(Error::UnsafeState(format!(
        "ignored path has an unsupported special-file type: {}",
        path.display()
    )))
}

fn opened_regular_file(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.is_file()
            && metadata.file_attributes()
                & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
                == 0
    }
    #[cfg(not(windows))]
    {
        metadata.is_file()
    }
}

fn open_ignored_inventory_file(path: &Path) -> Result<File> {
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
    options.open(path).map_err(|error| io(path, error))
}

#[cfg_attr(any(unix, windows), allow(clippy::unnecessary_wraps))]
fn update_native_path(digest: &mut Sha256, field: &[u8], path: &Path) -> Result<()> {
    digest.update((field.len() as u64).to_be_bytes());
    digest.update(field);
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let bytes = path.as_os_str().as_bytes();
        digest.update(b"unix-bytes");
        digest.update((bytes.len() as u64).to_be_bytes());
        digest.update(bytes);
        Ok(())
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let units: Vec<u16> = path.as_os_str().encode_wide().collect();
        digest.update(b"windows-utf16le");
        digest.update((units.len() as u64).to_be_bytes());
        for unit in units {
            digest.update(unit.to_le_bytes());
        }
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let value = path
            .to_str()
            .ok_or_else(|| Error::NonUtf8Path(path.to_path_buf()))?;
        digest.update(b"portable-utf8");
        digest.update((value.len() as u64).to_be_bytes());
        digest.update(value.as_bytes());
        Ok(())
    }
}

fn artifact_error(id: &str, error: Error) -> Error {
    match error {
        Error::Io { ref source, .. } if source.kind() == std::io::ErrorKind::NotFound => {
            Error::ResultDrift(id.to_owned())
        }
        Error::Json { .. } | Error::UnsafeState(_) | Error::SchemaVersion { .. } => {
            Error::ResultDrift(id.to_owned())
        }
        other => other,
    }
}

fn recorded_capsule_head(capsule: &Capsule, head: &str) -> bool {
    head == capsule.base_commit
        || capsule
            .checkpoints
            .iter()
            .any(|checkpoint| checkpoint.commit == head)
        || capsule
            .result
            .as_ref()
            .is_some_and(|result| result.head_commit == head)
        || capsule.checkpoint.as_ref().is_some_and(|checkpoint| {
            checkpoint.head_before == head || checkpoint.head_after == head
        })
}

fn checkpoint_ref(capsule: &Capsule) -> String {
    format!("refs/change-capsule/{}/checkpoint", capsule.id)
}

fn integration_ref(capsule: &Capsule) -> String {
    format!("refs/change-capsule/{}/integration", capsule.id)
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum IdempotentCreateTestStage {
    AfterReservation,
    AfterManifest,
}

#[cfg(test)]
static IDEMPOTENT_CREATE_TEST_HOOK: std::sync::Mutex<Option<IdempotentCreateTestStage>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
fn run_idempotent_create_test_hook(stage: IdempotentCreateTestStage) -> Result<()> {
    let mut hook = IDEMPOTENT_CREATE_TEST_HOOK
        .lock()
        .expect("idempotent-create test hook lock");
    if hook.as_ref() == Some(&stage) {
        *hook = None;
        return Err(Error::UnsafeState(
            "injected idempotent creation interruption".to_owned(),
        ));
    }
    Ok(())
}

fn is_unchecked_worktree_shape(path: &Path) -> Result<bool> {
    let metadata = fs::symlink_metadata(path).map_err(|error| io(path, error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Ok(false);
    }
    let mut entries = fs::read_dir(path).map_err(|error| io(path, error))?;
    let Some(entry) = entries
        .next()
        .transpose()
        .map_err(|error| io(path, error))?
    else {
        return Ok(false);
    };
    if entry.file_name() != std::ffi::OsStr::new(".git")
        || entries
            .next()
            .transpose()
            .map_err(|error| io(path, error))?
            .is_some()
    {
        return Ok(false);
    }
    let git_metadata =
        fs::symlink_metadata(entry.path()).map_err(|error| io(entry.path(), error))?;
    Ok(!git_metadata.file_type().is_symlink() && git_metadata.is_file())
}

fn new_capsule_id() -> String {
    format!("cap-{}", Ulid::generate().to_string().to_ascii_lowercase())
}

fn path_entry_exists_no_follow(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io(path, error)),
    }
}

fn validate_reservation_capsule(record: &IdempotencyRecord, capsule: &Capsule) -> Result<()> {
    if capsule.id != record.capsule_id
        || capsule.source_worktree != record.source_worktree
        || capsule.repository_common_dir != record.repository_common_dir
        || capsule.project_key != record.project_key
        || capsule.base_commit != record.base_commit
        || capsule.label != record.label
        || capsule.links != record.links
        || capsule.created_at_unix != record.reserved_at_unix
    {
        return Err(Error::UnsafeState(format!(
            "idempotency reservation does not agree with capsule {}",
            record.capsule_id
        )));
    }
    Ok(())
}

fn validate_create_options(options: &CreateOptions) -> Result<()> {
    if let Some(label) = &options.label {
        validate_bounded_text(label, LABEL_CAP, "label", false)?;
    }
    if options.links.len() > LINKS_LIMIT {
        return Err(Error::InvalidInput(format!(
            "at most {LINKS_LIMIT} links are allowed"
        )));
    }
    for (key, value) in &options.links {
        let valid_key = !key.is_empty()
            && key.len() <= LINK_KEY_CAP
            && key
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
        if !valid_key {
            return Err(Error::InvalidInput(format!("invalid link key: {key:?}")));
        }
        validate_bounded_text(value, LINK_VALUE_CAP, "link value", false)?;
    }
    Ok(())
}

fn validate_author(author: &Author) -> Result<()> {
    validate_bounded_text(&author.name, 256, "author name", false)?;
    validate_bounded_text(&author.email, 512, "author email", false)?;
    if !author.email.contains('@') {
        return Err(Error::InvalidInput(
            "author email must contain '@'".to_owned(),
        ));
    }
    Ok(())
}

fn validate_message(message: &str, label: &str) -> Result<()> {
    validate_bounded_text(message, MESSAGE_CAP, label, true)
}

fn validate_bounded_text(value: &str, cap: usize, label: &str, multiline: bool) -> Result<()> {
    if value.trim().is_empty() || value.len() > cap || value.contains('\0') {
        return Err(Error::InvalidInput(format!(
            "{label} must contain 1-{cap} bytes without NUL"
        )));
    }
    if value
        .chars()
        .any(|character| character.is_control() && !(multiline && matches!(character, '\n' | '\t')))
    {
        return Err(Error::InvalidInput(format!(
            "{label} contains unsupported control characters"
        )));
    }
    Ok(())
}

fn require_state(capsule: &Capsule, expected: CapsuleState, label: &str) -> Result<()> {
    if capsule.state == expected {
        Ok(())
    } else {
        Err(invalid_state(capsule, label))
    }
}

fn invalid_state(capsule: &Capsule, expected: &str) -> Error {
    Error::InvalidState {
        id: capsule.id.clone(),
        state: format!("{:?}", capsule.state).to_ascii_lowercase(),
        expected: expected.to_owned(),
    }
}

fn not_found(id: &str, error: Error) -> Error {
    match error {
        Error::Io { ref source, .. } if source.kind() == std::io::ErrorKind::NotFound => {
            Error::NotFound(id.to_owned())
        }
        other => other,
    }
}

fn now() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| Error::InvalidInput("system clock is before the Unix epoch".to_owned()))
}

fn require_stable_ignored_content(
    initial: &IgnoredContentInventory,
    final_inventory: &IgnoredContentInventory,
) -> Result<()> {
    if initial != final_inventory {
        return Err(Error::UnsafeState(
            "capsule ignored paths or content changed while close was finalizing; no artifacts were written"
                .to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
#[derive(Clone)]
struct CloseIgnoredInventoryTestHook {
    capsule_id: String,
    initial_inventory_captured: std::sync::Arc<std::sync::Barrier>,
    mutation_finished: std::sync::Arc<std::sync::Barrier>,
}

#[cfg(test)]
static CLOSE_IGNORED_INVENTORY_TEST_HOOK: std::sync::Mutex<Option<CloseIgnoredInventoryTestHook>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
fn run_close_ignored_inventory_test_hook(capsule_id: &str) {
    let hook = CLOSE_IGNORED_INVENTORY_TEST_HOOK
        .lock()
        .expect("close ignored-inventory test hook lock")
        .clone();
    if let Some(hook) = hook.filter(|hook| hook.capsule_id == capsule_id) {
        hook.initial_inventory_captured.wait();
        hook.mutation_finished.wait();
    }
}

fn require_stable_close_snapshot(
    initial: &crate::git::Snapshot,
    initial_head: &str,
    initial_clean: bool,
    final_snapshot: &crate::git::Snapshot,
    final_head: &str,
    final_clean: bool,
) -> Result<()> {
    if initial.patch != final_snapshot.patch
        || initial.changed_paths != final_snapshot.changed_paths
        || initial_head != final_head
        || initial_clean != final_clean
    {
        return Err(Error::UnsafeState(
            "capsule tracked content, HEAD, or clean state changed while close was finalizing; no artifacts were written"
                .to_owned(),
        ));
    }
    Ok(())
}

fn result_sha256(result: &CapsuleResult) -> Result<String> {
    let bytes = serde_json::to_vec(result).map_err(|source| Error::Json {
        path: PathBuf::from("result digest"),
        source,
    })?;
    Ok(sha256_hex(&bytes))
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn canonical_existing(path: &Path) -> Result<PathBuf> {
    crate::path::canonicalize(path).map_err(|error| io(path, error))
}

fn same_path_existing_or_clean(left: &Path, right: &Path) -> bool {
    match (
        crate::path::canonicalize(left),
        crate::path::canonicalize(right),
    ) {
        (Ok(left), Ok(right)) => left == right,
        _ => clean_absolute(left) == clean_absolute(right),
    }
}

fn clean_absolute(path: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        Some(path.to_path_buf())
    } else {
        std::env::current_dir().ok().map(|cwd| cwd.join(path))
    }
}

#[cfg(test)]
mod tests;
