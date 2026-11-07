use anyhow::{Context as _, Result, bail, ensure};
use futures::{AsyncRead, AsyncReadExt as _};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

pub(crate) fn command() -> PathBuf {
    command_from_environment(std::env::var_os("TIMBER_WORKTREE_CLI"))
}

fn command_from_environment(command: Option<OsString>) -> PathBuf {
    // The editor also installs a `timber` executable; do not invoke it as the worktree backend.
    command
        .filter(|command| !command.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("timber-worktree"))
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RepositoryIdentity {
    pub common_git_dir: PathBuf,
    pub primary_worktree_root: PathBuf,
    pub current_worktree_root: PathBuf,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WorktreeRecord {
    pub path: PathBuf,
    #[serde(deserialize_with = "required_option")]
    pub canonical_path: Option<PathBuf>,
    #[serde(deserialize_with = "required_option")]
    pub branch: Option<String>,
    #[serde(deserialize_with = "required_option")]
    pub head: Option<String>,
    pub is_primary: bool,
    pub detached: bool,
    pub path_exists: bool,
    #[serde(deserialize_with = "required_option")]
    pub locked: Option<WorktreeReason>,
    #[serde(deserialize_with = "required_option")]
    pub prunable: Option<WorktreeReason>,
    pub status: FileStatus,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct WorktreeReason {
    #[serde(deserialize_with = "required_option")]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub(crate) enum FileStatus {
    Known {
        #[serde(deserialize_with = "path_count")]
        modified: usize,
        #[serde(deserialize_with = "path_count")]
        untracked: usize,
        #[serde(deserialize_with = "path_count")]
        ignored: usize,
    },
    Unknown {
        reason: String,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct Inventory {
    pub repository: RepositoryIdentity,
    pub worktrees: Vec<WorktreeRecord>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CleanupOptions {
    pub keep_branch: bool,
    pub discard_ignored: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct CleanupPreview {
    pub approval: serde_json::Value,
    pub tree: WorktreeRecord,
}

fn parse_cleanup_preview(
    approval: serde_json::Value,
    repository: &Path,
    selected: &WorktreeRecord,
    options: &CleanupOptions,
) -> Result<CleanupPreview> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Preview {
        schema_version: u32,
        snapshot: String,
        expires_at: u64,
        repository: RepositoryIdentity,
        worktree: WorktreeRecord,
        options: CleanupOptions,
    }
    let preview: Preview = serde_json::from_value(approval.clone())
        .context("Timber did not return a supported cleanup preview. Update Timber and retry.")?;
    ensure!(
        preview.schema_version == 1
            && preview.snapshot.len() == 64
            && preview
                .snapshot
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit()),
        "Unsupported cleanup preview"
    );
    ensure!(
        preview.expires_at
            > std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_millis() as u64,
        "Cleanup preview expired. Review it again."
    );
    ensure!(
        preview.repository.common_git_dir == repository && preview.options == *options,
        "Cleanup preview belongs to another repository or settings changed"
    );
    ensure!(
        !preview.worktree.is_primary
            && preview.worktree.path_exists
            && preview.worktree.path == selected.path
            && preview.worktree.canonical_path.is_some()
            && preview.worktree.canonical_path == selected.canonical_path
            && preview.worktree.head == selected.head
            && preview.worktree.branch == selected.branch,
        "Worktree changed since selection. Refresh the worktrees and try again."
    );
    Ok(CleanupPreview {
        approval,
        tree: preview.worktree,
    })
}

pub(crate) async fn preview_cleanup(
    cwd: PathBuf,
    repository: PathBuf,
    selected: WorktreeRecord,
    options: CleanupOptions,
    timeout: impl std::future::Future<Output = ()>,
) -> Result<CleanupPreview> {
    with_timeout(
        async {
            let mut arguments = vec!["__ide-done".to_owned(), "preview".to_owned()];
            if options.keep_branch {
                arguments.push("--keep-branch".to_owned());
            }
            if !options.discard_ignored {
                arguments.push("--keep-ignored".to_owned());
            }
            if let Some(base) = &options.base {
                arguments.extend(["--base".to_owned(), base.clone()]);
            }
            arguments.extend([
                "--".to_owned(),
                selected
                    .path
                    .to_str()
                    .context("Worktree path is not UTF-8")?
                    .to_owned(),
            ]);
            let arguments: Vec<_> = arguments.iter().map(OsStr::new).collect();
            let output = command_output(&command(), &arguments, &cwd).await?;
            ensure!(
                output.status.success(),
                "Cannot preview cleanup: {}. Update Timber if native cleanup is unavailable.",
                display_text(&String::from_utf8_lossy(&output.stderr))
            );
            ensure!(
                output.stdout.len() <= DIAGNOSTIC_LIMIT * 64,
                "Cleanup preview exceeded the size limit"
            );
            parse_cleanup_preview(
                serde_json::from_slice(&output.stdout)?,
                &repository,
                &selected,
                &options,
            )
        },
        timeout,
    )
    .await
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MutationCommand {
    New,
    Open,
    RetrySetup,
    Done,
}

impl MutationCommand {
    pub fn name(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Open | Self::RetrySetup => "open",
            Self::Done => "done",
        }
    }

    pub fn arguments(self, input: &str, result_path: &Path) -> Result<Vec<String>> {
        ensure!(self != Self::Done, "Cleanup requires an approved preview");
        let mut arguments = vec![
            self.name().to_owned(),
            "--result-file".to_owned(),
            result_path
                .to_str()
                .context("Result path is not UTF-8")?
                .to_owned(),
            "--no-editor".to_owned(),
            "--no-agent".to_owned(),
        ];
        if self == Self::RetrySetup {
            arguments.push("--setup".to_owned());
        }
        arguments.extend(["--".to_owned(), input.to_owned()]);
        Ok(arguments)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum MutationOutcome {
    Succeeded,
    Cancelled,
    Failed,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MutationResult {
    schema_version: u32,
    command: String,
    pub outcome: MutationOutcome,
    exit_code: i32,
    #[serde(deserialize_with = "required_option")]
    repository: Option<RepositoryIdentity>,
    #[serde(deserialize_with = "required_option")]
    pub worktree: Option<WorktreeRecord>,
    pub materialized: bool,
    #[serde(deserialize_with = "required_option")]
    pub diagnostic: Option<String>,
}

pub(crate) fn parse_mutation_result(
    bytes: &[u8],
    process_exit: Option<i32>,
    command: MutationCommand,
    common_git_dir: &Path,
) -> Result<MutationResult> {
    ensure!(
        bytes.len() <= DIAGNOSTIC_LIMIT * 64,
        "Timber result exceeded the size limit"
    );
    let mut result: MutationResult = serde_json::from_slice(bytes).context(
        "Timber did not return a complete v1 result; reconcile worktrees before retrying",
    )?;
    ensure!(
        result.schema_version == 1,
        "Unsupported Timber result schema"
    );
    ensure!(
        result.command == command.name(),
        "Timber result command mismatch"
    );
    ensure!(
        process_exit == Some(result.exit_code),
        "Timber process and result exit codes disagree"
    );
    match result.outcome {
        MutationOutcome::Succeeded => ensure!(
            result.exit_code == 0
                && result.worktree.is_some()
                && result.repository.is_some()
                && result.diagnostic.is_none(),
            "Invalid successful Timber result"
        ),
        MutationOutcome::Cancelled => ensure!(
            result.exit_code == 0
                && result.worktree.is_none()
                && !result.materialized
                && result.diagnostic.is_none(),
            "Invalid cancelled Timber result"
        ),
        MutationOutcome::Failed => ensure!(
            result.exit_code == 1 && result.diagnostic.is_some(),
            "Invalid failed Timber result"
        ),
    }
    if let Some(repository) = &result.repository {
        ensure!(
            repository.common_git_dir == common_git_dir,
            "Timber result belongs to another repository"
        );
        ensure!(
            repository.primary_worktree_root.is_absolute()
                && repository.current_worktree_root.is_absolute(),
            "Timber returned relative repository paths"
        );
    }
    ensure!(
        !result.materialized || result.worktree.is_some(),
        "Timber omitted the materialized worktree"
    );
    if let Some(tree) = &result.worktree {
        ensure!(
            result.repository.is_some() && tree.path.is_absolute(),
            "Timber returned an invalid worktree target"
        );
        ensure!(
            tree.canonical_path
                .as_ref()
                .is_none_or(|path| path.is_absolute()),
            "Timber returned a relative canonical path"
        );
        ensure!(
            tree.detached == tree.branch.is_none(),
            "Timber returned inconsistent branch identity"
        );
    }
    result.diagnostic = result.diagnostic.as_deref().map(diagnostic_text);
    Ok(result)
}

pub(crate) async fn read_mutation_result(
    path: &Path,
    process_exit: Option<i32>,
    command: MutationCommand,
    common_git_dir: &Path,
) -> Result<MutationResult> {
    let metadata = smol::fs::symlink_metadata(path)
        .await
        .context("Timber result is missing; refresh before retrying")?;
    ensure!(
        metadata.is_file() && metadata.len() <= (DIAGNOSTIC_LIMIT * 64) as u64,
        "Invalid Timber result file"
    );
    let file = smol::fs::File::open(path).await?;
    let bytes = bounded_output(file, DIAGNOSTIC_LIMIT * 64, true).await?;
    parse_mutation_result(&bytes, process_exit, command, common_git_dir)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Envelope {
    schema_version: u32,
    command: String,
    outcome: String,
    exit_code: i32,
    #[serde(deserialize_with = "required_option")]
    repository: Option<RepositoryIdentity>,
    #[serde(deserialize_with = "required_option")]
    worktree: Option<serde_json::Value>,
    materialized: bool,
    #[serde(deserialize_with = "required_option")]
    diagnostic: Option<String>,
    worktrees: Vec<WorktreeRecord>,
}

fn path_count<'de, D: Deserializer<'de>>(deserializer: D) -> std::result::Result<usize, D::Error> {
    let paths = Vec::<String>::deserialize(deserializer)?;
    Ok(paths.into_iter().collect::<HashSet<_>>().len())
}

fn required_option<'de, T: Deserialize<'de>, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error> {
    Option::<T>::deserialize(deserializer)
}

pub(crate) fn parse_inventory(stdout: &[u8], exit_code: Option<i32>) -> Result<Inventory> {
    let record: Envelope = serde_json::from_slice(stdout)
        .context("Timber returned malformed inventory; install a CLI supporting ls --json v1")?;
    ensure!(
        record.schema_version == 1,
        "Unsupported Timber inventory schema {} (expected 1)",
        record.schema_version
    );
    ensure!(
        record.command == "ls" && record.worktree.is_none() && !record.materialized,
        "Invalid Timber listing envelope"
    );
    ensure!(
        exit_code == Some(record.exit_code),
        "Timber process and inventory exit codes disagree"
    );
    if record.outcome == "failed" && record.exit_code == 1 {
        bail!(
            "Timber listing failed: {}",
            diagnostic_text(
                record
                    .diagnostic
                    .as_deref()
                    .unwrap_or("No diagnostic provided")
            )
        );
    }
    ensure!(
        record.outcome == "succeeded" && record.exit_code == 0 && record.diagnostic.is_none(),
        "Timber listing did not succeed"
    );
    let repository = record
        .repository
        .context("Timber inventory has no repository identity")?;
    for path in [
        &repository.common_git_dir,
        &repository.primary_worktree_root,
        &repository.current_worktree_root,
    ] {
        ensure!(
            path.is_absolute(),
            "Timber returned a relative repository path"
        );
    }
    let mut paths = HashSet::new();
    let mut canonical_paths = HashSet::new();
    let mut primary_count = 0;
    for tree in &record.worktrees {
        ensure!(
            tree.path.is_absolute(),
            "Timber returned a relative worktree path"
        );
        ensure!(
            paths.insert(&tree.path),
            "Timber returned duplicate worktree paths"
        );
        ensure!(
            tree.detached == tree.branch.is_none(),
            "Timber returned inconsistent branch state"
        );
        if let Some(canonical_path) = &tree.canonical_path {
            ensure!(
                canonical_path.is_absolute()
                    && tree.path_exists
                    && matches!(tree.status, FileStatus::Known { .. }),
                "Timber returned an invalid canonical worktree identity"
            );
            ensure!(
                canonical_paths.insert(canonical_path),
                "Timber returned duplicate canonical identities"
            );
        } else {
            ensure!(
                matches!(tree.status, FileStatus::Unknown { .. }),
                "Timber returned known status without verified identity"
            );
        }
        if tree.is_primary {
            primary_count += 1;
            ensure!(
                tree.path == repository.primary_worktree_root,
                "Timber primary root does not match inventory"
            );
        }
    }
    ensure!(
        primary_count == 1,
        "Timber inventory must contain one primary worktree"
    );
    ensure!(
        paths.contains(&repository.current_worktree_root),
        "Timber inventory omits the invoking worktree"
    );
    Ok(Inventory {
        repository,
        worktrees: record.worktrees,
    })
}

impl WorktreeRecord {
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if self.is_primary {
            parts.push("primary".to_owned());
        }
        if self.detached {
            parts.push(
                self.head
                    .as_deref()
                    .map(|head| format!("detached HEAD {}", head.get(..8).unwrap_or(head)))
                    .unwrap_or_else(|| "detached HEAD".to_owned()),
            );
        }
        match &self.status {
            FileStatus::Known {
                modified,
                untracked,
                ignored,
            } => {
                if *modified == 0 && *untracked == 0 {
                    parts.push("clean".to_owned());
                } else {
                    parts.push(format!("{modified} modified, {untracked} untracked"));
                }
                if *ignored != 0 {
                    parts.push(format!("{ignored} ignored"));
                }
            }
            FileStatus::Unknown { reason } => {
                parts.push(format!("unknown: {}", diagnostic_text(reason)))
            }
        }
        if !self.path_exists {
            parts.push("missing".to_owned());
        }
        if let Some(locked) = &self.locked {
            parts.push(
                locked
                    .reason
                    .as_deref()
                    .map(|reason| format!("locked: {}", display_text(reason)))
                    .unwrap_or_else(|| "locked".to_owned()),
            );
        }
        if let Some(prunable) = &self.prunable {
            parts.push(
                prunable
                    .reason
                    .as_deref()
                    .map(|reason| format!("prunable: {}", display_text(reason)))
                    .unwrap_or_else(|| "prunable".to_owned()),
            );
        }
        parts.join(" · ")
    }
}

pub(crate) fn display_text(text: &str) -> String {
    text.chars()
        .flat_map(|character| {
            if character.is_control() || matches!(character, '\u{2028}' | '\u{2029}') {
                character.escape_default().collect::<Vec<_>>()
            } else {
                vec![character]
            }
        })
        .collect()
}

fn diagnostic_text(text: &str) -> String {
    let mut displayed = display_text(text);
    if displayed.len() > DIAGNOSTIC_LIMIT {
        let end = displayed
            .char_indices()
            .map(|(offset, _)| offset)
            .take_while(|offset| *offset <= DIAGNOSTIC_LIMIT)
            .last()
            .unwrap_or(0);
        displayed.truncate(end);
        displayed.push_str("… [truncated]");
    }
    displayed
}

const OUTPUT_LIMIT: usize = 64 * 1024 * 1024;
// The inventory includes ignored build-file paths for every linked worktree.
const INVENTORY_OUTPUT_LIMIT: usize = 256 * 1024 * 1024;
const DIAGNOSTIC_LIMIT: usize = 16 * 1024;
pub(crate) const TIMEOUT: Duration = Duration::from_secs(30);

async fn bounded_output(
    reader: impl AsyncRead + Unpin,
    limit: usize,
    fail_on_limit: bool,
) -> Result<Vec<u8>> {
    let mut reader = reader;
    let mut result = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        let remaining = limit.saturating_sub(result.len());
        ensure!(
            !fail_on_limit || count <= remaining,
            "Timber machine output exceeded the size limit"
        );
        result.extend_from_slice(&buffer[..count.min(remaining)]);
    }
    Ok(result)
}

async fn command_output(program: &Path, arguments: &[&OsStr], cwd: &Path) -> Result<Output> {
    command_output_with_stdout_limit(program, arguments, cwd, OUTPUT_LIMIT).await
}

async fn command_output_with_stdout_limit(
    program: &Path,
    arguments: &[&OsStr],
    cwd: &Path,
    stdout_limit: usize,
) -> Result<Output> {
    let mut command = smol::process::Command::new(program);
    #[cfg(windows)]
    {
        use smol::process::windows::CommandExt as _;
        command.creation_flags(0x08000000);
    }
    let mut child = command.args(arguments).current_dir(cwd)
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .kill_on_drop(true).spawn()
        .with_context(|| format!("Could not start {}. Install Timber with ls --json v1 support and ensure it is on PATH", display_text(&program.display().to_string())))?;
    let stdout = child.stdout.take().context("Missing process stdout")?;
    let stderr = child.stderr.take().context("Missing process stderr")?;
    let (stdout, stderr, status) = futures::try_join!(
        bounded_output(stdout, stdout_limit, true),
        bounded_output(stderr, DIAGNOSTIC_LIMIT, false),
        async { child.status().await.map_err(anyhow::Error::from) },
    )?;
    Ok(Output {
        stdout,
        stderr,
        status,
    })
}

async fn with_timeout<T>(
    future: impl std::future::Future<Output = Result<T>>,
    timeout: impl std::future::Future<Output = ()>,
) -> Result<T> {
    smol::future::or(future, async move {
        timeout.await;
        bail!("Timber operation timed out; refresh to retry")
    })
    .await
}

async fn git_path(cwd: &Path, argument: &str) -> Result<PathBuf> {
    let output = command_output(
        Path::new("git"),
        &[
            OsStr::new("rev-parse"),
            OsStr::new("--path-format=absolute"),
            OsStr::new(argument),
        ],
        cwd,
    )
    .await?;
    ensure!(
        output.status.success(),
        "Cannot verify Git identity: {}",
        display_text(&String::from_utf8_lossy(&output.stderr))
    );
    let bytes = output
        .stdout
        .strip_suffix(b"\n")
        .context("Git returned an incomplete path")?;
    let path = PathBuf::from(std::str::from_utf8(bytes).context("Git identity is not UTF-8")?);
    ensure!(path.is_absolute(), "Git returned a relative identity");
    smol::fs::canonicalize(path)
        .await
        .context("Cannot canonicalize Git identity")
}

pub(crate) async fn list_worktrees(
    executable: PathBuf,
    cwd: PathBuf,
    timeout: impl std::future::Future<Output = ()>,
) -> Result<Inventory> {
    with_timeout(
        async {
            let common_git_dir = git_path(&cwd, "--git-common-dir").await?;
            let root = git_path(&cwd, "--show-toplevel").await?;
            let output = command_output_with_stdout_limit(
                &executable,
                &[OsStr::new("ls"), OsStr::new("--json")],
                &cwd,
                INVENTORY_OUTPUT_LIMIT,
            )
            .await?;
            let inventory =
                parse_inventory(&output.stdout, output.status.code()).map_err(|error| {
                    let diagnostic = display_text(&String::from_utf8_lossy(&output.stderr));
                    if diagnostic.is_empty() {
                        error
                    } else {
                        error.context(diagnostic)
                    }
                })?;
            ensure!(
                inventory.repository.common_git_dir == common_git_dir,
                "Timber returned inventory for another repository"
            );
            let current_root =
                smol::fs::canonicalize(&inventory.repository.current_worktree_root).await?;
            ensure!(
                current_root == root,
                "Timber returned inventory for another invoking worktree"
            );
            let current = inventory
                .worktrees
                .iter()
                .find(|tree| tree.path == inventory.repository.current_worktree_root)
                .context("Invoking worktree not listed")?;
            ensure!(
                current
                    .canonical_path
                    .as_ref()
                    .is_none_or(|canonical_path| canonical_path == &root),
                "Timber could not verify the invoking worktree identity"
            );
            Ok(inventory)
        },
        timeout,
    )
    .await
}

pub(crate) async fn verify_worktree(
    path: &Path,
    common_git_dir: &Path,
    canonical_path: &Path,
    timeout: impl std::future::Future<Output = ()>,
) -> Result<()> {
    with_timeout(
        async {
            ensure!(
                smol::fs::canonicalize(path).await? == canonical_path,
                "Worktree path changed since refresh; refresh before opening"
            );
            ensure!(
                git_path(path, "--show-toplevel").await? == canonical_path,
                "Selected path is not a worktree root"
            );
            ensure!(
                git_path(path, "--git-common-dir").await? == common_git_dir,
                "Worktree belongs to another repository; refresh before opening"
            );
            Ok(())
        },
        timeout,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    #[allow(
        clippy::disallowed_methods,
        reason = "These OS subprocess tests do not run on the GPUI scheduler"
    )]
    async fn wall_clock_timer(duration: Duration) {
        smol::Timer::after(duration).await;
    }

    async fn list_worktrees(executable: PathBuf, cwd: PathBuf) -> Result<Inventory> {
        super::list_worktrees(executable, cwd, wall_clock_timer(TIMEOUT)).await
    }

    async fn verify_worktree(path: &Path, common: &Path, canonical: &Path) -> Result<()> {
        super::verify_worktree(path, common, canonical, wall_clock_timer(TIMEOUT)).await
    }

    fn record() -> Value {
        json!({
            "schemaVersion": 1, "command": "ls", "outcome": "succeeded", "exitCode": 0,
            "repository": {"commonGitDir": "/repo/.git", "primaryWorktreeRoot": "/repo", "currentWorktreeRoot": "/repo"},
            "worktree": null, "materialized": false, "diagnostic": null,
            "worktrees": [{"path": "/repo", "canonicalPath": "/repo", "branch": "main", "head": null,
                "isPrimary": true, "detached": false, "pathExists": true, "locked": null, "prunable": null,
                "status": {"kind": "known", "modified": [], "untracked": [], "ignored": []}}]
        })
    }

    #[test]
    fn cleanup_preview_rejects_changed_target_options_and_repository() {
        let inventory = parse_inventory(&serde_json::to_vec(&record()).expect("fixture"), Some(0))
            .expect("inventory");
        let mut tree = inventory.worktrees.first().expect("tree").clone();
        tree.is_primary = false;
        let options = CleanupOptions {
            keep_branch: true,
            discard_ignored: true,
            base: None,
        };
        let mut value = json!({ "schemaVersion": 1, "snapshot": "a".repeat(64), "expiresAt": 9999999999999_u64,
            "repository": record()["repository"], "worktree": record()["worktrees"][0], "options": options });
        value["worktree"]["isPrimary"] = json!(false);
        let preview = parse_cleanup_preview(
            value.clone(),
            &inventory.repository.common_git_dir,
            &tree,
            &options,
        )
        .expect("preview");
        assert_eq!(
            preview.approval, value,
            "preserve the full CLI approval unchanged"
        );
        for pointer in [
            "/repository/commonGitDir",
            "/worktree/path",
            "/worktree/head",
            "/options/keepBranch",
        ] {
            let mut changed = value.clone();
            *changed.pointer_mut(pointer).expect("fixture field") =
                if pointer == "/options/keepBranch" {
                    json!(false)
                } else {
                    json!("changed")
                };
            assert!(
                parse_cleanup_preview(
                    changed,
                    &inventory.repository.common_git_dir,
                    &tree,
                    &options
                )
                .is_err(),
                "{pointer}"
            );
        }
    }

    #[test]
    fn mutation_arguments_keep_selector_literal_and_suppress_launches() {
        let input = "--ticket $(touch unexpected) with spaces";
        for command in [
            MutationCommand::New,
            MutationCommand::Open,
            MutationCommand::RetrySetup,
        ] {
            let arguments = command
                .arguments(input, Path::new("/private result/result.json"))
                .expect("arguments");
            assert_eq!(arguments.first().map(String::as_str), Some(command.name()));
            assert!(arguments.iter().any(|argument| argument == "--no-editor"));
            assert!(arguments.iter().any(|argument| argument == "--no-agent"));
            assert_eq!(arguments.last().map(String::as_str), Some(input));
            assert!(arguments.windows(2).any(|pair| pair == ["--", input]));
            assert_eq!(
                arguments.iter().any(|argument| argument == "--setup"),
                command == MutationCommand::RetrySetup
            );
        }
    }

    #[test]
    fn mutation_results_preserve_partial_failure_and_require_matching_identity() {
        let mut value = record();
        value["command"] = json!("new");
        value["worktree"] = value["worktrees"][0].clone();
        value["materialized"] = json!(true);
        value.as_object_mut().expect("object").remove("worktrees");
        let parse = |value: &Value, exit_code| {
            parse_mutation_result(
                value.to_string().as_bytes(),
                exit_code,
                MutationCommand::New,
                Path::new("/repo/.git"),
            )
        };
        assert_eq!(
            parse(&value, Some(0)).expect("success").outcome,
            MutationOutcome::Succeeded
        );
        assert!(parse(&value, Some(1)).is_err());
        value["repository"]["commonGitDir"] = json!("/other/.git");
        assert!(parse(&value, Some(0)).is_err());
        value["repository"]["commonGitDir"] = json!("/repo/.git");
        value["outcome"] = json!("failed");
        value["exitCode"] = json!(1);
        value["diagnostic"] = json!("setup failed");
        let result = parse(&value, Some(1)).expect("partial failure");
        assert_eq!(result.outcome, MutationOutcome::Failed);
        assert!(result.materialized);
        assert_eq!(
            result.worktree.expect("retained target").path,
            Path::new("/repo")
        );
        value["outcome"] = json!("cancelled");
        value["exitCode"] = json!(0);
        value["diagnostic"] = Value::Null;
        value["worktree"] = Value::Null;
        value["materialized"] = json!(false);
        assert_eq!(
            parse(&value, Some(0)).expect("cancelled").outcome,
            MutationOutcome::Cancelled
        );
        assert!(parse(&value, None).is_err());
    }

    #[test]
    fn accepts_v1_and_additional_fields() {
        let mut value = record();
        value["futureField"] = json!(true);
        let inventory =
            parse_inventory(value.to_string().as_bytes(), Some(0)).expect("valid inventory");
        assert_eq!(inventory.repository.common_git_dir, Path::new("/repo/.git"));
        assert_eq!(inventory.worktrees.len(), 1);
        assert_eq!(
            inventory.worktrees.first().expect("row").summary(),
            "primary · clean"
        );
    }

    #[test]
    fn rejects_incompatible_and_inconsistent_envelopes() {
        for (field, replacement) in [
            ("schemaVersion", json!(2)),
            ("command", json!("open")),
            ("outcome", json!("cancelled")),
            ("exitCode", json!(1)),
            ("materialized", json!(true)),
            ("repository", Value::Null),
            ("worktree", json!({})),
            ("diagnostic", json!("unexpected")),
        ] {
            let mut value = record();
            value[field] = replacement;
            assert!(
                parse_inventory(value.to_string().as_bytes(), Some(0)).is_err(),
                "{field}"
            );
        }
        assert!(parse_inventory(record().to_string().as_bytes(), None).is_err());
        assert!(parse_inventory(b"not JSON", Some(0)).is_err());
    }

    #[test]
    fn requires_nullable_fields() {
        for field in ["diagnostic", "worktree", "repository"] {
            let mut value = record();
            value.as_object_mut().expect("object").remove(field);
            assert!(
                parse_inventory(value.to_string().as_bytes(), Some(0)).is_err(),
                "{field}"
            );
        }
        for field in ["canonicalPath", "branch", "head", "locked", "prunable"] {
            let mut value = record();
            value["worktrees"][0]
                .as_object_mut()
                .expect("row")
                .remove(field);
            assert!(
                parse_inventory(value.to_string().as_bytes(), Some(0)).is_err(),
                "{field}"
            );
        }
    }

    #[test]
    fn rejects_duplicate_identity_and_relative_paths() {
        let mut value = record();
        let row = value["worktrees"][0].clone();
        value["worktrees"].as_array_mut().expect("rows").push(row);
        assert!(parse_inventory(value.to_string().as_bytes(), Some(0)).is_err());
        let mut value = record();
        value["worktrees"][0]["path"] = json!("relative");
        assert!(parse_inventory(value.to_string().as_bytes(), Some(0)).is_err());
    }

    #[test]
    fn failed_inventory_reports_cli_diagnostic() {
        let mut value = record();
        value["outcome"] = json!("failed");
        value["exitCode"] = json!(1);
        value["diagnostic"] = json!("Invalid repository configuration");
        let error = parse_inventory(value.to_string().as_bytes(), Some(1)).expect_err("failure");
        assert!(
            error
                .to_string()
                .contains("Invalid repository configuration")
        );
    }

    #[test]
    fn unknown_and_missing_are_not_clean() {
        let mut value = record();
        value["worktrees"][0]["status"] = json!({"kind": "unknown", "reason": "Permission denied"});
        value["worktrees"][0]["pathExists"] = json!(false);
        value["worktrees"][0]["canonicalPath"] = Value::Null;
        let inventory =
            parse_inventory(value.to_string().as_bytes(), Some(0)).expect("unknown row");
        let summary = inventory.worktrees.first().expect("row").summary();
        assert!(summary.contains("unknown"));
        assert!(summary.contains("missing"));
        assert!(!summary.contains("clean"));
    }
    #[cfg(unix)]
    fn repository() -> tempfile::TempDir {
        let directory = tempfile::Builder::new()
            .prefix("timber repo ")
            .tempdir()
            .expect("temporary repository");
        let status = smol::block_on(
            smol::process::Command::new("git")
                .args(["init", "-q"])
                .arg(directory.path())
                .status(),
        )
        .expect("git init");
        assert!(status.success());
        directory
    }

    #[cfg(unix)]
    fn executable(directory: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = directory.join("timber fixture");
        std::fs::write(&path, format!("#!/usr/bin/python3\n{body}\n")).expect("fixture");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .expect("permissions");
        path
    }

    #[cfg(unix)]
    fn repository_record(directory: &Path) -> Value {
        let root = directory.canonicalize().expect("canonical root");
        let mut value = record();
        value["repository"] = json!({"commonGitDir": root.join(".git"), "primaryWorktreeRoot": root, "currentWorktreeRoot": root});
        value["worktrees"][0]["path"] = json!(root);
        value["worktrees"][0]["canonicalPath"] = json!(root);
        value
    }

    #[cfg(unix)]
    #[test]
    fn transport_uses_literal_arguments_and_captured_cwd() {
        let directory = repository();
        let value = repository_record(directory.path());
        let script = executable(
            directory.path(),
            &format!(
                "import os, sys\nassert sys.argv[1:] == ['ls', '--json']\nassert os.getcwd() == {:?}\nprint({:?})",
                directory
                    .path()
                    .canonicalize()
                    .expect("root")
                    .to_str()
                    .expect("utf8"),
                value.to_string()
            ),
        );
        let inventory = smol::block_on(list_worktrees(script, directory.path().to_path_buf()))
            .expect("inventory");
        assert_eq!(
            inventory.worktrees.first().expect("row").path,
            directory.path()
        );
    }

    #[cfg(unix)]
    #[test]
    fn inventory_transport_accepts_responses_above_generic_command_limit() {
        let directory = repository();
        let value = repository_record(directory.path());
        let script = executable(
            directory.path(),
            &format!(
                "import sys\nsys.stdout.write(' ' * {})\nprint({:?})",
                OUTPUT_LIMIT,
                value.to_string(),
            ),
        );
        let inventory = smol::block_on(list_worktrees(script, directory.path().to_path_buf()))
            .expect("large but bounded inventory");
        assert_eq!(inventory.worktrees.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn command_transport_enforces_its_selected_stdout_limit() {
        let directory = repository();
        let script = executable(directory.path(), "print('1234', end='')");
        let output = smol::block_on(command_output_with_stdout_limit(
            &script,
            &[],
            directory.path(),
            4,
        ))
        .expect("output at the limit");
        assert_eq!(output.stdout, b"1234");
        assert!(
            smol::block_on(command_output_with_stdout_limit(
                &script,
                &[],
                directory.path(),
                3,
            ))
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn transport_rejects_foreign_repository_and_missing_cli() {
        let directory = repository();
        let foreign = repository();
        let value = repository_record(foreign.path());
        let script = executable(directory.path(), &format!("print({:?})", value.to_string()));
        assert!(smol::block_on(list_worktrees(script, directory.path().to_path_buf())).is_err());
        assert!(
            smol::block_on(list_worktrees(
                directory.path().join("absent"),
                directory.path().to_path_buf()
            ))
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn activation_rejects_foreign_or_replaced_identity() {
        let directory = repository();
        let foreign = repository();
        let root = directory.path().canonicalize().expect("root");
        smol::block_on(verify_worktree(&root, &root.join(".git"), &root)).expect("own repository");
        assert!(
            smol::block_on(verify_worktree(
                foreign.path(),
                &root.join(".git"),
                foreign.path()
            ))
            .is_err()
        );
        assert!(
            smol::block_on(verify_worktree(&root, &root.join(".git"), foreign.path())).is_err()
        );
        let alias = directory.path().join("alias");
        std::os::unix::fs::symlink(&root, &alias).expect("alias");
        smol::block_on(verify_worktree(&alias, &root.join(".git"), &root))
            .expect("same identity alias");
    }
    #[cfg(unix)]
    #[test]
    fn drains_diagnostics_and_rejects_oversized_machine_output() {
        let directory = repository();
        let script = executable(
            directory.path(),
            "import sys\nsys.stderr.write('d' * 262144)\nsys.stdout.write('ok')",
        );
        let output =
            smol::block_on(command_output(&script, &[], directory.path())).expect("drained output");
        assert_eq!(output.stdout, b"ok");
        assert_eq!(output.stderr.len(), 16384);
        let script = executable(
            directory.path(),
            "import sys\nsys.stdout.write('x' * (64 * 1024 * 1024 + 1))",
        );
        assert!(smol::block_on(command_output(&script, &[], directory.path())).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn timeout_terminates_subprocess() {
        let directory = repository();
        let pid_file = directory.path().join("pid");
        let script = executable(
            directory.path(),
            &format!(
                "import os, time\nopen({:?}, 'w').write(str(os.getpid()))\ntime.sleep(60)",
                pid_file.to_str().expect("path")
            ),
        );
        let result = smol::block_on(with_timeout(
            command_output(&script, &[], directory.path()),
            wall_clock_timer(Duration::from_millis(750)),
        ));
        assert!(result.is_err());
        let pid = std::fs::read_to_string(pid_file).expect("subprocess started");
        smol::block_on(async {
            for _attempt in 0..100 {
                if !Path::new("/proc").join(&pid).exists() {
                    return;
                }
                wall_clock_timer(Duration::from_millis(10)).await;
            }
            panic!("Timed-out subprocess is still running");
        });
    }

    #[cfg(unix)]
    #[test]
    fn rejects_nonzero_and_signal_exit_even_with_success_record() {
        let directory = repository();
        let value = repository_record(directory.path());
        for ending in ["sys.exit(1)", "os.kill(os.getpid(), signal.SIGTERM)"] {
            let script = executable(
                directory.path(),
                &format!(
                    "import os, signal, sys\nprint({:?}, flush=True)\n{ending}",
                    value.to_string()
                ),
            );
            assert!(
                smol::block_on(list_worktrees(script, directory.path().to_path_buf())).is_err()
            );
        }
    }
    #[cfg(unix)]
    #[test]
    fn listing_keeps_unknown_current_status_visible() {
        let directory = repository();
        let mut value = repository_record(directory.path());
        value["worktrees"][0]["canonicalPath"] = Value::Null;
        value["worktrees"][0]["status"] =
            json!({"kind": "unknown", "reason": "Status unavailable"});
        let script = executable(directory.path(), &format!("print({:?})", value.to_string()));
        let inventory = smol::block_on(list_worktrees(script, directory.path().to_path_buf()))
            .expect("unknown status remains visible");
        assert!(
            inventory
                .worktrees
                .first()
                .expect("row")
                .summary()
                .contains("Status unavailable")
        );
    }
    #[test]
    fn status_counts_are_unique_and_diagnostics_are_bounded() {
        let mut value = record();
        value["worktrees"][0]["status"] = json!({"kind": "known", "modified": ["a", "a", "b"], "untracked": ["c"], "ignored": ["target", "target"]});
        let inventory =
            parse_inventory(value.to_string().as_bytes(), Some(0)).expect("status inventory");
        assert_eq!(
            inventory.worktrees.first().expect("row").summary(),
            "primary · 2 modified, 1 untracked · 1 ignored"
        );
        value["outcome"] = json!("failed");
        value["exitCode"] = json!(1);
        value["diagnostic"] = json!("large diagnostic ".repeat(10000));
        let error = parse_inventory(value.to_string().as_bytes(), Some(1))
            .expect_err("failure")
            .to_string();
        assert!(error.len() < 16500);
        assert!(error.ends_with("[truncated]"));
    }
}

#[cfg(test)]
mod command_tests {
    use super::*;

    #[test]
    fn defaults_to_a_distinct_worktree_backend() {
        assert_eq!(
            command_from_environment(None),
            PathBuf::from("timber-worktree")
        );
        assert_eq!(
            command_from_environment(Some(OsString::new())),
            PathBuf::from("timber-worktree")
        );
    }

    #[test]
    fn preserves_the_configured_backend_path() {
        let path = PathBuf::from("/opt/worktree tools/timber");
        assert_eq!(
            command_from_environment(Some(path.clone().into_os_string())),
            path
        );
    }
}
