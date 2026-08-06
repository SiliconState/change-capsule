use std::collections::BTreeSet;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use fs2::FileExt;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tempfile::NamedTempFile;

use crate::error::{Error, Result, io};
use crate::model::{
    AUDIT_EVENT_CAP, AUDIT_SCHEMA_VERSION, BackupReport, Capsule, CapsuleResult, CapsuleState,
    SCHEMA_VERSION, StateInspection, StateRecordInspection,
};
use crate::policy::{HARD_PATCH_BYTES, Policy};

const JSON_CAP: u64 = 1024 * 1024;
const PATCH_CAP: u64 = HARD_PATCH_BYTES;
const POLICY_CAP: u64 = 64 * 1024;

/// Resolve the default state directory.
///
/// Honours `CAPSULE_HOME` first, then `XDG_STATE_HOME` on Unix-like platforms
/// or `LOCALAPPDATA` on Windows, then `HOME`.
///
/// # Errors
///
/// Fails when none of those variables identify a usable location.
pub fn default_state_root() -> Result<PathBuf> {
    if let Some(path) = env::var_os("CAPSULE_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    if cfg!(windows) {
        if let Some(path) = env::var_os("LOCALAPPDATA").filter(|value| !value.is_empty()) {
            return Ok(PathBuf::from(path).join("change-capsule"));
        }
    } else if let Some(path) = env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path).join("change-capsule"));
    }
    if let Some(path) = env::var_os("HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path)
            .join(".local")
            .join("state")
            .join("change-capsule"));
    }
    Err(Error::InvalidInput(
        "cannot determine state directory; set CAPSULE_HOME".to_owned(),
    ))
}

#[derive(Debug)]
pub(crate) struct StateStore {
    root: PathBuf,
}

impl StateStore {
    pub(crate) fn open(root: &Path) -> Result<Self> {
        ensure_private_dir(root)?;
        let root = fs::canonicalize(root).map_err(|error| io(root, error))?;
        let store = Self { root };
        ensure_private_dir(&store.capsules_dir())?;
        ensure_private_dir(&store.workspaces_dir())?;
        ensure_private_dir(&store.locks_dir())?;
        Ok(store)
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn capsules_dir(&self) -> PathBuf {
        self.root.join("capsules")
    }

    pub(crate) fn workspaces_dir(&self) -> PathBuf {
        self.root.join("workspaces")
    }

    fn locks_dir(&self) -> PathBuf {
        self.root.join("locks")
    }

    pub(crate) fn capsule_dir(&self, id: &str) -> Result<PathBuf> {
        validate_id(id)?;
        Ok(self.capsules_dir().join(id))
    }

    pub(crate) fn workspace_path(&self, project_key: &str, id: &str) -> Result<PathBuf> {
        validate_project_key(project_key)?;
        validate_id(id)?;
        Ok(self.workspaces_dir().join(project_key).join(id))
    }

    pub(crate) fn prepare_capsule(&self, id: &str, project_key: &str) -> Result<()> {
        validate_project_key(project_key)?;
        validate_id(id)?;
        let project_dir = self.workspaces_dir().join(project_key);
        ensure_private_dir(&project_dir)?;
        let capsule_dir = self.capsule_dir(id)?;
        match fs::create_dir(&capsule_dir) {
            Ok(()) => set_private_dir_permissions(&capsule_dir),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(Error::UnsafeState(format!(
                    "refusing to reuse existing capsule state: {}",
                    capsule_dir.display()
                )))
            }
            Err(error) => Err(io(&capsule_dir, error)),
        }
    }

    pub(crate) fn lock_global(&self) -> Result<StateLock> {
        self.lock_file("global")
    }

    pub(crate) fn lock_project(&self, project_key: &str) -> Result<StateLock> {
        validate_project_key(project_key)?;
        self.lock_file(&format!("project-{project_key}"))
    }

