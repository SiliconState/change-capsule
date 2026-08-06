use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use fs2::FileExt;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tempfile::NamedTempFile;

use crate::error::{Error, Result, io};
use crate::model::{Capsule, CapsuleResult, SCHEMA_VERSION};

const JSON_CAP: u64 = 1024 * 1024;
const PATCH_CAP: u64 = 64 * 1024 * 1024;

pub fn default_state_root() -> Result<PathBuf> {
    if let Some(path) = env::var_os("CAPSULE_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path).join("change-capsule"));
    }
    if let Some(path) = env::var_os("HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path)
            .join(".local")
            .join("state")
            .join("change-capsule"));
    }
    if let Some(path) = env::var_os("LOCALAPPDATA").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path).join("change-capsule"));
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
    pub(crate) fn open(root: PathBuf) -> Result<Self> {
        ensure_private_dir(&root)?;
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
        let capsule_dir = self.capsule_dir(id)?;
        ensure_private_dir(&capsule_dir)?;
        let project_dir = self.workspaces_dir().join(project_key);
        ensure_private_dir(&project_dir)
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
        write_json_atomic(&path, capsule)
    }

    pub(crate) fn read_capsule(&self, id: &str) -> Result<Capsule> {
        let path = self.capsule_dir(id)?.join("capsule.json");
        let capsule: Capsule = read_json_bounded(&path, JSON_CAP)?;
        if capsule.id != id {
            return Err(Error::UnsafeState(format!(
                "manifest id {} does not match directory {id}",
                capsule.id
            )));
        }
        if capsule.schema_version > SCHEMA_VERSION {
            return Err(Error::SchemaVersion {
                found: capsule.schema_version,
                supported: SCHEMA_VERSION,
            });
        }
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
        let path = self.capsule_dir(id)?.join("result.json");
        write_json_atomic(&path, result)
    }

    pub(crate) fn read_result(&self, id: &str) -> Result<CapsuleResult> {
        let path = self.capsule_dir(id)?.join("result.json");
        read_json_bounded(&path, JSON_CAP)
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
        let lock = PathBuf::from(format!("{}.lock", self.path.display()));
        let _ = fs::remove_file(lock);
    }
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

fn read_json_bounded<T: DeserializeOwned>(path: &Path, cap: u64) -> Result<T> {
    let bytes = read_bytes_bounded(path, cap)?;
    serde_json::from_slice(&bytes).map_err(|source| Error::Json {
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

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|source| Error::Json {
        path: path.to_path_buf(),
        source,
    })?;
    bytes.push(b'\n');
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
