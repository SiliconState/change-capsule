use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use fs2::FileExt;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tempfile::NamedTempFile;

use crate::error::{Error, Result, io};
use crate::model::{Capsule, CapsuleResult, CapsuleState, SCHEMA_VERSION};

const JSON_CAP: u64 = 1024 * 1024;
const PATCH_CAP: u64 = 64 * 1024 * 1024;

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
            if !manifest.exists() {
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
        let path = self.capsule_dir(id)?.join("result.json");
        let result: CapsuleResult = read_versioned_json(&path, JSON_CAP)?;
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
        if !valid_object_id(&result.base_commit)
            || !valid_object_id(&result.head_commit)
            || !valid_sha256(&result.patch_sha256)
            || result.patch_bytes > PATCH_CAP
        {
            return Err(Error::UnsafeState(format!(
                "result for capsule {id} contains malformed identity or size data"
            )));
        }
        Ok(result)
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

fn validate_id(id: &str) -> Result<()> {
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

fn read_versioned_json<T: DeserializeOwned>(path: &Path, cap: u64) -> Result<T> {
    let bytes = read_bytes_bounded(path, cap)?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|source| Error::Json {
            path: path.to_path_buf(),
            source,
        })?;
    let found = value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .and_then(|version| u32::try_from(version).ok())
        .ok_or_else(|| {
            Error::UnsafeState(format!(
                "state JSON has no valid schema_version: {}",
                path.display()
            ))
        })?;
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

fn read_bytes_bounded(path: &Path, cap: u64) -> Result<Vec<u8>> {
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

#[cfg(not(unix))]
fn set_private_dir_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|error| io(path, error))
}

#[cfg(not(unix))]
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
fn sync_directory(_path: &Path) -> Result<()> {
    Ok(())
}