    pub(crate) fn lock_all_projects(&self) -> Result<Vec<StateLock>> {
        let mut keys = BTreeSet::new();
        let directory = self.locks_dir();
        for entry in fs::read_dir(&directory).map_err(|error| io(&directory, error))? {
            let entry = entry.map_err(|error| io(&directory, error))?;
            let file_type = entry.file_type().map_err(|error| io(entry.path(), error))?;
            if file_type.is_symlink() || !file_type.is_file() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(key) = name
                .strip_prefix("project-")
                .and_then(|value| value.strip_suffix(".lock"))
            else {
                continue;
            };
            validate_project_key(key)?;
            keys.insert(key.to_owned());
        }
        keys.into_iter()
            .map(|key| self.lock_project(&key))
            .collect()
    }

    fn lock_file(&self, name: &str) -> Result<StateLock> {
        let path = self.locks_dir().join(format!("{name}.lock"));
        reject_symlink_or_non_file_if_present(&path)?;
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| io(&path, error))?;
        set_private_file_permissions(&path)?;
        let metadata = file.metadata().map_err(|error| io(&path, error))?;
        if !metadata.is_file() {
            return Err(Error::UnsafeState(format!(
                "lock is not a regular file: {}",
                path.display()
            )));
        }
        FileExt::lock_exclusive(&file).map_err(|error| io(&path, error))?;
        Ok(StateLock { _file: file })
    }

    pub(crate) fn write_capsule(&self, capsule: &Capsule) -> Result<()> {
        if capsule.schema_version != SCHEMA_VERSION {
            return Err(Error::SchemaVersion {
                found: capsule.schema_version,
                supported: SCHEMA_VERSION,
            });
        }
        let path = self.capsule_dir(&capsule.id)?.join("capsule.json");
        write_json_atomic_bounded(&path, capsule, JSON_CAP)
    }

    pub(crate) fn read_capsule(&self, id: &str) -> Result<Capsule> {
        let path = self.capsule_dir(id)?.join("capsule.json");
        let capsule: Capsule = read_versioned_json(&path, JSON_CAP)?;
        if capsule.id != id {
            return Err(Error::UnsafeState(format!(
                "manifest id {} does not match directory {id}",
                capsule.id
            )));
        }
        if capsule.schema_version != SCHEMA_VERSION {
            return Err(Error::SchemaVersion {
                found: capsule.schema_version,
                supported: SCHEMA_VERSION,
            });
        }
        validate_capsule_manifest(self, &capsule, id)?;
        Ok(capsule)
    }

    pub(crate) fn list_capsules(&self) -> Result<Vec<Capsule>> {
        let directory = self.capsules_dir();
        let mut capsules = Vec::new();
        for entry in fs::read_dir(&directory).map_err(|error| io(&directory, error))? {
            let entry = entry.map_err(|error| io(&directory, error))?;
            let file_type = entry.file_type().map_err(|error| io(entry.path(), error))?;
            if file_type.is_symlink() || !file_type.is_dir() {
                continue;
            }
            let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if validate_id(&id).is_err() {
                continue;
            }
            let manifest = entry.path().join("capsule.json");
            if !path_entry_exists(&manifest)? {
                continue;
            }
            capsules.push(self.read_capsule(&id)?);
        }
        capsules.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(capsules)
    }

    pub(crate) fn write_result(&self, id: &str, result: &CapsuleResult) -> Result<()> {
        if result.schema_version != SCHEMA_VERSION {
            return Err(Error::SchemaVersion {
                found: result.schema_version,
                supported: SCHEMA_VERSION,
            });
        }
        if result.capsule_id != id {
            return Err(Error::UnsafeState(format!(
                "result id {} does not match capsule {id}",
                result.capsule_id
            )));
        }
        let path = self.capsule_dir(id)?.join("result.json");
        write_json_atomic_bounded(&path, result, JSON_CAP)
    }

    pub(crate) fn read_result(&self, id: &str) -> Result<CapsuleResult> {
        self.read_result_artifact(id).map(|(result, _)| result)
    }

    pub(crate) fn read_result_artifact(&self, id: &str) -> Result<(CapsuleResult, Vec<u8>)> {
        let path = self.capsule_dir(id)?.join("result.json");
        let bytes = read_bytes_bounded(&path, JSON_CAP)?;
        let result: CapsuleResult = decode_versioned_json(&path, &bytes)?;
        if result.capsule_id != id {
            return Err(Error::UnsafeState(format!(
                "result id {} does not match capsule {id}",
                result.capsule_id
            )));
        }
        if !valid_object_id(&result.base_commit)
            || !valid_object_id(&result.head_commit)
            || !valid_sha256(&result.patch_sha256)
            || result
                .ignored_content_sha256
                .as_ref()
                .is_some_and(|digest| !valid_sha256(digest))
            || result.patch_bytes > PATCH_CAP
        {
            return Err(Error::UnsafeState(format!(
                "result for capsule {id} contains malformed identity or size data"
            )));
        }
        Ok((result, bytes))
    }

    pub(crate) fn write_patch(&self, id: &str, patch: &[u8]) -> Result<()> {
        if patch.len() as u64 > PATCH_CAP {
            return Err(Error::InvalidInput(format!(
                "result patch exceeds the {PATCH_CAP}-byte limit"
            )));
        }
        let patch_path = self.capsule_dir(id)?.join("result.patch");
        write_bytes_atomic(&patch_path, patch)
    }

    pub(crate) fn read_patch(&self, id: &str) -> Result<Vec<u8>> {
        let path = self.capsule_dir(id)?.join("result.patch");
        read_bytes_bounded(&path, PATCH_CAP)
    }

    pub(crate) fn read_policy(&self) -> Result<Policy> {
        let path = self.root.join("policy.json");
        if !path_entry_exists(&path)? {
            return Ok(Policy::default());
        }
        let policy: Policy = read_json(&path, POLICY_CAP)?;
        policy.validate()?;
        Ok(policy)
    }

    pub(crate) fn write_policy(&self, policy: &Policy) -> Result<()> {
        policy.validate()?;
        write_json_atomic_bounded(&self.root.join("policy.json"), policy, POLICY_CAP)
    }

    pub(crate) fn state_bytes(&self) -> Result<u64> {
        directory_size(&self.root, &["locks", "workspaces"])
    }

    pub(crate) fn workspace_bytes(&self) -> Result<u64> {
        directory_size(&self.workspaces_dir(), &[])
    }

    pub(crate) fn inspect(&self) -> Result<StateInspection> {
        let _lock = self.lock_global()?;
        let _project_locks = self.lock_all_projects()?;
        let mut records = Vec::new();
        let directory = self.capsules_dir();
        for entry in fs::read_dir(&directory).map_err(|error| io(&directory, error))? {
            let entry = entry.map_err(|error| io(&directory, error))?;
            let file_type = entry.file_type().map_err(|error| io(entry.path(), error))?;
            if file_type.is_symlink() || !file_type.is_dir() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            let manifest = entry.path().join("capsule.json");
            let result = entry.path().join("result.json");
            match inspect_manifest(&manifest) {
                Ok((schema_version, state)) => records.push(StateRecordInspection {
                    id,
                    schema_version,
                    state,
                    has_result: result.is_file(),
                    error: None,
                }),
                Err(error) => records.push(StateRecordInspection {
                    id,
                    schema_version: None,
                    state: None,
                    has_result: result.is_file(),
                    error: Some(error.to_string()),
                }),
            }
        }
        records.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(StateInspection {
            state_root: self.root.clone(),
            supported_schema_version: SCHEMA_VERSION,
            state_bytes: self.state_bytes()?,
            records,
        })
    }

    pub(crate) fn external_destination(&self, destination: &Path) -> Result<PathBuf> {
        let parent = destination
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let parent = fs::canonicalize(parent).map_err(|error| io(parent, error))?;
        let name = destination.file_name().ok_or_else(|| {
            Error::InvalidInput(format!(
                "destination must name a new directory: {}",
                destination.display()
            ))
        })?;
        let normalized = parent.join(name);
        if normalized.starts_with(&self.root) {
            return Err(Error::InvalidInput(format!(
                "destination must be outside managed state: {}",
                destination.display()
            )));
        }
        Ok(normalized)
    }

    pub(crate) fn backup(&self, destination: &Path) -> Result<BackupReport> {
        let destination = self.external_destination(destination)?;
        let _lock = self.lock_global()?;
        let _project_locks = self.lock_all_projects()?;
        self.backup_locked(&destination)
    }

    pub(crate) fn export_artifacts(destination: &Path, files: &[(&str, &[u8])]) -> Result<()> {
        if path_entry_exists(destination)? {
            return Err(Error::InvalidInput(format!(
                "export destination already exists: {}",
                destination.display()
            )));
        }
        let parent = destination.parent().unwrap_or_else(|| Path::new("."));
        validate_existing_directory(parent)?;
        let temporary = tempfile::Builder::new()
            .prefix("change-capsule-export-")
            .tempdir_in(parent)
            .map_err(|error| io(parent, error))?;
        for (name, content) in files {
            write_bytes_atomic(&temporary.path().join(name), content)?;
        }
        publish_staged_directory(&temporary, destination, "bundle.json")
    }

    fn backup_locked(&self, destination: &Path) -> Result<BackupReport> {
        if path_entry_exists(destination)? {
            return Err(Error::InvalidInput(format!(
                "backup destination already exists: {}",
                destination.display()
            )));
        }
        let parent = destination.parent().unwrap_or_else(|| Path::new("."));
        validate_existing_directory(parent)?;
        let temporary = tempfile::Builder::new()
            .prefix("change-capsule-backup-")
            .tempdir_in(parent)
            .map_err(|error| io(parent, error))?;
        let root = temporary.path();
        ensure_private_dir(&root.join("capsules"))?;
        let mut files = 0;
        let mut bytes = 0;
        let capsules = self.capsules_dir();
        for entry in fs::read_dir(&capsules).map_err(|error| io(&capsules, error))? {
            let entry = entry.map_err(|error| io(&capsules, error))?;
            let file_type = entry.file_type().map_err(|error| io(entry.path(), error))?;
            if file_type.is_symlink() || !file_type.is_dir() {
                continue;
            }
            let target = root.join("capsules").join(entry.file_name());
            ensure_private_dir(&target)?;
            for (name, cap) in [
                ("capsule.json", JSON_CAP),
                ("result.json", JSON_CAP),
                ("result.patch", PATCH_CAP),
            ] {
                let source = entry.path().join(name);
                if path_entry_exists(&source)? {
                    let content = read_bytes_bounded(&source, cap)?;
                    write_bytes_atomic(&target.join(name), &content)?;
                    files += 1;
                    bytes += content.len() as u64;
                }
            }
        }
        let policy_path = self.root.join("policy.json");
        if path_entry_exists(&policy_path)? {
            let content = read_bytes_bounded(&policy_path, POLICY_CAP)?;
            write_bytes_atomic(&root.join("policy.json"), &content)?;
            files += 1;
            bytes += content.len() as u64;
        }
        let report = BackupReport {
            source: self.root.clone(),
            destination: destination.to_path_buf(),
            files,
            bytes,
        };
        write_json_atomic_bounded(&root.join("backup.json"), &report, JSON_CAP)?;
        publish_staged_directory(&temporary, destination, "backup.json")?;
        Ok(report)
    }

    pub(crate) fn temporary_index(&self, id: &str) -> Result<TemporaryIndex> {
        let directory = self.capsule_dir(id)?;
        let temporary = tempfile::Builder::new()
            .prefix("index-")
            .tempfile_in(&directory)
            .map_err(|error| io(&directory, error))?;
        let (file, path) = temporary
            .keep()
            .map_err(|error| io(&directory, error.error))?;
        drop(file);
        fs::remove_file(&path).map_err(|error| io(&path, error))?;
        Ok(TemporaryIndex { path })
    }
}

