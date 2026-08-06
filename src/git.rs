use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

use crate::error::{Error, Result, io};

const SMALL_OUTPUT_CAP: usize = 1024 * 1024;
const STDERR_CAP: usize = 64 * 1024;
const PATCH_OUTPUT_CAP: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone)]
pub(crate) struct Repository {
    pub(crate) worktree: PathBuf,
    pub(crate) common_dir: PathBuf,
    pub(crate) git_dir: PathBuf,
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
        let git_dir = self.text(
            path,
            ["rev-parse", "--path-format=absolute", "--absolute-git-dir"],
        )?;
        Ok(Repository {
            worktree: canonical_existing(Path::new(worktree.trim()))?,
            common_dir: canonical_existing(Path::new(common_dir.trim()))?,
            git_dir: canonical_existing(Path::new(git_dir.trim()))?,
        })
    }

    pub(crate) fn resolve_commit(&self, repo: &Path, revision: &str) -> Result<String> {
        if revision.is_empty()
            || revision.len() > 512
            || revision.starts_with('-')
            || revision.chars().any(char::is_control)
        {
            return Err(Error::InvalidInput("invalid base revision".to_owned()));
        }
        let commit = self
            .text(
                repo,
                ["rev-parse", "--verify", &format!("{revision}^{{commit}}")],
            )?
            .trim()
            .to_owned();
        if !valid_object_id(&commit) {
            return Err(Error::InvalidInput(
                "Git returned a malformed base commit ID".to_owned(),
            ));
        }
        Ok(commit)
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

    pub(crate) fn branch_head(&self, repo: &Path, branch: &str) -> Result<Option<String>> {
        self.ref_head(repo, &format!("refs/heads/{branch}"))
    }

    pub(crate) fn create_ref(&self, repo: &Path, reference: &str, commit: &str) -> Result<()> {
        let zero = "0".repeat(commit.len());
        self.success(repo, ["update-ref", reference, commit, &zero])
    }

    pub(crate) fn delete_ref_if_matches(
        &self,
        repo: &Path,
        reference: &str,
        expected: &str,
    ) -> Result<()> {
        let Some(current) = self.ref_head(repo, reference)? else {
            return Ok(());
        };
        if current != expected {
            return Err(Error::UnsafeState(format!(
                "refusing to delete ref {reference}: expected {expected}, found {current}"
            )));
        }
        self.success(repo, ["update-ref", "-d", reference, expected])
    }

    pub(crate) fn ref_head(&self, repo: &Path, reference: &str) -> Result<Option<String>> {
        let value = self.text(
            repo,
            ["for-each-ref", "--format=%(objectname)", "--", reference],
        )?;
        let value = value.trim();
        if value.is_empty() {
            Ok(None)
        } else {
            Ok(Some(value.to_owned()))
        }
    }

    pub(crate) fn delete_branch_if_matches(
        &self,
        repo: &Path,
        branch: &str,
        expected: &str,
    ) -> Result<()> {
        let Some(current) = self.branch_head(repo, branch)? else {
            return Ok(());
        };
        if current != expected {
            return Err(Error::UnsafeState(format!(
                "refusing to delete branch {branch}: expected {expected}, found {current}"
            )));
        }
        if self
            .registered_worktrees(repo)?
            .iter()
            .any(|record| record.branch.as_deref() == Some(branch))
        {
            return Err(Error::UnsafeState(format!(
                "refusing to delete checked-out branch {branch}"
            )));
        }
        self.delete_ref_if_matches(repo, &format!("refs/heads/{branch}"), expected)
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

    pub(crate) fn head_ref(&self, worktree: &Path) -> Result<String> {
        let reference = self
            .text(worktree, ["rev-parse", "--symbolic-full-name", "HEAD"])?
            .trim()
            .to_owned();
        if reference.is_empty() || reference.len() > 1024 || reference.chars().any(char::is_control)
        {
            return Err(Error::InvalidInput(
                "Git returned an invalid HEAD reference".to_owned(),
            ));
        }
        Ok(reference)
    }

    pub(crate) fn branch(&self, worktree: &Path) -> Result<String> {
        Ok(self
            .text(worktree, ["symbolic-ref", "--quiet", "--short", "HEAD"])?
            .trim()
            .to_owned())
    }

    pub(crate) fn clean(&self, worktree: &Path) -> Result<bool> {
        Ok(self.status_bytes(worktree, false)?.is_empty())
    }

    pub(crate) fn sparse_checkout(&self, worktree: &Path) -> Result<bool> {
        let value = self.text(
            worktree,
            [
                "config",
                "--type=bool",
                "--default=false",
                "--get",
                "core.sparseCheckout",
            ],
        )?;
        match value.trim() {
            "true" => Ok(true),
            "false" => Ok(false),
            other => Err(Error::InvalidInput(format!(
                "Git returned invalid core.sparseCheckout value: {other:?}"
            ))),
        }
    }

    pub(crate) fn hidden_index_entries(&self, worktree: &Path) -> Result<bool> {
        let output = self.output_with_env(
            worktree,
            ["ls-files", "-v", "-z"],
            &[],
            PATCH_OUTPUT_CAP,
            None,
        )?;
        Ok(output.split(|byte| *byte == 0).any(|entry| {
            entry
                .first()
                .is_some_and(|tag| *tag == b'S' || tag.is_ascii_lowercase())
        }))
    }

    pub(crate) fn dirty_submodules(&self, worktree: &Path) -> Result<bool> {
        Ok(self.status_bytes(worktree, false)? != self.status_bytes(worktree, true)?)
    }

    fn status_bytes(&self, worktree: &Path, ignore_dirty_submodules: bool) -> Result<Vec<u8>> {
        let mut arguments = vec!["status", "--porcelain=v2", "-z", "--untracked-files=all"];
        arguments.push(if ignore_dirty_submodules {
            "--ignore-submodules=dirty"
        } else {
            "--ignore-submodules=none"
        });
        self.output(worktree, arguments, None)
    }

    pub(crate) fn ignored_paths(&self, worktree: &Path) -> Result<Vec<String>> {
        let output = self.output_with_env(
            worktree,
            [
                "ls-files",
                "--others",
                "--ignored",
                "--exclude-standard",
                "--directory",
                "-z",
            ],
            &[],
            SMALL_OUTPUT_CAP,
            None,
        )?;
        parse_nul_strings(&output)
    }

    pub(crate) fn commits_ahead(&self, worktree: &Path, base: &str) -> Result<u64> {
        let range = format!("{base}..HEAD");
        let value = self.text(worktree, ["rev-list", "--count", &range])?;
        value.trim().parse::<u64>().map_err(|_| {
            Error::InvalidInput(format!("Git returned invalid commit count: {value:?}"))
        })
    }

    pub(crate) fn parents(&self, worktree: &Path, commit: &str) -> Result<Vec<String>> {
        let value = self.text(worktree, ["rev-list", "--parents", "-n", "1", commit])?;
        let mut fields = value.split_whitespace();
        let Some(resolved) = fields.next() else {
            return Err(Error::InvalidInput(
                "Git returned an empty commit parent record".to_owned(),
            ));
        };
        if resolved != commit || !valid_object_id(resolved) {
            return Err(Error::InvalidInput(format!(
                "Git resolved unexpected commit {resolved} while inspecting {commit}"
            )));
        }
        Ok(fields.map(str::to_owned).collect())
    }

    pub(crate) fn snapshot(&self, worktree: &Path, base: &str, index: &Path) -> Result<Snapshot> {
        let index_value = index.as_os_str().to_owned();
        let env = [(OsString::from("GIT_INDEX_FILE"), index_value)];
        self.success_with_env(worktree, ["read-tree", base], &env)?;
        self.success_with_env(worktree, ["add", "-A", "--", "."], &env)?;
        let raw = self.output_with_env(
            worktree,
            [
                "diff",
                "--cached",
                "--raw",
                "-z",
                "--no-renames",
                base,
                "--",
            ],
            &env,
            PATCH_OUTPUT_CAP,
            None,
        )?;
        for path in parse_changed_gitlinks(&raw)? {
            self.output_with_env(
                worktree,
                ["submodule", "status", "--cached", "--", &path],
                &env,
                SMALL_OUTPUT_CAP,
                None,
            )
            .map_err(|_| {
                Error::InvalidInput(format!(
                    "unregistered embedded Git repository cannot be represented safely: {path}"
                ))
            })?;
        }
        let patch = self.output_with_env(
            worktree,
            [
                "diff",
                "--cached",
                "--binary",
                "--full-index",
                "--no-ext-diff",
                "--no-textconv",
                "--no-renames",
                "--no-color",
                base,
                "--",
            ],
            &env,
            PATCH_OUTPUT_CAP,
            None,
        )?;
        let paths = self.output_with_env(
            worktree,
            [
                "diff",
                "--cached",
                "--name-only",
                "-z",
                "--no-renames",
                base,
                "--",
            ],
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

    pub(crate) fn commit_snapshot(
        &self,
        repo: &Path,
        base: &str,
        commit: &str,
    ) -> Result<Snapshot> {
        let patch = self.output_with_env(
            repo,
            [
                "diff",
                "--binary",
                "--full-index",
                "--no-ext-diff",
                "--no-textconv",
                "--no-renames",
                "--no-color",
                base,
                commit,
                "--",
            ],
            &[],
            PATCH_OUTPUT_CAP,
            None,
        )?;
        let paths = self.output_with_env(
            repo,
            [
                "diff",
                "--name-only",
                "-z",
                "--no-renames",
                base,
                commit,
                "--",
            ],
            &[],
            SMALL_OUTPUT_CAP,
            None,
        )?;
        Ok(Snapshot {
            patch,
            changed_paths: parse_nul_strings(&paths)?,
        })
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

    /// Apply a sealed patch onto the base tree in a private index and report
    /// the reproduced patch and changed paths without creating a commit.
    pub(crate) fn apply_patch_preview(
        &self,
        worktree: &Path,
        base: &str,
        patch: &[u8],
        index: &Path,
    ) -> Result<Snapshot> {
        let env = [(
            OsString::from("GIT_INDEX_FILE"),
            index.as_os_str().to_owned(),
        )];
        self.success_with_env(worktree, ["read-tree", base], &env)?;
        self.output_with_env(
            worktree,
            ["apply", "--cached", "--whitespace=nowarn", "-"],
            &env,
            SMALL_OUTPUT_CAP,
            Some(patch),
        )?;
        let reproduced = self.output_with_env(
            worktree,
            [
                "diff",
                "--cached",
                "--binary",
                "--full-index",
                "--no-ext-diff",
                "--no-textconv",
                "--no-renames",
                "--no-color",
                base,
                "--",
            ],
            &env,
            PATCH_OUTPUT_CAP,
            None,
        )?;
        let paths = self.output_with_env(
            worktree,
            [
                "diff",
                "--cached",
                "--name-only",
                "-z",
                "--no-renames",
                base,
                "--",
            ],
            &env,
            SMALL_OUTPUT_CAP,
            None,
        )?;
        Ok(Snapshot {
            patch: reproduced,
            changed_paths: parse_nul_strings(&paths)?,
        })
    }

    pub(crate) fn fast_forward(&self, worktree: &Path, commit: &str) -> Result<()> {
        self.success(worktree, ["merge", "--ff-only", "--no-edit", commit])
    }

    pub(crate) fn advance_branch(
        &self,
        worktree: &Path,
        branch: &str,
        commit: &str,
        previous: &str,
    ) -> Result<()> {
        let reference = format!("refs/heads/{branch}");
        self.success(worktree, ["update-ref", &reference, commit, previous])?;
        self.success(worktree, ["reset", "--mixed", commit])
    }

    pub(crate) fn reset_index(&self, worktree: &Path, commit: &str) -> Result<()> {
        self.success(worktree, ["reset", "--mixed", commit])
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
        let hooks_directory =
            tempfile::tempdir().map_err(|error| io("temporary Git hooks directory", error))?;
        let mut hooks_config = OsString::from("core.hooksPath=");
        hooks_config.push(hooks_directory.path());

        let mut command = Command::new(&self.executable);
        command.current_dir(directory);
        command.env_clear();
        command.envs(scrubbed_environment());
        command.envs(extra_env.iter().cloned());
        command.arg("--no-optional-locks");
        command.args([
            OsString::from("-c"),
            hooks_config,
            OsString::from("-c"),
            OsString::from("core.fsmonitor=false"),
            OsString::from("-c"),
            OsString::from("commit.gpgSign=false"),
            OsString::from("-c"),
            OsString::from("diff.external="),
        ]);
        command.args(&arguments);
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        command.stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });

        let mut child = command
            .spawn()
            .map_err(|error| io(&self.executable, error))?;
        let child_stdout = child.stdout.take().ok_or(Error::CaptureWorker)?;
        let child_stderr = child.stderr.take().ok_or(Error::CaptureWorker)?;
        let stdout_worker = thread::spawn(move || capture_bounded(child_stdout, stdout_cap));
        let stderr_worker = thread::spawn(move || capture_bounded(child_stderr, STDERR_CAP));

        let input_error = if let Some(bytes) = input {
            match child.stdin.take() {
                Some(mut stdin) => stdin.write_all(bytes).err(),
                None => Some(std::io::Error::other("Git stdin pipe was unavailable")),
            }
        } else {
            None
        };
        let status = child.wait().map_err(|error| io(&self.executable, error))?;
        let stdout = stdout_worker
            .join()
            .map_err(|_| Error::CaptureWorker)?
            .map_err(|error| io("Git stdout", error))?;
        let stderr = stderr_worker
            .join()
            .map_err(|_| Error::CaptureWorker)?
            .map_err(|error| io("Git stderr", error))?;
        if !status.success() {
            return Err(Error::Git {
                command: command_label,
                status: status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&stderr.bytes).trim().to_owned(),
            });
        }
        if let Some(error) = input_error {
            return Err(io("Git stdin", error));
        }
        if stdout.exceeded {
            return Err(Error::GitOutputTooLarge {
                command: command_label,
                cap: stdout_cap,
            });
        }
        Ok(stdout.bytes)
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
                return crate::path::canonicalize(&candidate)
                    .ok()
                    .or(Some(candidate));
            }
        }
    }
    None
}

