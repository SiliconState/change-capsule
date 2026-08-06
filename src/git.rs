use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tempfile::tempfile;

use crate::error::{Error, Result, io};

const SMALL_OUTPUT_CAP: usize = 1024 * 1024;
const STDERR_CAP: usize = 64 * 1024;
const PATCH_OUTPUT_CAP: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone)]
pub(crate) struct Repository {
    pub(crate) worktree: PathBuf,
    pub(crate) common_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub(crate) struct WorktreeRecord {
    pub(crate) path: PathBuf,
    pub(crate) branch: Option<String>,
    pub(crate) head: Option<String>,
    pub(crate) bare: bool,
}

#[derive(Debug)]
pub(crate) struct Snapshot {
    pub(crate) patch: Vec<u8>,
    pub(crate) changed_paths: Vec<String>,
}

pub(crate) struct CommitPatch<'a> {
    pub(crate) worktree: &'a Path,
    pub(crate) base: &'a str,
    pub(crate) patch: &'a [u8],
    pub(crate) index: &'a Path,
    pub(crate) message: &'a str,
    pub(crate) name: &'a str,
    pub(crate) email: &'a str,
}

#[derive(Debug)]
pub(crate) struct Git {
    executable: PathBuf,
}

impl Git {
    pub(crate) fn discover() -> Result<Self> {
        let path = executable_in_path("git")
            .ok_or_else(|| Error::InvalidInput("cannot find Git on PATH".to_owned()))?;
        Ok(Self { executable: path })
    }

    pub(crate) fn repository(&self, path: &Path) -> Result<Repository> {
        let worktree = self
            .text(path, ["rev-parse", "--show-toplevel"])
            .map_err(|error| match error {
                Error::Git { .. } => Error::NotRepository(path.to_path_buf()),
                other => other,
            })?;
        let common_dir = self.text(
            path,
            ["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        Ok(Repository {
            worktree: canonical_existing(Path::new(worktree.trim()))?,
            common_dir: canonical_existing(Path::new(common_dir.trim()))?,
        })
    }

    pub(crate) fn resolve_commit(&self, repo: &Path, revision: &str) -> Result<String> {
        if revision.is_empty() || revision.len() > 512 || revision.contains('\0') {
            return Err(Error::InvalidInput("invalid base revision".to_owned()));
        }
        Ok(self
            .text(
                repo,
                ["rev-parse", "--verify", &format!("{revision}^{{commit}}")],
            )?
            .trim()
            .to_owned())
    }

    pub(crate) fn add_worktree(
        &self,
        repo: &Path,
        path: &Path,
        branch: &str,
        base: &str,
    ) -> Result<()> {
        self.success(
            repo,
            [
                OsString::from("worktree"),
                OsString::from("add"),
                OsString::from("--no-checkout"),
                OsString::from("-b"),
                OsString::from(branch),
                path.as_os_str().to_owned(),
                OsString::from(base),
            ],
        )?;
        self.success(path, ["reset", "--hard", base])
    }

    pub(crate) fn remove_worktree(&self, repo: &Path, path: &Path, force: bool) -> Result<()> {
        let mut args = vec![OsString::from("worktree"), OsString::from("remove")];
        if force {
            args.push(OsString::from("--force"));
        }
        args.push(path.as_os_str().to_owned());
        self.success(repo, args)
    }

    pub(crate) fn delete_branch(&self, repo: &Path, branch: &str) -> Result<()> {
        self.success(repo, ["branch", "-D", branch])
    }

    pub(crate) fn registered_worktrees(&self, repo: &Path) -> Result<Vec<WorktreeRecord>> {
        let output = self.output(repo, ["worktree", "list", "--porcelain", "-z"], None)?;
        parse_worktrees(&output)
    }

    pub(crate) fn head(&self, worktree: &Path) -> Result<String> {
        Ok(self
            .text(worktree, ["rev-parse", "HEAD"])?
            .trim()
            .to_owned())
    }

    pub(crate) fn branch(&self, worktree: &Path) -> Result<String> {
        Ok(self
            .text(worktree, ["symbolic-ref", "--quiet", "--short", "HEAD"])?
            .trim()
            .to_owned())
    }

    pub(crate) fn clean(&self, worktree: &Path) -> Result<bool> {
        Ok(self
            .output(
                worktree,
                ["status", "--porcelain=v1", "-z", "--untracked-files=all"],
                None,
            )?
            .is_empty())
    }

    pub(crate) fn commits_ahead(&self, worktree: &Path, base: &str) -> Result<u64> {
        let range = format!("{base}..HEAD");
        let value = self.text(worktree, ["rev-list", "--count", &range])?;
        value.trim().parse::<u64>().map_err(|_| {
            Error::InvalidInput(format!("Git returned invalid commit count: {value:?}"))
        })
    }