#[derive(Debug)]
pub(crate) struct StateLock {
    _file: File,
}

#[derive(Debug)]
pub(crate) struct TemporaryIndex {
    path: PathBuf,
}

impl TemporaryIndex {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TemporaryIndex {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let mut lock = self.path.as_os_str().to_owned();
        lock.push(".lock");
        let _ = fs::remove_file(PathBuf::from(lock));
    }
}

fn validate_capsule_manifest(store: &StateStore, capsule: &Capsule, id: &str) -> Result<()> {
    validate_project_key(&capsule.project_key)?;
    let expected_workspace = store.workspace_path(&capsule.project_key, id)?;
    let expected_branch = format!("capsule/{}", &id[4..]);
    if malformed_capsule_identity(capsule, &expected_workspace, &expected_branch) {
        return Err(Error::UnsafeState(format!(
            "capsule {id} contains inconsistent or malformed identity data"
        )));
    }
    if !lifecycle_journals_consistent(capsule) {
        return Err(Error::UnsafeState(format!(
            "capsule {id} contains an inconsistent lifecycle journal"
        )));
    }
    if project_key(&capsule.repository_common_dir)? != capsule.project_key {
        return Err(Error::UnsafeState(format!(
            "capsule {id} contains an inconsistent repository identity"
        )));
    }
    Ok(())
}

