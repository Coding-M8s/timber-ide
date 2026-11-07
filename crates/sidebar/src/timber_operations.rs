use crate::timber_cli::{
    self, CleanupPreview, Inventory, MutationCommand, MutationOutcome, MutationResult,
};
use anyhow::{Context as _, Result, ensure};
use gpui::{
    App, AppContext as _, Context, Entity, EntityId, Global, Subscription, WeakEntity, Window,
    WindowHandle,
};
use std::{collections::HashMap, path::PathBuf, time::Duration};
use terminal::{Terminal, TerminalBuilder, TerminalMode};
use terminal_view::{TerminalView, terminal_panel::TerminalPanel};
use util::{ResultExt as _, paths::PathStyle};
use workspace::{MultiWorkspace, Workspace};

#[derive(Clone)]
pub(crate) struct OperationRequest {
    pub common_git_dir: PathBuf,
    pub cwd: PathBuf,
    pub canonical_cwd: PathBuf,
    pub reconciliation_cwd: PathBuf,
    pub command: MutationCommand,
    pub cleanup: Option<CleanupPreview>,
    pub input: String,
    pub sidebar: EntityId,
    pub generation: u64,
    pub workspace: WeakEntity<Workspace>,
    pub window: WindowHandle<MultiWorkspace>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum OperationPhase {
    Running,
    Reconciling,
    Finished,
    NeedsRefresh,
}

#[derive(Clone)]
pub(crate) struct Operation {
    pub id: u64,
    pub request: OperationRequest,
    pub phase: OperationPhase,
    pub result: Option<MutationResult>,
    pub inventory: Option<Inventory>,
    pub error: Option<String>,
    pub reconciliation_error: Option<String>,
    pub terminal: Option<Entity<Terminal>>,
    pub view: Option<Entity<TerminalView>>,
    interrupted: bool,
    agent_started: bool,
}

impl Operation {
    pub fn status(&self) -> &'static str {
        match self.phase {
            OperationPhase::Running => "Running",
            OperationPhase::Reconciling => "Refreshing worktrees",
            OperationPhase::NeedsRefresh => "Refresh required before retry",
            OperationPhase::Finished => match self.result.as_ref() {
                Some(result)
                    if result.outcome == MutationOutcome::Succeeded && self.error.is_none() =>
                {
                    "Completed"
                }
                Some(result) if result.outcome == MutationOutcome::Cancelled => "Cancelled",
                Some(result) if result.materialized => "Worktree created; follow-up failed",
                _ => "Failed; worktrees refreshed",
            },
        }
    }
}

#[derive(Default)]
struct RepositorySlots {
    next_id: u64,
    active: HashMap<PathBuf, u64>,
}

impl RepositorySlots {
    fn acquire(&mut self, repository: PathBuf) -> Result<u64> {
        ensure!(
            !self.active.contains_key(&repository),
            "A Timber operation is already running for this repository"
        );
        self.next_id = self
            .next_id
            .checked_add(1)
            .context("Timber operation IDs exhausted")?;
        self.active.insert(repository, self.next_id);
        Ok(self.next_id)
    }

    fn release(&mut self, repository: &PathBuf, operation: u64) {
        if self.active.get(repository) == Some(&operation) {
            self.active.remove(repository);
        }
    }
}

#[derive(Default)]
pub(crate) struct TimberOperations {
    slots: RepositorySlots,
    pub operations: Vec<Operation>,
    window_subscription: Option<Subscription>,
}

struct SharedOperations(Entity<TimberOperations>);
impl Global for SharedOperations {}