    pub(crate) fn snapshot(&self, worktree: &Path, base: &str, index: &Path) -> Result<Snapshot> {
        let index_value = index.as_os_str().to_owned();
        let env = [(OsString::from("GIT_INDEX_FILE"), index_value)];
        self.success_with_env(worktree, ["read-tree", base], &env)?;
        self.success_with_env(worktree, ["add", "-A", "--", "."], &env)?;
        let patch = self.output_with_env(
            worktree,
            [
                "diff",
                "--cached",
                "--binary",
                "--full-index",
                "--no-ext-diff",
                base,
                "--",
            ],
            &env,
            PATCH_OUTPUT_CAP,
            None,
        )?;
        let paths = self.output_with_env(
            worktree,
            ["diff", "--cached", "--name-only", "-z", base, "--"],
            &env,
            SMALL_OUTPUT_CAP,
            None,
        )?;
        let changed_paths = parse_nul_strings(&paths)?;
        Ok(Snapshot {
            patch,
            changed_paths,
        })
    }

    pub(crate) fn checkpoint(
        &self,
        worktree: &Path,
        message: &str,
        name: &str,
        email: &str,
    ) -> Result<String> {
        self.success(worktree, ["add", "-A", "--", "."])?;
        let env = identity_env(name, email);
        self.success_with_env(worktree, ["commit", "--no-verify", "-m", message], &env)?;
        self.head(worktree)
    }

    pub(crate) fn commit_patch(&self, request: &CommitPatch<'_>) -> Result<String> {
        let mut env = identity_env(request.name, request.email);
        env.push((
            OsString::from("GIT_INDEX_FILE"),
            request.index.as_os_str().to_owned(),
        ));
        self.success_with_env(request.worktree, ["read-tree", request.base], &env)?;
        self.output_with_env(
            request.worktree,
            ["apply", "--cached", "--3way", "--whitespace=nowarn", "-"],
            &env,
            SMALL_OUTPUT_CAP,
            Some(request.patch),
        )?;
        let tree = self.text_with_env(request.worktree, ["write-tree"], &env, None)?;
        let commit = self.text_with_env(
            request.worktree,
            ["commit-tree", tree.trim(), "-p", request.base],
            &env,
            Some(request.message.as_bytes()),
        )?;
        Ok(commit.trim().to_owned())
    }

    pub(crate) fn reset_hard(&self, worktree: &Path, commit: &str) -> Result<()> {
        self.success(worktree, ["reset", "--hard", commit])
    }

    pub(crate) fn prune(&self, repo: &Path) -> Result<()> {
        self.success(repo, ["worktree", "prune"])
    }

    fn text<I, S>(&self, directory: &Path, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let bytes = self.output(directory, args, None)?;
        String::from_utf8(bytes)
            .map_err(|_| Error::InvalidInput("Git returned non-UTF-8 text".to_owned()))
    }

    fn text_with_env<I, S>(
        &self,
        directory: &Path,
        args: I,
        env: &[(OsString, OsString)],
        input: Option<&[u8]>,
    ) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let bytes = self.output_with_env(directory, args, env, SMALL_OUTPUT_CAP, input)?;
        String::from_utf8(bytes)
            .map_err(|_| Error::InvalidInput("Git returned non-UTF-8 text".to_owned()))
    }

    fn success<I, S>(&self, directory: &Path, args: I) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.output(directory, args, None).map(|_| ())
    }

    fn success_with_env<I, S>(
        &self,
        directory: &Path,
        args: I,
        env: &[(OsString, OsString)],
    ) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.output_with_env(directory, args, env, SMALL_OUTPUT_CAP, None)
            .map(|_| ())
    }

    fn output<I, S>(&self, directory: &Path, args: I, input: Option<&[u8]>) -> Result<Vec<u8>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.output_with_env(directory, args, &[], SMALL_OUTPUT_CAP, input)
    }

    fn output_with_env<I, S>(
        &self,
        directory: &Path,
        args: I,
        extra_env: &[(OsString, OsString)],
        stdout_cap: usize,
        input: Option<&[u8]>,
    ) -> Result<Vec<u8>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let arguments: Vec<OsString> = args
            .into_iter()
            .map(|argument| argument.as_ref().to_owned())
            .collect();
        let command_label = render_command(&self.executable, &arguments);
        let mut stdout = tempfile().map_err(|error| io("temporary Git stdout", error))?;
        let mut stderr = tempfile().map_err(|error| io("temporary Git stderr", error))?;
        let stdout_child = stdout
            .try_clone()
            .map_err(|error| io("temporary Git stdout", error))?;
        let stderr_child = stderr
            .try_clone()
            .map_err(|error| io("temporary Git stderr", error))?;

        let mut command = Command::new(&self.executable);
        command.current_dir(directory);
        command.env_clear();
        command.envs(scrubbed_environment());
        command.envs(extra_env.iter().cloned());
        command.args([
            OsString::from("-c"),
            OsString::from("core.hooksPath="),
            OsString::from("-c"),
            OsString::from("core.fsmonitor=false"),
            OsString::from("-c"),
            OsString::from("commit.gpgSign=false"),
            OsString::from("-c"),
            OsString::from("diff.external="),
        ]);
        command.args(&arguments);
        command.stdout(Stdio::from(stdout_child));
        command.stderr(Stdio::from(stderr_child));
        command.stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });

        let mut child = command
            .spawn()
            .map_err(|error| io(&self.executable, error))?;
        if let Some(bytes) = input {
            let mut stdin = child.stdin.take().ok_or(Error::CaptureWorker)?;
            stdin
                .write_all(bytes)
                .map_err(|error| io("Git stdin", error))?;
        }
        let status = child.wait().map_err(|error| io(&self.executable, error))?;
        let stderr_bytes = read_file_bounded(&mut stderr, STDERR_CAP)?;
        if !status.success() {
            return Err(Error::Git {
                command: command_label,
                status: status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&stderr_bytes).trim().to_owned(),
            });
        }
        let stdout_metadata = stdout
            .metadata()
            .map_err(|error| io("temporary Git stdout", error))?;
        if stdout_metadata.len() > stdout_cap as u64 {
            return Err(Error::GitOutputTooLarge {
                command: command_label,
                cap: stdout_cap,
            });
        }
        read_file_bounded(&mut stdout, stdout_cap)
    }
}