fn malformed_capsule_identity(
    capsule: &Capsule,
    expected_workspace: &Path,
    expected_branch: &str,
) -> bool {
    capsule.workspace_path != expected_workspace
        || capsule.branch != expected_branch
        || !capsule.source_worktree.is_absolute()
        || !capsule.repository_common_dir.is_absolute()
        || capsule
            .workspace_git_dir
            .as_ref()
            .is_some_and(|path| !path.is_absolute())
        || (matches!(
            capsule.state,
            CapsuleState::Active
                | CapsuleState::Checkpointing
                | CapsuleState::Closed
                | CapsuleState::Integrating
                | CapsuleState::Integrated
        ) && capsule.workspace_git_dir.is_none())
        || !valid_object_id(&capsule.base_commit)
        || capsule
            .checkpoints
            .iter()
            .any(|checkpoint| !valid_object_id(&checkpoint.commit))
        || capsule
            .result
            .as_ref()
            .is_some_and(|result| !valid_result_ref(result))
        || capsule.checkpoint.as_ref().is_some_and(|checkpoint| {
            !valid_object_id(&checkpoint.head_before)
                || !valid_object_id(&checkpoint.head_after)
                || !valid_sha256(&checkpoint.patch_sha256)
        })
        || capsule.integration.as_ref().is_some_and(|integration| {
            !integration.target_worktree.is_absolute()
                || !integration.target_git_dir.is_absolute()
                || integration.target_head_ref.is_empty()
                || integration.target_head_ref.len() > 1024
                || integration.target_head_ref.chars().any(char::is_control)
                || !valid_object_id(&integration.target_head_before)
                || integration
                    .target_head_after
                    .as_ref()
                    .is_some_and(|head| !valid_object_id(head))
        })
        || capsule
            .cleanup
            .as_ref()
            .and_then(|cleanup| cleanup.branch_head.as_ref())
            .is_some_and(|head| !valid_object_id(head))
        || capsule.audit_events_dropped > 0 && capsule.audit_events.len() < AUDIT_EVENT_CAP
        || invalid_audit_events(capsule)
}