fn valid_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn canonical_existing(path: &Path) -> Result<PathBuf> {
    crate::path::canonicalize(path).map_err(|error| io(path, error))
}

#[derive(Debug)]
struct CapturedOutput {
    bytes: Vec<u8>,
    exceeded: bool,
}

fn capture_bounded(mut stream: impl Read, cap: usize) -> std::io::Result<CapturedOutput> {
    let mut bytes = Vec::with_capacity(cap.min(64 * 1024));
    let mut exceeded = false;
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let remaining = cap.saturating_add(1).saturating_sub(bytes.len());
        let retained = count.min(remaining);
        bytes.extend_from_slice(&buffer[..retained]);
        exceeded |= count > retained || bytes.len() > cap;
    }
    if bytes.len() > cap {
        bytes.truncate(cap);
    }
    Ok(CapturedOutput { bytes, exceeded })
}

fn render_command(executable: &Path, arguments: &[OsString]) -> String {
    let mut rendered = executable.display().to_string();
    for argument in arguments {
        rendered.push(' ');
        rendered.push_str(&argument.to_string_lossy());
    }
    rendered
}

fn parse_changed_gitlinks(bytes: &[u8]) -> Result<Vec<String>> {
    let mut fields = bytes
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty());
    let mut paths = Vec::new();
    while let Some(header) = fields.next() {
        let header = std::str::from_utf8(header).map_err(|_| {
            Error::InvalidInput("Git returned invalid raw diff metadata".to_owned())
        })?;
        let mut metadata = header.split_whitespace();
        let _old_mode = metadata.next().unwrap_or_default().trim_start_matches(':');
        let new_mode = metadata.next().unwrap_or_default();
        let _old_object = metadata.next();
        let _new_object = metadata.next();
        let status = metadata.next().unwrap_or_default();
        let path = fields.next().ok_or_else(|| {
            Error::InvalidInput("Git returned an incomplete raw diff record".to_owned())
        })?;
        if status.starts_with('R') || status.starts_with('C') {
            return Err(Error::UnsafeState(
                "Git returned rename metadata despite --no-renames".to_owned(),
            ));
        }
        if new_mode == "160000" {
            paths.push(
                String::from_utf8(path.to_vec())
                    .map_err(|_| Error::InvalidInput("Git path is not valid UTF-8".to_owned()))?,
            );
        }
    }
    Ok(paths)
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