fn identity_env(name: &str, email: &str) -> Vec<(OsString, OsString)> {
    vec![
        (OsString::from("GIT_AUTHOR_NAME"), OsString::from(name)),
        (OsString::from("GIT_AUTHOR_EMAIL"), OsString::from(email)),
        (OsString::from("GIT_COMMITTER_NAME"), OsString::from(name)),
        (OsString::from("GIT_COMMITTER_EMAIL"), OsString::from(email)),
    ]
}

fn scrubbed_environment() -> Vec<(OsString, OsString)> {
    std::env::vars_os()
        .filter(|(key, _)| {
            !key.to_string_lossy()
                .to_ascii_uppercase()
                .starts_with("GIT_")
        })
        .collect()
}

fn executable_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let extensions: Vec<OsString> = if cfg!(windows) {
        std::env::var_os("PATHEXT")
            .unwrap_or_else(|| OsString::from(".COM;.EXE;.BAT;.CMD"))
            .to_string_lossy()
            .split(';')
            .map(OsString::from)
            .collect()
    } else {
        vec![OsString::new()]
    };
    for directory in std::env::split_paths(&path) {
        for extension in &extensions {
            let mut file = OsString::from(name);
            file.push(extension);
            let candidate = directory.join(file);
            if candidate.is_file() {
                return fs::canonicalize(&candidate).ok().or(Some(candidate));
            }
        }
    }
    None
}

fn canonical_existing(path: &Path) -> Result<PathBuf> {
    fs::canonicalize(path).map_err(|error| io(path, error))
}

fn read_file_bounded(file: &mut std::fs::File, cap: usize) -> Result<Vec<u8>> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| io("temporary Git output", error))?;
    let mut bytes = Vec::new();
    file.take(cap as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| io("temporary Git output", error))?;
    if bytes.len() > cap {
        bytes.truncate(cap);
    }
    Ok(bytes)
}

fn render_command(executable: &Path, arguments: &[OsString]) -> String {
    let mut rendered = executable.display().to_string();
    for argument in arguments {
        rendered.push(' ');
        rendered.push_str(&argument.to_string_lossy());
    }
    rendered
}

fn parse_nul_strings(bytes: &[u8]) -> Result<Vec<String>> {
    let mut values = Vec::new();
    for item in bytes
        .split(|byte| *byte == 0)
        .filter(|item| !item.is_empty())
    {
        values.push(
            String::from_utf8(item.to_vec())
                .map_err(|_| Error::InvalidInput("Git path is not valid UTF-8".to_owned()))?,
        );
    }
    Ok(values)
}

fn parse_worktrees(bytes: &[u8]) -> Result<Vec<WorktreeRecord>> {
    let fields = parse_nul_strings(bytes)?;
    let mut records = Vec::new();
    let mut current: Option<WorktreeRecord> = None;
    for field in fields {
        if let Some(value) = field.strip_prefix("worktree ") {
            if let Some(record) = current.take() {
                records.push(record);
            }
            current = Some(WorktreeRecord {
                path: PathBuf::from(value),
                branch: None,
                head: None,
                bare: false,
            });
        } else if let Some(record) = current.as_mut() {
            if let Some(value) = field.strip_prefix("HEAD ") {
                record.head = Some(value.to_owned());
            } else if let Some(value) = field.strip_prefix("branch refs/heads/") {
                record.branch = Some(value.to_owned());
            } else if field == "bare" {
                record.bare = true;
            }
        }
    }
    if let Some(record) = current {
        records.push(record);
    }
    Ok(records)
}