fn invalid_audit_events(capsule: &Capsule) -> bool {
    if capsule.audit_events.len() > AUDIT_EVENT_CAP {
        return true;
    }
    let mut previous_time = capsule.created_at_unix;
    for event in &capsule.audit_events {
        let valid_id = event.event_id.len() == 30
            && event.event_id.starts_with("evt-")
            && event
                .event_id
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
        if event.schema_version != AUDIT_SCHEMA_VERSION
            || !valid_id
            || event.capsule_id.as_deref() != Some(capsule.id.as_str())
            || event.project_key.as_deref() != Some(capsule.project_key.as_str())
            || event.state.is_none()
            || event.occurred_at_unix < previous_time
            || event.attributes.iter().any(|(key, value)| {
                key.is_empty()
                    || key.len() > 64
                    || value.len() > 4096
                    || key.chars().any(char::is_control)
                    || value.chars().any(char::is_control)
            })
        {
            return true;
        }
        previous_time = event.occurred_at_unix;
    }
    false
}

fn lifecycle_journals_consistent(capsule: &Capsule) -> bool {
    let checkpoint_consistent = if capsule.checkpoint.is_some() {
        matches!(
            capsule.state,
            CapsuleState::Checkpointing | CapsuleState::Dropping
        )
    } else {
        capsule.state != CapsuleState::Checkpointing
    };
    let integration_consistent = if capsule.integration.is_some() {
        matches!(
            capsule.state,
            CapsuleState::Integrating
                | CapsuleState::Integrated
                | CapsuleState::Dropping
                | CapsuleState::Dropped
        )
    } else {
        !matches!(
            capsule.state,
            CapsuleState::Integrating | CapsuleState::Integrated
        )
    };
    let cleanup_consistent = (capsule.state == CapsuleState::Dropping) == capsule.cleanup.is_some();
    checkpoint_consistent && integration_consistent && cleanup_consistent
}