impl TimberOperations {
    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(shared) = cx.try_global::<SharedOperations>() {
            return shared.0.clone();
        }
        let operations = cx.new(|_| Self::default());
        let subscription = cx.on_window_closed({
            let operations = operations.downgrade();
            move |cx, window_id| {
                operations
                    .update(cx, |operations, cx| {
                        for operation in &mut operations.operations {
                            if operation.request.window.window_id() == window_id
                                && operation.phase == OperationPhase::Running
                            {
                                operation.interrupted = true;
                                if let Some(terminal) = &operation.terminal {
                                    terminal.update(cx, |terminal, _| terminal.terminate_process());
                                }
                            }
                        }
                        cx.notify();
                    })
                    .log_err();
            }
        });
        operations.update(cx, |operations, _| {
            operations.window_subscription = Some(subscription)
        });
        cx.set_global(SharedOperations(operations.clone()));
        operations
    }

    pub fn busy(&self, repository: &PathBuf) -> bool {
        self.slots.active.contains_key(repository)
    }

    pub fn start(&mut self, request: OperationRequest, cx: &mut Context<Self>) -> Result<u64> {
        if let Some(operation) = self.operations.iter().find(|operation| {
            self.slots.active.get(&request.common_git_dir) == Some(&operation.id)
                && operation.request.command == request.command
                && operation.request.input == request.input
        }) {
            return Ok(operation.id);
        }
        let command = timber_cli::command()
            .to_str()
            .context("The Timber worktree CLI path is not UTF-8")?
            .to_owned();
        let directory = tempfile::Builder::new()
            .prefix("timber-operation-")
            .tempdir()?;
        let result_path = directory.path().join("result.json");
        let approval_path = directory.path().join("cleanup-approval.json");
        let arguments = if request.command == MutationCommand::Done {
            let preview = request
                .cleanup
                .as_ref()
                .context("Review the cleanup preview first")?;
            ensure!(
                preview.tree.path.to_str() == Some(request.input.as_str()),
                "Cleanup target changed"
            );
            vec![
                "__ide-done".to_owned(),
                "apply".to_owned(),
                "--approval-file".to_owned(),
                approval_path
                    .to_str()
                    .context("Approval path is not UTF-8")?
                    .to_owned(),
                "--result-file".to_owned(),
                result_path
                    .to_str()
                    .context("Result path is not UTF-8")?
                    .to_owned(),
                "--".to_owned(),
                request.input.clone(),
            ]
        } else {
            ensure!(
                request.cleanup.is_none(),
                "Cleanup approval supplied to another operation"
            );
            request.command.arguments(&request.input, &result_path)?
        };
        let workspace = request
            .workspace
            .upgrade()
            .context("The originating workspace has closed")?;
        ensure!(
            workspace.read(cx).panel::<TerminalPanel>(cx).is_some(),
            "The terminal panel is not ready; try again shortly"
        );
        let project = workspace.read(cx).project().clone();
        let settings = project
            .read(cx)
            .terminal_settings(&Some(request.cwd.clone()), cx)
            .clone();
        let id = self.slots.acquire(request.common_git_dir.clone())?;
        cx.observe_release(&workspace, move |operations, _, cx| {
            if let Some(operation) = operations.operation_mut(id) {
                if operation.phase == OperationPhase::Running {
                    operation.interrupted = true;
                    if let Some(terminal) = &operation.terminal {
                        terminal.update(cx, |terminal, _| terminal.terminate_process());
                    }
                }
            }
            cx.notify();
        })
        .detach();
        self.operations.push(Operation {
            id,
            request: request.clone(),
            phase: OperationPhase::Running,
            result: None,
            inventory: None,
            error: None,
            reconciliation_error: None,
            terminal: None,
            view: None,
            interrupted: false,
            agent_started: false,
        });
        let operations = cx.entity();
        cx.spawn(async move |_, cx| {
            let result = async {
                if let Some(preview) = &request.cleanup {
                    smol::fs::write(&approval_path, serde_json::to_vec(&preview.approval)?).await?;
                }
                let timer = cx.background_executor().timer(timber_cli::TIMEOUT);
                cx.background_spawn({
                    let request = request.clone();
                    async move {
                        timber_cli::verify_worktree(
                            &request.cwd,
                            &request.common_git_dir,
                            &request.canonical_cwd,
                            timer,
                        )
                        .await
                    }
                })
                .await?;
                request.window.update(cx, |_, _, _| ())?;
                ensure!(request.workspace.upgrade().is_some(), "The originating workspace closed before command launch");
                // TerminalBuilder executes the program and argument array directly. Task terminals
                // format shell commands, which would interpret ticket input as shell syntax.
                let (completion_sender, completion_receiver) = async_channel::bounded(1);
                let builder = cx.update(|cx| {
                    TerminalBuilder::new(
                        Some(request.cwd.clone()),
                        TerminalMode::interactive_with_retained_output(completion_sender),
                        task::Shell::WithArguments {
                            program: command,
                            args: arguments,
                            title_override: Some(format!(
                                "Timber {} #{id}",
                                request.command.name()
                            )),
                        },
                        settings.env,
                        settings.cursor_shape,
                        settings.alternate_scroll,
                        Some(
                            settings
                                .max_scroll_history_lines
                                .unwrap_or(10_000)
                                .min(10_000),
                        ),
                        settings.path_hyperlink_regexes,
                        Duration::from_millis(settings.path_hyperlink_timeout_ms),
                        false,
                        project.entity_id().as_u64(),
                        cx,
                        Vec::new(),
                        PathStyle::local(),
                    )
                });
                let builder = builder.await?;
                let terminal = cx.new(|cx| builder.subscribe(cx));
                operations.update(cx, |operations, cx| {
                    if let Some(operation) = operations.operation_mut(id) {
                        operation.terminal = Some(terminal.clone());
                        if operation.interrupted {
                            terminal.update(cx, |terminal, _| terminal.terminate_process());
                        }
                    }
                    cx.notify();
                });
                let attached = request
                    .window
                    .update(cx, |multi_workspace, window, cx| {
                        let take_focus = request.command != MutationCommand::Done
                            && multi_workspace.workspace().entity_id() == request.workspace.entity_id();
                        let view = request.workspace.update(cx, |workspace, cx| {
                            let panel = workspace
                                .panel::<TerminalPanel>(cx)
                                .context("Terminal panel closed")?;
                            let pane = panel
                                .read(cx)
                                .panes()
                                .first()
                                .map(|pane| (*pane).clone())
                                .context("Terminal pane is unavailable")?;
                            let view = cx.new(|cx| {
                                TerminalView::new(
                                    terminal.clone(),
                                    request.workspace.clone(),
                                    workspace.database_id(),
                                    project.downgrade(),
                                    window,
                                    cx,
                                )
                            });
                            pane.update(cx, |pane, cx| {
                                pane.add_item(
                                    Box::new(view.clone()),
                                    true,
                                    take_focus,
                                    None,
                                    window,
                                    cx,
                                );
                            });
                            if take_focus && !workspace.has_active_modal(window, cx) {
                                workspace.focus_panel::<TerminalPanel>(window, cx);
                            }
                            Ok::<_, anyhow::Error>(view)
                        })??;
                        operations.update(cx, |operations, cx| {
                            if let Some(operation) = operations.operation_mut(id) {
                                operation.view = Some(view);
                            }
                            cx.notify();
                        });
                        Ok::<_, anyhow::Error>(())
                    })
                    .and_then(|result| result);
                if let Err(error) = attached {
                    log::error!("Could not attach Timber operation output: {error:#}");
                    terminal.update(cx, |terminal, _| terminal.terminate_process());
                    operations.update(cx, |operations, cx| {
                        if let Some(operation) = operations.operation_mut(id) {
                            operation.interrupted = true;
                        }
                        cx.notify();
                    });
                }
                let status = completion_receiver.recv().await.ok().flatten();
                let exit_code = status.and_then(|status| status.code());
                terminal.update(cx, |terminal, cx| {
                    let exit = exit_code.map(|code| format!("exit {code}"))
                        .unwrap_or_else(|| "interrupted".to_owned());
                    terminal.write_output(format!("\r\n[Timber command finished: {exit}. Saved command output.]\r\n\x1b[?25l").as_bytes(), cx);
                });
                timber_cli::read_mutation_result(
                    &result_path,
                    exit_code,
                    request.command,
                    &request.common_git_dir,
                )
                .await
            }
            .await;
            operations.update(cx, |operations, cx| {
                if let Some(operation) = operations.operation_mut(id) {
                    match result {
                        Ok(result) => {
                            operation.error = result.diagnostic.clone();
                            operation.result = Some(result);
                        }
                        Err(error) => operation.error = Some(format!("{error:#}")),
                    }
                    if operation.interrupted {
                        operation.error = Some("The operation's terminal became unavailable. The command was interrupted; inspect reconciled worktrees before retrying.".to_owned());
                    }
                    operation.phase = OperationPhase::Reconciling;
                }
                cx.notify();
            });
            let timer = cx.background_executor().timer(timber_cli::TIMEOUT);
            let inventory = cx.background_spawn({
                let cwd = request.reconciliation_cwd.clone();
                async move { timber_cli::list_worktrees(timber_cli::command(), cwd, timer).await }
            }).await;
            operations.update(cx, |operations, cx| {
                operations.finish_reconciliation(id, inventory, cx)
            });
            if let Err(error) = directory.close() {
                log::error!("Could not remove Timber operation result directory: {error}");
            }
        })
        .detach();
        self.operations.retain(|operation| {
            operation.id.saturating_add(20) > id || operation.phase != OperationPhase::Finished
        });
        cx.notify();
        Ok(id)
    }

    pub fn claim_agent_launch(&mut self, id: u64) -> bool {
        let Some(operation) = self.operation_mut(id) else {
            return false;
        };
        if operation.request.command == MutationCommand::Done
            || operation.agent_started
            || operation.phase != OperationPhase::Finished
            || operation.error.is_some()
            || !operation
                .result
                .as_ref()
                .is_some_and(|result| result.outcome == MutationOutcome::Succeeded)
        {
            return false;
        }
        operation.agent_started = true;
        true
    }

    pub fn release_agent_launch(&mut self, id: u64) {
        if let Some(operation) = self.operation_mut(id) {
            operation.agent_started = false;
        }
    }

    fn operation_mut(&mut self, id: u64) -> Option<&mut Operation> {
        self.operations
            .iter_mut()
            .find(|operation| operation.id == id)
    }

    fn finish_reconciliation(
        &mut self,
        id: u64,
        inventory: Result<Inventory>,
        cx: &mut Context<Self>,
    ) {
        let Some(operation) = self.operation_mut(id) else {
            return;
        };
        let inventory = inventory.and_then(|inventory| {
            ensure!(
                inventory.repository.common_git_dir == operation.request.common_git_dir,
                "Repository identity changed while refreshing the operation"
            );
            Ok(inventory)
        });
        match inventory {
            Ok(inventory) => {
                operation.reconciliation_error = None;
                operation.inventory = Some(inventory);
                operation.phase = OperationPhase::Finished;
                let repository = operation.request.common_git_dir.clone();
                self.slots.release(&repository, id);
            }
            Err(error) => {
                let previous = operation.error.as_deref().unwrap_or("Command exited");
                operation.reconciliation_error = Some(format!(
                    "{previous}. Could not reconcile worktrees: {error:#}"
                ));
                operation.phase = OperationPhase::NeedsRefresh;
            }
        }
        cx.notify();
    }

    pub fn reconcile(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(operation) = self.operation_mut(id) else {
            return;
        };
        if operation.phase != OperationPhase::NeedsRefresh {
            return;
        }
        let cwd = operation.request.reconciliation_cwd.clone();
        operation.phase = OperationPhase::Reconciling;
        let timer = cx.background_executor().timer(timber_cli::TIMEOUT);
        let task = cx.background_spawn(async move {
            timber_cli::list_worktrees(timber_cli::command(), cwd, timer).await
        });
        let operations = cx.entity();
        cx.spawn(async move |_, cx| {
            let inventory = task.await;
            operations.update(cx, |operations, cx| {
                operations.finish_reconciliation(id, inventory, cx)
            });
        })
        .detach();
        cx.notify();
    }

    pub fn output(&self, id: u64, window: &mut Window, cx: &mut App) -> Result<()> {
        let operation = self
            .operations
            .iter()
            .find(|operation| operation.id == id)
            .context("Operation is unavailable")?;
        let view = operation
            .view
            .clone()
            .context("Output terminal is not available")?;
        let workspace = operation
            .request
            .workspace
            .upgrade()
            .context("The output workspace has closed")?;
        let multi_workspace = window
            .window_handle()
            .downcast::<MultiWorkspace>()
            .context("Not a workspace window")?;
        ensure!(
            multi_workspace == operation.request.window,
            "Output belongs to its originating window"
        );
        let multi_workspace = window
            .root::<MultiWorkspace>()
            .flatten()
            .context("Workspace window is unavailable")?;
        multi_workspace.update(cx, |multi_workspace, cx| {
            multi_workspace.activate(workspace.clone(), None, window, cx);
            workspace.update(cx, |workspace, cx| {
                let panel = workspace
                    .panel::<TerminalPanel>(cx)
                    .context("Terminal panel is unavailable")?;
                let pane = panel
                    .read(cx)
                    .panes()
                    .into_iter()
                    .find(|pane| pane.read(cx).index_for_item(&view).is_some())
                    .cloned();
                let pane = pane
                    .or_else(|| panel.read(cx).panes().first().map(|pane| (*pane).clone()))
                    .context("Output pane is unavailable")?;
                pane.update(cx, |pane, cx| {
                    if let Some(index) = pane.index_for_item(&view) {
                        pane.activate_item(index, true, true, window, cx);
                    } else {
                        pane.add_item(Box::new(view.clone()), true, true, None, window, cx);
                    }
                });
                workspace.focus_panel::<TerminalPanel>(window, cx);
                Ok(())
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_slot_survives_duplicates_and_late_completion() {
        let repository = PathBuf::from("/repo/.git");
        let mut slots = RepositorySlots::default();
        let first = slots.acquire(repository.clone()).expect("first operation");
        assert!(slots.acquire(repository.clone()).is_err());
        slots.release(&repository, first);
        let second = slots.acquire(repository.clone()).expect("second operation");
        slots.release(&repository, first);
        assert_eq!(slots.active.get(&repository), Some(&second));
        assert!(slots.acquire(PathBuf::from("/other/.git")).is_ok());
    }
}