pub(crate) fn validate_id(id: &str) -> Result<()> {
    let valid = id.len() >= 8
        && id.len() <= 64
        && id.starts_with("cap-")
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if valid {
        Ok(())
    } else {
        Err(Error::InvalidId(id.to_owned()))
    }
}

fn valid_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_result_ref(result: &crate::model::ResultRef) -> bool {
    valid_object_id(&result.head_commit)
        && valid_sha256(&result.patch_sha256)
        && valid_sha256(&result.result_sha256)
        && result.patch_bytes <= PATCH_CAP
}

fn project_key(common_dir: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};

    let path = common_dir
        .to_str()
        .ok_or_else(|| Error::NonUtf8Path(common_dir.to_path_buf()))?;
    let digest = Sha256::digest(path.as_bytes());
    Ok(hex::encode(&digest[..12]))
}

fn validate_project_key(key: &str) -> Result<()> {
    if key.len() == 24 && key.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(Error::UnsafeState(format!("invalid project key: {key}")))
    }
}

fn ensure_private_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(Error::UnsafeState(format!(
                    "expected a non-symlink directory: {}",
                    path.display()
                )));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path).map_err(|error| io(path, error))?;
            let metadata = fs::symlink_metadata(path).map_err(|error| io(path, error))?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(Error::UnsafeState(format!(
                    "directory changed during creation: {}",
                    path.display()
                )));
            }
        }
        Err(error) => return Err(io(path, error)),
    }
    set_private_dir_permissions(path)
}

fn reject_symlink_or_non_file_if_present(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(Error::UnsafeState(format!(
                "expected a regular non-symlink file: {}",
                path.display()
            )))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io(path, error)),
    }
}

fn publish_staged_directory(
    staged: &tempfile::TempDir,
    destination: &Path,
    completion_marker: &str,
) -> Result<()> {
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    match fs::create_dir(destination) {
        Ok(()) => set_private_dir_permissions(destination)?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(Error::InvalidInput(format!(
                "destination already exists: {}",
                destination.display()
            )));
        }
        Err(error) => return Err(io(destination, error)),
    }

    let marker = staged.path().join(completion_marker);
    let mut entries = fs::read_dir(staged.path())
        .map_err(|error| io(staged.path(), error))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| io(staged.path(), error))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        if entry.path() == marker {
            continue;
        }
        let target = destination.join(entry.file_name());
        fs::rename(entry.path(), &target).map_err(|error| io(&target, error))?;
    }
    let target_marker = destination.join(completion_marker);
    fs::rename(&marker, &target_marker).map_err(|error| io(&target_marker, error))?;
    sync_directory(destination)?;
    sync_directory(parent)
}

fn path_entry_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io(path, error)),
    }
}

fn validate_existing_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| io(path, error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(Error::UnsafeState(format!(
            "expected an existing non-symlink directory: {}",
            path.display()
        )));
    }
    Ok(())
}

fn directory_size(root: &Path, excluded_root_names: &[&str]) -> Result<u64> {
    validate_existing_directory(root)?;
    let mut total = 0_u64;
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory).map_err(|error| io(&directory, error))? {
            let entry = entry.map_err(|error| io(&directory, error))?;
            if directory == root
                && excluded_root_names
                    .iter()
                    .any(|name| entry.file_name() == std::ffi::OsStr::new(name))
            {
                continue;
            }
            let file_type = entry.file_type().map_err(|error| io(entry.path(), error))?;
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_file() {
                total = total
                    .checked_add(
                        entry
                            .metadata()
                            .map_err(|error| io(entry.path(), error))?
                            .len(),
                    )
                    .ok_or_else(|| {
                        Error::UnsafeState("filesystem byte count overflowed".to_owned())
                    })?;
            }
        }
    }
    Ok(total)
}

fn inspect_manifest(path: &Path) -> Result<(Option<u32>, Option<String>)> {
    let value = read_json_value(path, JSON_CAP)?;
    let version = value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u32::try_from(value).ok());
    let state = value
        .get("state")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    Ok((version, state))
}

fn read_json<T: DeserializeOwned>(path: &Path, cap: u64) -> Result<T> {
    let value = read_json_value(path, cap)?;
    serde_json::from_value(value).map_err(|source| Error::Json {
        path: path.to_path_buf(),
        source,
    })
}

fn read_json_value(path: &Path, cap: u64) -> Result<serde_json::Value> {
    let bytes = read_bytes_bounded(path, cap)?;
    serde_json::from_slice(&bytes).map_err(|source| Error::Json {
        path: path.to_path_buf(),
        source,
    })
}

fn json_version(value: &serde_json::Value, path: &Path) -> Result<u32> {
    value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .and_then(|version| u32::try_from(version).ok())
        .ok_or_else(|| {
            Error::UnsafeState(format!(
                "state JSON has no valid schema_version: {}",
                path.display()
            ))
        })
}

fn read_versioned_json<T: DeserializeOwned>(path: &Path, cap: u64) -> Result<T> {
    let bytes = read_bytes_bounded(path, cap)?;
    decode_versioned_json(path, &bytes)
}

pub(crate) fn decode_versioned_json<T: DeserializeOwned>(path: &Path, bytes: &[u8]) -> Result<T> {
    let value = serde_json::from_slice(bytes).map_err(|source| Error::Json {
        path: path.to_path_buf(),
        source,
    })?;
    let found = json_version(&value, path)?;
    if found != SCHEMA_VERSION {
        return Err(Error::SchemaVersion {
            found,
            supported: SCHEMA_VERSION,
        });
    }
    serde_json::from_value(value).map_err(|source| Error::Json {
        path: path.to_path_buf(),
        source,
    })
}

pub(crate) fn read_bytes_bounded(path: &Path, cap: u64) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path).map_err(|error| io(path, error))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > cap {
        return Err(Error::UnsafeState(format!(
            "state file is unsafe or exceeds {cap} bytes: {}",
            path.display()
        )));
    }
    let mut file = File::open(path).map_err(|error| io(path, error))?;
    let mut bytes = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take(cap + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| io(path, error))?;
    if bytes.len() as u64 > cap {
        return Err(Error::UnsafeState(format!(
            "state file exceeds {cap} bytes: {}",
            path.display()
        )));
    }
    Ok(bytes)
}

fn write_json_atomic_bounded<T: Serialize>(path: &Path, value: &T, cap: u64) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|source| Error::Json {
        path: path.to_path_buf(),
        source,
    })?;
    bytes.push(b'\n');
    if bytes.len() as u64 > cap {
        return Err(Error::InvalidInput(format!(
            "state JSON exceeds the {cap}-byte limit: {}",
            path.display()
        )));
    }
    write_bytes_atomic(path, &bytes)
}

fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        Error::UnsafeState(format!("state path has no parent: {}", path.display()))
    })?;
    ensure_private_dir(parent)?;
    reject_symlink_or_non_file_if_present(path)?;
    let mut temporary = NamedTempFile::new_in(parent).map_err(|error| io(parent, error))?;
    temporary
        .as_file_mut()
        .write_all(bytes)
        .map_err(|error| io(temporary.path(), error))?;
    temporary
        .as_file_mut()
        .sync_all()
        .map_err(|error| io(temporary.path(), error))?;
    set_private_file_permissions(temporary.path())?;
    temporary
        .persist(path)
        .map_err(|error| io(path, error.error))?;
    sync_directory(parent)
}

#[cfg(unix)]
fn set_private_dir_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|error| io(path, error))
}

// The non-Unix stubs mirror the fallible Unix signatures so every call site
// stays platform-independent; they cannot fail, hence the allow.
#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
fn set_private_dir_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|error| io(path, error))
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
fn set_private_file_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| io(path, error))
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
fn sync_directory(_path: &Path) -> Result<()> {
    Ok(())
}
