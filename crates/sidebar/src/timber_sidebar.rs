use crate::timber_cli::{
    self, FileStatus, Inventory, MutationCommand, MutationOutcome, WorktreeRecord, display_text,
};
use crate::timber_operations::{Operation, OperationPhase, OperationRequest, TimberOperations};
use anyhow::{Context as _, Result};
use editor::Editor;
use futures::{FutureExt as _, StreamExt as _, future::BoxFuture};
use gpui::prelude::*;
use gpui::{
    App, Context, Entity, EntityId, EventEmitter, FocusHandle, Focusable, Pixels, Render,
    SharedString, Subscription, Task, WeakEntity, Window, px,
};
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use ui::ActiveTheme as _;
use ui::{ButtonLike, Tooltip, prelude::*};
use util::ResultExt as _;
use util::path_list::PathList;
use workspace::{
    MultiWorkspace, MultiWorkspaceEvent, OpenMode, Sidebar as WorkspaceSidebar, SidebarEvent,
    SidebarSide, Workspace,
};

fn inventory_path_changed(path: &Path, repository: &Path, roots: &[PathBuf]) -> bool {
    path == repository
        || path.starts_with(repository.join("worktrees"))
        || roots
            .iter()
            .any(|root| root.starts_with(path) || path == root.join(".git"))
}

type InventoryLoader = Arc<dyn Fn(PathBuf) -> BoxFuture<'static, Result<Inventory>> + Send + Sync>;
type WorktreeVerifier =
    Arc<dyn Fn(PathBuf, PathBuf, PathBuf) -> BoxFuture<'static, Result<()>> + Send + Sync>;

struct RetainedWorktree {
    workspace: WeakEntity<Workspace>,
    common_git_dir: PathBuf,
    roots: PathList,
    tree: WorktreeRecord,
}

struct HandoffBinding {
    command: PathBuf,
    socket: PathBuf,
    project_ids_by_workspace: HashMap<EntityId, u64>,
}

pub struct TimberSidebar {
    multi_workspace: WeakEntity<MultiWorkspace>,
    focus_handle: FocusHandle,
    width: Pixels,
    cwd: Option<PathBuf>,
    inventory: Option<Inventory>,
    selected_common_git_dir: Option<PathBuf>,
    retained: HashMap<EntityId, RetainedWorktree>,
    error: Option<SharedString>,
    loading: bool,
    generation: Rc<Cell<u64>>,
    activation_generation: Rc<Cell<u64>>,
    loader: InventoryLoader,
    verifier: WorktreeVerifier,
    refresh_task: Option<Task<()>>,
    refresh_pending: bool,
    debounce_task: Option<Task<()>>,
    inventory_watch: Option<Task<()>>,
    watched_paths: Vec<PathBuf>,
    terminal_monitor: Option<Task<()>>,
    git_subscription: Option<Subscription>,
    activation_tasks: Vec<Task<()>>,
    handoff_task: Option<Task<()>>,
    handoff_listener: Option<Task<()>>,
    handoff_binding: Option<HandoffBinding>,
    project_subscription: Option<Subscription>,
    operations: Entity<TimberOperations>,
    handled_operations: HashSet<u64>,
    input: Option<Entity<Editor>>,
    form_open: bool,
    show_paths: bool,
    expanded_operations: HashSet<u64>,
    _subscriptions: Vec<Subscription>,
}

impl TimberSidebar {
    pub fn new(
        multi_workspace: Entity<MultiWorkspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let executor = cx.background_executor().clone();
        let mut sidebar = Self::with_loader(
            multi_workspace,
            Arc::new(move |cwd| {
                timber_cli::list_worktrees(
                    timber_cli::command(),
                    cwd,
                    executor.timer(timber_cli::TIMEOUT),
                )
                .boxed()
            }),
            cx,
        );
        sidebar.input = Some(cx.new(|cx| {
            let mut input = Editor::single_line(window, cx);
            input.set_placeholder_text("Ticket or branch name", window, cx);
            input
        }));
        let operations = sidebar.operations.clone();
        sidebar
            ._subscriptions
            .push(cx.observe_in(&operations, window, |this, _, window, cx| {
                this.apply_completed_operations(window, cx);
                cx.notify();
            }));
        sidebar.listen_for_terminal_handoffs(window, cx);
        sidebar.monitor_terminal_commands(cx);
        sidebar
    }

    fn with_loader(
        multi_workspace: Entity<MultiWorkspace>,
        loader: InventoryLoader,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.subscribe(&multi_workspace, |this, _, event, cx| match event {
            MultiWorkspaceEvent::ActiveWorkspaceChanged { .. } => {
                this.activation_generation
                    .set(this.activation_generation.get().wrapping_add(1));
                this.cancel_pending_handoff();
                this.sync_handoff_projects(cx);
                Self::schedule_sync(cx);
            }
            MultiWorkspaceEvent::WorkspaceAdded(_) | MultiWorkspaceEvent::WorkspaceRemoved(_) => {
                this.sync_handoff_projects(cx);
            }
            MultiWorkspaceEvent::ProjectGroupsChanged => {}
        });
        Self::schedule_sync(cx);
        let executor = cx.background_executor().clone();
        let operations = TimberOperations::global(cx);
        Self {
            multi_workspace: multi_workspace.downgrade(),
            focus_handle: cx.focus_handle(),
            width: px(262.),
            cwd: None,
            inventory: None,
            selected_common_git_dir: None,
            retained: HashMap::new(),
            error: None,
            loading: false,
            generation: Rc::new(Cell::new(0)),
            activation_generation: Rc::new(Cell::new(0)),
            loader,
            verifier: Arc::new(move |path, common_git_dir, canonical_path| {
                let timer = executor.timer(timber_cli::TIMEOUT);
                async move {
                    timber_cli::verify_worktree(&path, &common_git_dir, &canonical_path, timer)
                        .await
                }
                .boxed()
            }),
            refresh_task: None,
            refresh_pending: false,
            debounce_task: None,
            inventory_watch: None,
            watched_paths: Vec::new(),
            terminal_monitor: None,
            git_subscription: None,
            activation_tasks: Vec::new(),
            handoff_task: None,
            handoff_listener: None,
            handoff_binding: None,
            project_subscription: None,
            operations,
            handled_operations: HashSet::new(),
            input: None,
            form_open: false,
            show_paths: false,
            expanded_operations: HashSet::new(),
            _subscriptions: vec![subscription],
        }
    }

    fn toggle_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.form_open = !self.form_open;
        if self.form_open {
            if let Some(input) = &self.input {
                window.focus(&input.focus_handle(cx), cx);
            }
        } else {
            window.focus(&self.focus_handle, cx);
        }
        cx.notify();
    }

    fn open_cleanup(&mut self, tree: WorktreeRecord, window: &mut Window, cx: &mut Context<Self>) {
        let result = (|| {
            let inventory = self
                .inventory
                .as_ref()
                .context("Refresh worktrees before cleanup")?;
            anyhow::ensure!(
                !tree.is_primary && tree.path_exists && tree.canonical_path.is_some(),
                "Select a verified linked worktree"
            );
            let primary = inventory
                .worktrees
                .iter()
                .find(|tree| tree.is_primary)
                .context("Primary worktree is unavailable")?;
            let workspace = self
                .multi_workspace
                .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())?;
            let request = OperationRequest {
                common_git_dir: inventory.repository.common_git_dir.clone(),
                cwd: primary.path.clone(),
                canonical_cwd: primary
                    .canonical_path
                    .clone()
                    .context("Primary worktree is not verified")?,
                reconciliation_cwd: primary.path.clone(),
                command: MutationCommand::Done,
                cleanup: None,
                input: tree
                    .path
                    .to_str()
                    .context("Worktree path is not UTF-8")?
                    .to_owned(),
                sidebar: cx.entity_id(),
                generation: self.activation_generation.get(),
                workspace: workspace.downgrade(),
                window: window
                    .window_handle()
                    .downcast::<MultiWorkspace>()
                    .context("Not a workspace window")?,
            };
            let sidebar = cx.weak_entity();
            let operations = self.operations.clone();
            let multi_workspace = self.multi_workspace.clone();
            workspace.update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, move |window, cx| {
                    crate::timber_delete::CleanupModal::new(
                        sidebar,
                        multi_workspace,
                        operations,
                        request,
                        tree,
                        window,
                        cx,
                    )
                });
            });
            Ok::<_, anyhow::Error>(())
        })();
        self.error = result.err().map(|error| format!("{error:#}").into());
        cx.notify();
    }

    pub(crate) fn retain_cleanup_editors(
        &mut self,
        tree: &WorktreeRecord,
        repository: &Path,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        anyhow::ensure!(
            self.selected_common_git_dir.as_deref() == Some(repository),
            "Selected repository changed. Close this dialog and select the worktree again."
        );
        let inventory = self
            .inventory
            .as_ref()
            .context("Worktree inventory is unavailable")?;
        anyhow::ensure!(
            inventory
                .worktrees
                .iter()
                .any(|current| current.path == tree.path
                    && current.canonical_path == tree.canonical_path
                    && current.head == tree.head
                    && current.branch == tree.branch),
            "Worktree changed. Close this dialog, refresh worktrees, and try again."
        );
        let workspaces = self.multi_workspace.read_with(cx, |multi_workspace, _| {
            multi_workspace.workspaces().cloned().collect::<Vec<_>>()
        })?;
        for workspace in workspaces {
            let roots = workspace.read(cx).root_paths(cx);
            let contains_target = roots.iter().any(|root| {
                tree.path.as_path() == root.as_ref()
                    || tree.canonical_path.as_deref() == Some(root.as_ref())
            });
            anyhow::ensure!(
                !contains_target || roots.len() == 1,
                "Open this worktree in its own workspace before removing it; other folders and their editors must stay open"
            );
            if roots.len() == 1
                && roots.first().is_some_and(|root| {
                    tree.path.as_path() == root.as_ref()
                        || tree.canonical_path.as_deref() == Some(root.as_ref())
                })
            {
                self.retained.insert(
                    workspace.entity_id(),
                    RetainedWorktree {
                        roots: PathList::new(&roots),
                        workspace: workspace.downgrade(),
                        common_git_dir: repository.to_path_buf(),
                        tree: tree.clone(),
                    },
                );
            }
        }
        self.multi_workspace.update(cx, |multi_workspace, cx| {
            multi_workspace.retain_active_workspace(cx)
        })?;
        Ok(())
    }

    fn submit_operation(
        &mut self,
        command: MutationCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = self
            .input
            .as_ref()
            .map(|input| input.read(cx).text(cx))
            .unwrap_or_default();
        self.start_operation(command, input, window, cx);
    }

    fn start_operation(
        &mut self,
        command: MutationCommand,
        input: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let result = (|| {
            anyhow::ensure!(
                !input.trim().is_empty(),
                "Enter a ticket identifier or branch name."
            );
            let inventory = self
                .inventory
                .as_ref()
                .context("Refresh the repository before starting an operation")?;
            let (workspace, cwd) = self.multi_workspace.read_with(cx, |multi_workspace, cx| {
                let workspace = multi_workspace.workspace();
                (
                    workspace.downgrade(),
                    workspace
                        .read(cx)
                        .root_paths(cx)
                        .first()
                        .map(|path| path.to_path_buf()),
                )
            })?;
            let cwd = cwd.context("Open a local repository first")?;
            let canonical_cwd = inventory
                .worktrees
                .iter()
                .find(|tree| tree.path == cwd || tree.canonical_path.as_ref() == Some(&cwd))
                .and_then(|tree| tree.canonical_path.clone())
                .context("Repository path is not verified; refresh before creating a worktree")?;
            let request = OperationRequest {
                common_git_dir: inventory.repository.common_git_dir.clone(),
                cwd,
                canonical_cwd,
                reconciliation_cwd: inventory.repository.primary_worktree_root.clone(),
                command,
                cleanup: None,
                input,
                sidebar: cx.entity_id(),
                generation: self.activation_generation.get(),
                workspace,
                window: window
                    .window_handle()
                    .downcast::<MultiWorkspace>()
                    .context("Not a workspace window")?,
            };
            self.operations
                .update(cx, |operations, cx| operations.start(request, cx))?;
            Ok::<_, anyhow::Error>(())
        })();
        self.error = result.err().map(|error| format!("{error:#}").into());
        cx.notify();
    }

    fn apply_completed_operations(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let completed = self
            .operations
            .read(cx)
            .operations
            .iter()
            .filter(|operation| {
                operation.request.sidebar == cx.entity_id()
                    && operation.phase == OperationPhase::Finished
                    && !self.handled_operations.contains(&operation.id)
            })
            .cloned()
            .collect::<Vec<_>>();
        for operation in completed {
            self.handled_operations.insert(operation.id);
            if operation.request.command == MutationCommand::Done
                && operation.error.is_none()
                && operation
                    .result
                    .as_ref()
                    .is_some_and(|result| result.outcome == MutationOutcome::Succeeded)
                && operation.inventory.as_ref().is_some_and(|inventory| {
                    inventory.repository.common_git_dir == operation.request.common_git_dir
                        && !inventory.worktrees.iter().any(|tree| {
                            tree.path.to_str() == Some(operation.request.input.as_str())
                        })
                })
            {
                self.forget_completed_worktree(&operation, window, cx);
            }
            if self.selected_common_git_dir.as_ref() != Some(&operation.request.common_git_dir) {
                continue;
            }
            if let Some(inventory) = operation.inventory.clone() {
                // Cancel an older listing so it cannot overwrite post-mutation inventory.
                self.generation.set(self.generation.get().wrapping_add(1));
                self.refresh_task = None;
                self.loading = false;
                self.inventory = Some(inventory);
                self.watch_inventory(cx);
                if self.refresh_pending {
                    self.schedule_inventory_refresh(cx);
                }
            }
            self.error = operation.error.as_deref().map(display_text).map(Into::into);
            if operation.request.generation != self.activation_generation.get() {
                continue;
            }
            if operation.request.command != MutationCommand::Done
                && operation
                    .result
                    .as_ref()
                    .is_some_and(|result| result.outcome == MutationOutcome::Succeeded)
            {
                self.open_operation_result(&operation, window, cx);
            }
        }
    }

    fn forget_completed_worktree(
        &mut self,
        operation: &Operation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(preview) = &operation.request.cleanup else {
            return;
        };
        let tree = preview.tree.clone();
        let primary = operation
            .inventory
            .as_ref()
            .and_then(|inventory| inventory.worktrees.iter().find(|tree| tree.is_primary))
            .cloned();
        let multi_workspace = self.multi_workspace.clone();
        cx.spawn_in(window, async move |sidebar, cx| {
            let result = async {
                let targets = multi_workspace.read_with(cx, |multi_workspace, cx| {
                    multi_workspace
                        .workspaces()
                        .filter(|workspace| {
                            let roots = workspace.read(cx).root_paths(cx);
                            roots.len() == 1
                                && roots.first().is_some_and(|root| {
                                    root.as_ref() == tree.path.as_path()
                                        || Some(root.as_ref()) == tree.canonical_path.as_deref()
                                })
                        })
                        .cloned()
                        .collect::<Vec<_>>()
                })?;
                let active_is_target = multi_workspace.read_with(cx, |multi_workspace, _| {
                    targets.contains(multi_workspace.workspace())
                })?;
                if active_is_target && let Some(primary) = primary {
                    let paths = PathList::new(&[primary.path]);
                    let opening =
                        multi_workspace.update_in(cx, |multi_workspace, window, cx| {
                            if multi_workspace
                                .workspace_for_paths(&paths, None, cx)
                                .is_some()
                            {
                                None
                            } else {
                                Some(multi_workspace.find_or_create_local_workspace(
                                    paths,
                                    None,
                                    None,
                                    OpenMode::Add,
                                    None,
                                    window,
                                    cx,
                                ))
                            }
                        })?;
                    if let Some(opening) = opening {
                        opening.await?;
                    }
                }
                let removing = multi_workspace.update_in(cx, |multi_workspace, window, cx| {
                    multi_workspace.remove(
                        targets.clone(),
                        workspace::RemovalIntent::DeletedWorktree,
                        window,
                        cx,
                    )
                })?;
                anyhow::ensure!(
                    targets.is_empty() || removing.await?,
                    "Workspace roots changed; editors were kept"
                );
                sidebar.update(cx, |sidebar, cx| {
                    sidebar.retained.retain(|id, _| {
                        !targets.iter().any(|workspace| workspace.entity_id() == *id)
                    });
                    cx.notify();
                })?;
                Ok::<_, anyhow::Error>(())
            }
            .await;
            if let Err(error) = result {
                sidebar
                    .update(cx, |sidebar, cx| {
                        sidebar.error = Some(
                            format!(
                                "Worktree removed, but its editor could not be closed: {error:#}"
                            )
                            .into(),
                        );
                        cx.notify();
                    })
                    .log_err();
            }
        })
        .detach();
    }

    fn operation_target(&self, operation: &Operation) -> Result<WorktreeRecord> {
        anyhow::ensure!(
            operation.request.command != MutationCommand::Done,
            "Removed worktrees cannot be reopened as operation results"
        );
        anyhow::ensure!(
            operation.phase == OperationPhase::Finished,
            "Refresh operation worktrees first"
        );
        anyhow::ensure!(
            self.selected_common_git_dir.as_ref() == Some(&operation.request.common_git_dir),
            "Return to the operation's repository to open its result"
        );
        let result = operation.result.as_ref().context(
            "No verified target was returned; inspect refreshed worktrees before retrying",
        )?;
        let target = result
            .worktree
            .as_ref()
            .context("Timber did not return a worktree target")?;
        let inventory = operation
            .inventory
            .as_ref()
            .context("Operation inventory is unavailable")?;
        let tree = inventory
            .worktrees
            .iter()
            .find(|tree| {
                tree.path == target.path
                    && tree.canonical_path.is_some()
                    && tree.canonical_path == target.canonical_path
            })
            .context("The returned worktree is not available in refreshed inventory")?;
        Ok(tree.clone())
    }

    fn open_operation_result(
        &mut self,
        operation: &Operation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.operation_target(operation) {
            Ok(tree) => {
                let form_input = operation
                    .result
                    .as_ref()
                    .filter(|result| {
                        result.outcome == MutationOutcome::Succeeded && operation.error.is_none()
                    })
                    .map(|_| operation.request.input.clone());
                self.activate_worktree_with_input(
                    tree,
                    None,
                    form_input,
                    Some(operation.id),
                    window,
                    cx,
                );
            }
            Err(error) => {
                self.error = Some(format!("{error:#}").into());
                cx.notify();
            }
        }
    }

    fn render_operations(&self, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let operations = self
            .operations
            .read(cx)
            .operations
            .iter()
            .filter(|operation| {
                Some(&operation.request.common_git_dir) == self.selected_common_git_dir.as_ref()
            })
            .rev()
            .take(5)
            .cloned()
            .collect::<Vec<_>>();
        div()
            .id("timber-operations")
            .max_h(px(200.))
            .flex()
            .flex_col()
            .flex_none()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .child(
                h_flex().h(px(34.)).flex_none().px_3().child(
                    Label::new("OPERATIONS")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
            )
            .child(
                v_flex()
                    .id("timber-operations-list")
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_1()
                    .pb_2()
                    .when(operations.is_empty(), |this| {
                        this.child(
                            div()
                                .px_2()
                                .text_xs()
                                .text_color(cx.theme().colors().text_muted)
                                .child("No operations yet."),
                        )
                    })
                    .children(operations.into_iter().map(|operation| {
                        let id = operation.id;
                        let target_available = self.operation_target(&operation).is_ok();
                        let failed = operation
                            .result
                            .as_ref()
                            .is_some_and(|result| result.outcome == MutationOutcome::Failed);
                        let label = format!(
                            "timber {} {}",
                            operation.request.command.name(),
                            display_text(&operation.request.input)
                        );
                        let expanded = self.expanded_operations.contains(&id);
                        let (state, color, icon) = match operation.phase {
                            OperationPhase::Running => {
                                ("Running", Color::Accent, IconName::ArrowCircle)
                            }
                            OperationPhase::Reconciling => {
                                ("Refreshing", Color::Accent, IconName::RefreshTitle)
                            }
                            OperationPhase::NeedsRefresh => {
                                ("Refresh needed", Color::Warning, IconName::Warning)
                            }
                            OperationPhase::Finished if operation.status() == "Completed" => {
                                ("Completed", Color::Success, IconName::Check)
                            }
                            OperationPhase::Finished if operation.status() == "Cancelled" => {
                                ("Cancelled", Color::Muted, IconName::Dash)
                            }
                            OperationPhase::Finished => ("Failed", Color::Error, IconName::Warning),
                        };
                        let result_operation = operation.clone();
                        let recovery_operation = operation.clone();
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                ButtonLike::new(("timber-operation", id))
                                    .full_width()
                                    .auto_height()
                                    .tab_index(0_isize)
                                    .aria_label(format!("{label}: {}", operation.status()))
                                    .aria_expanded(expanded)
                                    .tooltip({
                                        let label = label.clone();
                                        let status = operation.status();
                                        move |_, cx| {
                                            Tooltip::simple(format!("{label}: {status}"), cx)
                                        }
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if !this.expanded_operations.remove(&id) {
                                            this.expanded_operations.insert(id);
                                        }
                                        cx.notify();
                                    }))
                                    .child(
                                        h_flex()
                                            .w_full()
                                            .min_w_0()
                                            .gap_1()
                                            .py_1()
                                            .child(
                                                Icon::new(if expanded {
                                                    IconName::ChevronDown
                                                } else {
                                                    IconName::ChevronRight
                                                })
                                                .size(IconSize::XSmall)
                                                .color(Color::Muted),
                                            )
                                            .child(
                                                Icon::new(icon).size(IconSize::XSmall).color(color),
                                            )
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .text_left()
                                                    .text_xs()
                                                    .font_buffer(cx)
                                                    .truncate()
                                                    .child(label),
                                            )
                                            .child(
                                                Label::new(state)
                                                    .size(LabelSize::XSmall)
                                                    .color(color),
                                            ),
                                    ),
                            )
                            .when(expanded, |this| {
                                this.child(
                                    v_flex()
                                        .px_2()
                                        .pb_1()
                                        .gap_1()
                                        .child(
                                            Label::new(operation.status())
                                                .size(LabelSize::XSmall)
                                                .color(color),
                                        )
                                        .when_some(
                                            operation
                                                .error
                                                .clone()
                                                .or_else(|| operation.reconciliation_error.clone()),
                                            |this, error| {
                                                this.child(
                                                    div().text_xs().child(display_text(&error)),
                                                )
                                            },
                                        )
                                        .child(
                                            div()
                                                .flex()
                                                .flex_wrap()
                                                .gap_1()
                                                .child(
                                                    Button::new(("timber-output", id), "Output")
                                                        .size(ButtonSize::Compact)
                                                        .tab_index(0_isize)
                                                        .disabled(operation.view.is_none())
                                                        .on_click(cx.listener(
                                                            move |this, _, window, cx| {
                                                                let result =
                                                                    this.operations.update(
                                                                        cx,
                                                                        |operations, cx| {
                                                                            operations.output(
                                                                                id, window, cx,
                                                                            )
                                                                        },
                                                                    );
                                                                if let Err(error) = result {
                                                                    this.error = Some(
                                                                        format!("{error:#}").into(),
                                                                    );
                                                                    cx.notify();
                                                                }
                                                            },
                                                        )),
                                                )
                                                .when(
                                                    operation.phase == OperationPhase::NeedsRefresh,
                                                    |this| {
                                                        this.child(
                                                            Button::new(
                                                                ("timber-reconcile", id),
                                                                "Refresh",
                                                            )
                                                            .size(ButtonSize::Compact)
                                                            .tab_index(0_isize)
                                                            .on_click(cx.listener(
                                                                move |this, _, _, cx| {
                                                                    this.operations.update(
                                                                        cx,
                                                                        |operations, cx| {
                                                                            operations
                                                                                .reconcile(id, cx)
                                                                        },
                                                                    );
                                                                },
                                                            )),
                                                        )
                                                    },
                                                )
                                                .when(target_available, |this| {
                                                    this.child(
                                                        Button::new(
                                                            ("timber-open-result", id),
                                                            if failed {
                                                                "Open Existing"
                                                            } else {
                                                                "Open Result"
                                                            },
                                                        )
                                                        .size(ButtonSize::Compact)
                                                        .tab_index(0_isize)
                                                        .on_click(cx.listener(
                                                            move |this, _, window, cx| {
                                                                this.open_operation_result(
                                                                    &result_operation,
                                                                    window,
                                                                    cx,
                                                                );
                                                            },
                                                        )),
                                                    )
                                                })
                                                .when(failed && target_available, |this| {
                                                    this.child(
                                                        Button::new(
                                                            ("timber-retry-setup", id),
                                                            "Retry Setup",
                                                        )
                                                        .size(ButtonSize::Compact)
                                                        .tab_index(0_isize)
                                                        .disabled(
                                                            self.selected_common_git_dir
                                                                .as_ref()
                                                                .is_some_and(|repository| {
                                                                    self.operations
                                                                        .read(cx)
                                                                        .busy(repository)
                                                                }),
                                                        )
                                                        .on_click(cx.listener(
                                                            move |this, _, window, cx| {
                                                                let result = this
                                                                    .operation_target(
                                                                        &recovery_operation,
                                                                    )
                                                                    .and_then(|tree| {
                                                                        tree.path
                                                        .to_str()
                                                        .map(ToOwned::to_owned)
                                                        .context("Worktree path is not UTF-8")
                                                                    });
                                                                match result {
                                                                    Ok(path) => this
                                                                        .start_operation(
                                                                        MutationCommand::RetrySetup,
                                                                        path,
                                                                        window,
                                                                        cx,
                                                                    ),
                                                                    Err(error) => {
                                                                        this.error = Some(
                                                                            format!("{error:#}")
                                                                                .into(),
                                                                        );
                                                                        cx.notify();
                                                                    }
                                                                }
                                                            },
                                                        )),
                                                    )
                                                }),
                                        ),
                                )
                            })
                    })),
            )
    }

    fn listen_for_terminal_handoffs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        #[cfg(unix)]
        {
            match terminal::timber_handoff::WindowBridge::open() {
                Ok(bridge) => {
                    match std::env::current_exe() {
                        Ok(command) => {
                            self.handoff_binding = Some(HandoffBinding {
                                command,
                                socket: bridge.endpoint(),
                                project_ids_by_workspace: HashMap::new(),
                            });
                            self.sync_handoff_projects(cx);
                        }
                        Err(error) => {
                            self.error =
                                Some(format!("Cannot locate TAXE executable: {error:#}").into());
                        }
                    }
                    self._subscriptions
                        .push(cx.on_release(|this, cx| this.release_handoff_binding(cx)));
                    self.handoff_listener = Some(cx.spawn_in(window, async move |this, cx| {
                        loop {
                            match bridge.receive().await {
                                Ok(handoff) => {
                                    this.update_in(cx, |this, window, cx| {
                                        this.open_from_terminal(
                                            handoff.path,
                                            handoff.terminal,
                                            window,
                                            cx,
                                        )
                                    })
                                    .log_err();
                                }
                                Err(error) => {
                                    this.update(cx, |this, cx| {
                                        this.error = Some(
                                            format!("Timber terminal handoff failed: {error:#}")
                                                .into(),
                                        );
                                        cx.notify();
                                    })
                                    .log_err();
                                    break;
                                }
                            }
                        }
                    }));
                }
                Err(error) => {
                    self.error =
                        Some(format!("Cannot enable Timber terminal handoff: {error:#}").into())
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _window = window;
            let _cx = cx;
        }
    }

    fn sync_handoff_projects(&mut self, cx: &mut App) {
        let Some(binding) = self.handoff_binding.as_ref() else {
            return;
        };
        let command = binding.command.clone();
        let socket = binding.socket.clone();
        let Ok(current) = self.multi_workspace.read_with(cx, |multi_workspace, cx| {
            multi_workspace
                .workspaces()
                .map(|workspace| {
                    (
                        workspace.entity_id(),
                        workspace.read(cx).project().entity_id().as_u64(),
                    )
                })
                .collect::<HashMap<_, _>>()
        }) else {
            return;
        };
        let Some(binding) = self.handoff_binding.as_mut() else {
            return;
        };
        let mut unbound = Vec::new();
        binding
            .project_ids_by_workspace
            .retain(|workspace_id, project_id| {
                if current.get(workspace_id) == Some(project_id) {
                    true
                } else {
                    unbound.push(*project_id);
                    false
                }
            });
        let mut bound = Vec::new();
        for (workspace_id, project_id) in current {
            if binding
                .project_ids_by_workspace
                .insert(workspace_id, project_id)
                .is_none()
            {
                bound.push(project_id);
            }
        }
        for project_id in unbound {
            terminal::timber_handoff::unbind_project(project_id, cx);
        }
        for project_id in bound {
            terminal::timber_handoff::bind_project(project_id, command.clone(), socket.clone(), cx);
        }
    }

    fn start_operation_agent(
        &mut self,
        operation_id: Option<u64>,
        workspace: Entity<Workspace>,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(operation_id) = operation_id else {
            return;
        };
        let Some(path_argument) = path.to_str().map(ToOwned::to_owned) else {
            self.error = Some(
                "Opened the worktree, but its path cannot be passed to Timber for agent launch."
                    .into(),
            );
            cx.notify();
            return;
        };
        let backend = timber_cli::command();
        let Some(backend) = backend.to_str() else {
            self.error = Some("The Timber worktree CLI path is not UTF-8.".into());
            cx.notify();
            return;
        };
        if !self.operations.update(cx, |operations, _| {
            operations.claim_agent_launch(operation_id)
        }) {
            return;
        }
        // Reopening an exact existing path resolves the saved workflow and agent in
        // Timber without rerunning setup or invoking another editor.
        let command = vec![
            backend.to_owned(),
            "open".to_owned(),
            "--agent".to_owned(),
            "--no-editor".to_owned(),
            "--".to_owned(),
            path_argument,
        ];
        let operations = self.operations.clone();
        cx.spawn_in(window, async move |sidebar, cx| {
            if !open_agent_terminal(workspace, path, command, cx).await {
                operations.update(cx, |operations, _| {
                    operations.release_agent_launch(operation_id)
                });
                report_missing_agent_terminal(&sidebar, cx);
            }
        })
        .detach();
    }

    fn queue_agent_terminal(
        workspace: Entity<Workspace>,
        cwd: PathBuf,
        command: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |sidebar, cx| {
            if !open_agent_terminal(workspace, cwd, command, cx).await {
                report_missing_agent_terminal(&sidebar, cx);
            }
        })
        .detach();
    }

    fn release_handoff_binding(&mut self, cx: &mut App) {
        if let Some(binding) = self.handoff_binding.take() {
            for project_id in binding.project_ids_by_workspace.into_values() {
                terminal::timber_handoff::unbind_project(project_id, cx);
            }
        }
    }

    fn open_from_terminal(
        &mut self,
        path: PathBuf,
        terminal_command: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.invalidate();
        self.refresh_task = None;
        self.handoff_task = None;
        self.loading = true;
        self.error = None;
        let generation = self.generation.get();
        let activation_generation = self.activation_generation.get();
        let loader = self.loader.clone();
        let task = cx.background_spawn({
            let path = path.clone();
            async move { loader(path).await }
        });
        self.handoff_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| {
                if this.generation.get() != generation
                    || this.activation_generation.get() != activation_generation
                {
                    return;
                }
                this.loading = false;
                match result.and_then(|inventory| {
                    let tree = inventory
                        .worktrees
                        .iter()
                        .find(|tree| tree.path == path)
                        .cloned()
                        .context("Timber selected a path absent from the verified inventory")?;
                    Ok((inventory, tree))
                }) {
                    Ok((inventory, tree)) => {
                        this.cwd = Some(path);
                        this.selected_common_git_dir =
                            Some(inventory.repository.common_git_dir.clone());
                        this.inventory = Some(inventory);
                        this.watch_inventory(cx);
                        if this.refresh_pending {
                            this.schedule_inventory_refresh(cx);
                        }
                        this.activate_worktree(tree, Some(terminal_command), window, cx);
                    }
                    Err(error) => {
                        this.error =
                            Some(format!("Timber worktree handoff failed: {error:#}").into())
                    }
                }
                if this.refresh_pending {
                    this.schedule_inventory_refresh(cx);
                }
                cx.notify();
            })
            .log_err();
        }));
        cx.notify();
    }

    fn cancel_pending_handoff(&mut self) {
        if self.loading
            && self
                .handoff_task
                .as_ref()
                .is_some_and(|task| !task.is_ready())
        {
            self.handoff_task = None;
            self.loading = self
                .refresh_task
                .as_ref()
                .is_some_and(|task| !task.is_ready());
        }
    }

    fn schedule_sync(cx: &mut Context<Self>) {
        let sidebar = cx.weak_entity();
        cx.defer(move |cx| {
            sidebar
                .update(cx, |sidebar, cx| sidebar.sync_repository(cx))
                .log_err();
        });
    }

    fn invalidate(&mut self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        self.activation_generation
            .set(self.activation_generation.get().wrapping_add(1));
    }

    fn sync_repository(&mut self, cx: &mut Context<Self>) {
        let selection = self.multi_workspace.read_with(cx, |multi_workspace, cx| {
            let workspace = multi_workspace.workspace().clone();
            let project = workspace.read(cx).project().clone();
            let cwd = project
                .read(cx)
                .is_local()
                .then(|| {
                    workspace
                        .read(cx)
                        .root_paths(cx)
                        .first()
                        .map(|path| path.to_path_buf())
                })
                .flatten();
            (workspace, project, cwd)
        });
        let Ok((workspace, project, cwd)) = selection else {
            return;
        };
        self.project_subscription = Some(cx.subscribe(&project, |this, _, event, cx| {
            if matches!(
                event,
                project::Event::WorktreeAdded(_)
                    | project::Event::WorktreeRemoved(_)
                    | project::Event::WorktreeOrderChanged
                    | project::Event::WorktreePathsChanged { .. }
            ) {
                this.activation_generation
                    .set(this.activation_generation.get().wrapping_add(1));
                this.cancel_pending_handoff();
                Self::schedule_sync(cx);
            }
        }));
        let git_store = project.read(cx).git_store().clone();
        self.git_subscription = Some(cx.subscribe(&git_store, |this, _, event, cx| {
            use project::git_store::{GitStoreEvent, RepositoryEvent};
            if matches!(
                event,
                GitStoreEvent::RepositoryAdded
                    | GitStoreEvent::RepositoryRemoved(_)
                    | GitStoreEvent::RepositoryUpdated(
                        _,
                        RepositoryEvent::GitWorktreeListChanged,
                        _
                    )
            ) {
                this.schedule_inventory_refresh(cx);
            }
        }));
        self.forget_changed_workspaces(cx);
        let same_repository = self.inventory.as_ref().is_some_and(|inventory| {
            self.retained
                .get(&workspace.entity_id())
                .is_some_and(|retained| {
                    retained.common_git_dir == inventory.repository.common_git_dir
                })
                || cwd.as_ref().is_some_and(|cwd| {
                    inventory
                        .worktrees
                        .iter()
                        .any(|tree| tree.path == *cwd || tree.canonical_path.as_ref() == Some(cwd))
                })
        });
        if self.cwd == cwd || same_repository {
            cx.notify();
            return;
        }
        self.invalidate();
        self.refresh_task = None;
        self.inventory = None;
        self.inventory_watch = None;
        self.watched_paths.clear();
        self.debounce_task = None;
        self.refresh_pending = false;
        self.selected_common_git_dir = self
            .retained
            .get(&workspace.entity_id())
            .map(|retained| retained.common_git_dir.clone());
        self.cwd = cwd;
        self.error = None;
        self.loading = false;
        if self.cwd.is_some() {
            self.refresh(cx);
        } else {
            cx.notify();
        }
    }

    fn schedule_inventory_refresh(&mut self, cx: &mut Context<Self>) {
        if self.cwd.is_none() {
            return;
        }
        self.refresh_pending = true;
        if self.loading
            || self
                .debounce_task
                .as_ref()
                .is_some_and(|task| !task.is_ready())
        {
            return;
        }
        let cwd = self.cwd.clone();
        let repository = self.selected_common_git_dir.clone();
        self.debounce_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(300))
                .await;
            this.update(cx, |this, cx| {
                this.debounce_task = None;
                if this.cwd == cwd && this.selected_common_git_dir == repository && !this.loading {
                    this.refresh(cx);
                }
            })
            .log_err();
        }));
    }

    fn watch_inventory(&mut self, cx: &mut Context<Self>) {
        let Some(inventory) = &self.inventory else {
            return;
        };
        let repository = inventory.repository.common_git_dir.clone();
        let mut roots: Vec<_> = inventory
            .worktrees
            .iter()
            .map(|tree| tree.path.clone())
            .collect();
        roots.extend(
            inventory
                .worktrees
                .iter()
                .filter_map(|tree| tree.canonical_path.clone()),
        );
        roots.sort();
        roots.dedup();
        let mut paths = vec![repository.clone(), repository.join("worktrees")];
        paths.extend(roots.iter().cloned());
        if cfg!(target_os = "linux") {
            paths.extend(
                roots
                    .iter()
                    .filter_map(|path| path.parent().map(Path::to_path_buf)),
            );
        }
        paths.sort();
        paths.dedup();
        if self.watched_paths == paths {
            return;
        }
        self.watched_paths = paths.clone();
        let filesystem = self.multi_workspace.read_with(cx, |multi_workspace, cx| {
            multi_workspace
                .workspace()
                .read(cx)
                .project()
                .read(cx)
                .fs()
                .clone()
        });
        let Ok(filesystem) = filesystem else {
            return;
        };
        let registration = cx.background_spawn({
            let repository = repository.clone();
            async move {
                let (events, watcher) = filesystem
                    .watch(&repository, Duration::from_millis(100))
                    .await;
                let mut errors = Vec::new();
                for path in paths {
                    // Poll watchers scan recursively at registration; never scan sibling worktrees.
                    if !roots.contains(&path)
                        && path != repository
                        && path != repository.join("worktrees")
                        && filesystem.requires_poll_watcher(&path)
                    {
                        continue;
                    }
                    if let Err(error) = watcher.add(&path) {
                        errors.push(format!("{}: {error:#}", path.display()));
                    }
                }
                (events, watcher, roots, errors)
            }
        });
        self.inventory_watch = Some(cx.spawn(async move |this, cx| {
            let (mut events, _watcher, roots, errors) = registration.await;
            if this
                .update(cx, |this, cx| {
                    if this.selected_common_git_dir.as_ref() != Some(&repository) {
                        return;
                    }
                    if errors.is_empty() {
                        // Reconcile changes between the previous snapshot and watch registration.
                        this.schedule_inventory_refresh(cx);
                    } else {
                        this.watched_paths.clear();
                        this.error = Some(
                            format!(
                                "Unable to watch Timber worktrees: {}. Click Refresh to retry.",
                                errors.join("; ")
                            )
                            .into(),
                        );
                        cx.notify();
                    }
                })
                .is_err()
            {
                return;
            }
            while let Some(events) = events.next().await {
                if !events
                    .iter()
                    .any(|event| inventory_path_changed(&event.path, &repository, &roots))
                {
                    continue;
                }
                if this
                    .update(cx, |this, cx| {
                        if this.selected_common_git_dir.as_ref() == Some(&repository) {
                            this.schedule_inventory_refresh(cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    fn monitor_terminal_commands(&mut self, cx: &mut Context<Self>) {
        self.terminal_monitor = Some(cx.spawn(async move |this, cx| {
            let mut previous: HashMap<EntityId, Option<String>> = HashMap::new();
            let mut previous_repository = None;
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
                if this
                    .update(cx, |this, cx| {
                        if previous_repository != this.selected_common_git_dir {
                            previous.clear();
                            previous_repository = this.selected_common_git_dir.clone();
                        }
                        let mut current = HashMap::new();
                        if let Ok(workspaces) =
                            this.multi_workspace.read_with(cx, |multi_workspace, _| {
                                multi_workspace.workspaces().cloned().collect::<Vec<_>>()
                            })
                        {
                            for workspace in workspaces {
                                let project = workspace.read(cx).project();
                                if !project.read(cx).is_local() {
                                    continue;
                                }
                                let roots = workspace.read(cx).root_paths(cx);
                                let belongs =
                                    this.retained.get(&workspace.entity_id()).is_some_and(
                                        |retained| {
                                            Some(&retained.common_git_dir)
                                                == this.selected_common_git_dir.as_ref()
                                        },
                                    ) || this.inventory.as_ref().is_some_and(|inventory| {
                                        roots.iter().any(|root| {
                                            inventory.worktrees.iter().any(|tree| {
                                                tree.path.as_path() == root.as_ref()
                                                    || tree.canonical_path.as_deref()
                                                        == Some(root.as_ref())
                                            })
                                        })
                                    });
                                if !belongs {
                                    continue;
                                }
                                for terminal in project.read(cx).local_terminal_handles() {
                                    if let Some(terminal) = terminal.upgrade() {
                                        let command =
                                            terminal.read(cx).foreground_process_command_name();
                                        current.insert(terminal.entity_id(), command);
                                    }
                                }
                            }
                        }
                        // Process transitions are only hints: the CLI inventory decides what exists.
                        if previous.iter().any(|(id, command)| {
                            let backend = timber_cli::command();
                            let backend_name = backend.file_name().and_then(|name| name.to_str());
                            command
                                .as_deref()
                                .is_some_and(|name| name == "timber" || Some(name) == backend_name)
                                && current.get(id) != Some(command)
                        }) {
                            this.schedule_inventory_refresh(cx);
                        }
                        previous = current;
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.generation.set(self.generation.get().wrapping_add(1));
        self.debounce_task = None;
        self.refresh_pending = false;
        self.refresh_task = None;
        let Some(cwd) = self.cwd.clone() else {
            return;
        };
        let listing_cwd = self
            .inventory
            .as_ref()
            .map(|inventory| inventory.repository.primary_worktree_root.clone())
            .unwrap_or_else(|| cwd.clone());
        let generation = self.generation.get();
        let loader = self.loader.clone();
        self.loading = true;
        self.error = None;
        let task = cx.background_spawn(async move { loader(listing_cwd).await });
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.apply_inventory(generation, &cwd, result, cx)
            })
            .log_err();
        }));
        cx.notify();
    }

    fn apply_inventory(
        &mut self,
        generation: u64,
        cwd: &Path,
        result: Result<Inventory>,
        cx: &mut Context<Self>,
    ) {
        if generation != self.generation.get() || self.cwd.as_deref() != Some(cwd) {
            return;
        }
        self.loading = false;
        match result {
            Ok(inventory) => {
                if self
                    .selected_common_git_dir
                    .as_ref()
                    .is_some_and(|expected| expected != &inventory.repository.common_git_dir)
                {
                    self.error = Some("Repository identity changed since opening; retained editors remain available.".into());
                    cx.notify();
                    return;
                }
                self.selected_common_git_dir = Some(inventory.repository.common_git_dir.clone());
                if let Some(tree) = inventory.worktrees.iter().find(|tree| {
                    tree.path == inventory.repository.current_worktree_root
                        && tree.canonical_path.is_some()
                        && (tree.path == cwd || tree.canonical_path.as_deref() == Some(cwd))
                }) && let Ok(workspace) = self
                    .multi_workspace
                    .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
                    && workspace.read(cx).root_paths(cx).len() == 1
                {
                    self.forget_changed_workspaces(cx);
                    self.retained
                        .entry(workspace.entity_id())
                        .or_insert_with(|| RetainedWorktree {
                            roots: PathList::new(&workspace.read(cx).root_paths(cx)),
                            workspace: workspace.downgrade(),
                            common_git_dir: inventory.repository.common_git_dir.clone(),
                            tree: tree.clone(),
                        });
                }
                self.inventory = Some(inventory);
                self.error = None;
                self.watch_inventory(cx);
            }
            Err(error) => self.error = Some(format!("Unable to refresh Timber worktrees: {error:#}. Check the repository and Timber installation, then click Refresh to retry.").into()),
        }
        self.retained
            .retain(|_, retained| retained.workspace.upgrade().is_some());
        if self.refresh_pending {
            self.schedule_inventory_refresh(cx);
        }
        cx.notify();
    }

    fn forget_changed_workspaces(&mut self, cx: &App) {
        self.retained.retain(|_, retained| {
            retained.workspace.upgrade().is_some_and(|workspace| {
                PathList::new(&workspace.read(cx).root_paths(cx)) == retained.roots
            })
        });
    }

    fn recovery_rows(&self, cx: &App) -> Vec<(EntityId, WeakEntity<Workspace>, PathBuf)> {
        self.retained
            .iter()
            .filter(|(_, retained)| {
                self.cwd.is_some()
                    && self.selected_common_git_dir.as_ref() == Some(&retained.common_git_dir)
                    && retained.workspace.upgrade().is_some_and(|workspace| {
                        workspace.read(cx).items(cx).any(|item| item.is_dirty(cx))
                    })
                    && !self.inventory.as_ref().is_some_and(|inventory| {
                        inventory.worktrees.iter().any(|tree| {
                            tree.canonical_path.is_some()
                                && tree.canonical_path == retained.tree.canonical_path
                        })
                    })
            })
            .map(|(id, retained)| (*id, retained.workspace.clone(), retained.tree.path.clone()))
            .collect()
    }

    fn activate_retained(
        &mut self,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_pending_handoff();
        self.activation_generation
            .set(self.activation_generation.get().wrapping_add(1));
        self.forget_changed_workspaces(cx);
        let result = self
            .retained
            .get(&workspace.entity_id())
            .context("Retained workspace roots changed; refresh before selecting it")
            .and_then(|retained| {
                anyhow::ensure!(
                    self.selected_common_git_dir.as_ref() == Some(&retained.common_git_dir),
                    "Retained workspace belongs to another repository"
                );
                workspace
                    .upgrade()
                    .context("Retained workspace is no longer available")
            })
            .and_then(|workspace| {
                self.multi_workspace.update(cx, |multi_workspace, cx| {
                    multi_workspace.retain_active_workspace(cx);
                    multi_workspace.activate(workspace, None, window, cx);
                    multi_workspace.retain_active_workspace(cx);
                })
            });
        self.error = result.err().map(|error| format!("{error:#}").into());
        cx.notify();
    }

    fn activate_worktree(
        &mut self,
        tree: WorktreeRecord,
        terminal_command: Option<Vec<String>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.activate_worktree_with_input(tree, terminal_command, None, None, window, cx);
    }

    fn complete_form(
        &mut self,
        submitted_input: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(submitted_input) = submitted_input {
            if let Some(input) = &self.input {
                if input.read(cx).text(cx) == submitted_input {
                    input.update(cx, |input, cx| input.set_text("", window, cx));
                    self.form_open = false;
                    cx.notify();
                }
            }
        }
    }

    fn activate_worktree_with_input(
        &mut self,
        tree: WorktreeRecord,
        terminal_command: Option<Vec<String>>,
        form_input: Option<String>,
        operation_id: Option<u64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_pending_handoff();
        self.activation_generation
            .set(self.activation_generation.get().wrapping_add(1));
        self.forget_changed_workspaces(cx);
        let Some(inventory) = self.inventory.as_ref() else {
            return;
        };
        let common_git_dir = inventory.repository.common_git_dir.clone();
        if let Some(workspace) = self
            .retained
            .values()
            .find(|retained| {
                retained.common_git_dir == common_git_dir
                    && retained.tree.canonical_path.is_some()
                    && retained.tree.canonical_path == tree.canonical_path
            })
            .map(|retained| retained.workspace.clone())
        {
            self.activate_retained(workspace.clone(), window, cx);
            if self.error.is_none() {
                self.complete_form(form_input.as_deref(), window, cx);
                if let Some(workspace) = workspace.upgrade() {
                    self.start_operation_agent(
                        operation_id,
                        workspace,
                        tree.path.clone(),
                        window,
                        cx,
                    );
                }
            }
            if let Some(command) = terminal_command
                .clone()
                .filter(|command| !command.is_empty())
                && let Some(workspace) = workspace.upgrade()
            {
                Self::queue_agent_terminal(workspace, tree.path.clone(), command, window, cx);
            }
            return;
        }
        let Some(canonical_path) = tree.canonical_path.clone().filter(|_| tree.path_exists) else {
            self.error = Some("This worktree cannot be verified. Refresh or select its retained editor to recover unsaved work.".into());
            cx.notify();
            return;
        };
        self.activation_generation
            .set(self.activation_generation.get().wrapping_add(1));
        let generation = self.activation_generation.get();
        let activation_generation = self.activation_generation.clone();
        let guard: Rc<dyn Fn() -> bool> =
            Rc::new(move || activation_generation.get() == generation);
        let verifier = self.verifier.clone();
        let verification = cx.background_spawn({
            let verifier = verifier.clone();
            let path = tree.path.clone();
            let common_git_dir = common_git_dir.clone();
            let canonical_path = canonical_path.clone();
            async move { verifier(path, common_git_dir, canonical_path).await }
        });
        self.error = None;
        self.activation_tasks.retain(|task| !task.is_ready());
        self.activation_tasks
            .push(cx.spawn_in(window, async move |this, cx| {
                let result = async {
                    verification.await?;
                    anyhow::ensure!(guard(), "Worktree selection changed");
                    let validator: workspace::LocalWorkspacePathValidator = Arc::new({
                        let verifier = verifier.clone();
                        let path = tree.path.clone();
                        let common_git_dir = common_git_dir.clone();
                        let canonical_path = canonical_path.clone();
                        move |opened_paths| {
                            let verifier = verifier.clone();
                            let path = path.clone();
                            let common_git_dir = common_git_dir.clone();
                            let canonical_path = canonical_path.clone();
                            async move {
                                anyhow::ensure!(
                                    opened_paths == vec![canonical_path.clone()],
                                    "Opened roots differ from the verified worktree"
                                );
                                verifier(path, common_git_dir, canonical_path).await
                            }
                            .boxed()
                        }
                    });
                    let sidebar = this.clone();
                    let init_tree = tree.clone();
                    let init_common_git_dir = common_git_dir.clone();
                    let task = this.update_in(cx, |this, window, cx| {
                        this.multi_workspace.update(cx, |multi_workspace, cx| {
                            multi_workspace.retain_active_workspace(cx);
                            multi_workspace.find_or_create_local_workspace_guarded(
                                PathList::new(&[tree.path.clone()]),
                                None,
                                Some(Box::new(move |workspace, _, cx| {
                                    let roots = PathList::new(&workspace.root_paths(cx));
                                    let workspace = cx.entity().downgrade();
                                    sidebar
                                        .update(cx, |this, cx| {
                                            this.retained.insert(
                                                workspace.entity_id(),
                                                RetainedWorktree {
                                                    workspace,
                                                    roots,
                                                    common_git_dir: init_common_git_dir,
                                                    tree: init_tree,
                                                },
                                            );
                                            cx.notify();
                                        })
                                        .log_err();
                                })),
                                OpenMode::Activate,
                                None,
                                workspace::LocalWorkspaceOpenGuard {
                                    is_current: guard.clone(),
                                    validate_paths: Some(validator),
                                },
                                window,
                                cx,
                            )
                        })
                    })??;
                    let workspace = task.await?;
                    if let Some(command) = terminal_command.filter(|command| !command.is_empty()) {
                        let cwd = tree.path.clone();
                        let workspace = workspace.clone();
                        let sidebar = this.clone();
                        cx.spawn(async move |cx| {
                            if !open_agent_terminal(workspace, cwd, command, cx).await {
                                report_missing_agent_terminal(&sidebar, cx);
                            }
                        })
                        .detach();
                    }
                    this.update_in(cx, |this, window, cx| {
                        this.complete_form(form_input.as_deref(), window, cx);
                        this.start_operation_agent(
                            operation_id,
                            workspace.clone(),
                            tree.path.clone(),
                            window,
                            cx,
                        );
                        let roots = PathList::new(&workspace.read(cx).root_paths(cx));
                        if roots.paths().len() == 1
                            && roots.paths().first().is_some_and(|root| {
                                root.as_path() == canonical_path.as_path()
                                    || root.as_path() == tree.path.as_path()
                            })
                        {
                            this.retained
                                .entry(workspace.entity_id())
                                .or_insert_with(|| RetainedWorktree {
                                    workspace: workspace.downgrade(),
                                    roots,
                                    common_git_dir,
                                    tree,
                                });
                            cx.notify();
                        }
                    })?;
                    Ok::<_, anyhow::Error>(())
                }
                .await;
                this.update(cx, |this, cx| {
                    if this.activation_generation.get() == generation {
                        this.error = result.err().map(|error| format!("{error:#}").into());
                        cx.notify();
                    }
                })
                .log_err();
            }));
    }
}

fn report_missing_agent_terminal(
    sidebar: &WeakEntity<TimberSidebar>,
    cx: &mut gpui::AsyncWindowContext,
) {
    sidebar
        .update(cx, |sidebar, cx| {
            sidebar.error =
                Some("The agent command could not start or exited unsuccessfully. Inspect its terminal output; Open Result can retry the launch.".into());
            cx.notify();
        })
        .log_err();
}

async fn open_agent_terminal(
    workspace: Entity<Workspace>,
    cwd: PathBuf,
    mut command: Vec<String>,
    cx: &mut gpui::AsyncWindowContext,
) -> bool {
    let Some(program) = command.first().cloned() else {
        return true;
    };
    let args = command.split_off(1);
    let command_label = std::iter::once(program.as_str())
        .chain(args.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ");
    for _attempt in 0..100 {
        let ready = workspace.update(cx, |workspace, _| workspace.has_terminal_provider());
        if !ready {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(100))
                .await;
            continue;
        }
        let task = workspace.update_in(cx, |workspace, window, cx| {
            workspace.spawn_in_terminal(
                task::SpawnInTerminal {
                    id: task::TaskId(format!("timber-agent-{command_label}-{cwd:?}")),
                    full_label: command_label.clone(),
                    label: program.clone(),
                    command: Some(program.clone()),
                    args: args.clone(),
                    command_label: command_label.clone(),
                    cwd: Some(cwd.clone()),
                    env: HashMap::default(),
                    use_new_terminal: true,
                    allow_concurrent_runs: true,
                    reveal: task::RevealStrategy::Always,
                    reveal_target: task::RevealTarget::Dock,
                    hide: task::HideStrategy::Never,
                    shell: task::Shell::System,
                    show_summary: false,
                    show_command: true,
                    show_rerun: false,
                    save: task::SaveStrategy::None,
                },
                window,
                cx,
            )
        });
        return match task {
            Ok(task) => match task.await {
                Some(Ok(status)) => status.success(),
                Some(Err(error)) => {
                    log::error!("Timber agent terminal failed: {error:#}");
                    false
                }
                None => false,
            },
            Err(error) => {
                log::error!("Could not attach Timber agent terminal: {error:#}");
                false
            }
        };
    }
    false
}

impl Focusable for TimberSidebar {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TimberSidebar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.forget_changed_workspaces(cx);
        let active_workspace = self
            .multi_workspace
            .read_with(cx, |multi_workspace, _| {
                multi_workspace.workspace().entity_id()
            })
            .ok();
        let rows = self
            .inventory
            .as_ref()
            .map(|inventory| inventory.worktrees.clone())
            .unwrap_or_default();
        let repository = self
            .inventory
            .as_ref()
            .map(|inventory| inventory.repository.primary_worktree_root.clone());
        let retained_rows = self.recovery_rows(cx);
        let busy = self
            .selected_common_git_dir
            .as_ref()
            .is_some_and(|repository| self.operations.read(cx).busy(repository));
        let operations = self.render_operations(cx);
        v_flex()
            .track_focus(&self.focus_handle)
            .size_full()
            .min_w_0()
            .bg(cx.theme().colors().surface_background)
            .child(h_flex().h(px(34.)).flex_none().pl_3().pr_2().justify_between()
                .child(div().flex_1().min_w_0().text_xs().text_color(cx.theme().colors().text_muted).truncate().child("WORKTREES"))
                .child(h_flex().flex_none().gap_1()
                    .child(IconButton::new("timber-paths", IconName::Eye)
                        .icon_size(IconSize::Small).tab_index(0_isize)
                        .aria_label("Show full worktree paths").toggle_state(self.show_paths)
                        .tooltip(Tooltip::text(if self.show_paths { "Hide full worktree paths" } else { "Show full worktree paths" }))
                        .on_click(cx.listener(|this, _, _, cx| { this.show_paths = !this.show_paths; cx.notify(); })))
                    .child(IconButton::new("timber-refresh", IconName::RefreshTitle)
                        .icon_size(IconSize::Small).tab_index(0_isize)
                        .aria_label("Refresh worktrees")
                        .tooltip(Tooltip::text(if self.loading { "Refreshing worktrees…" } else { "Refresh worktrees" }))
                        .icon_color(if self.loading { Color::Accent } else { Color::Muted })
                        .disabled(self.cwd.is_none() || self.loading)
                        .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))))
                    .child(Button::new("timber-new", "New").size(ButtonSize::Compact)
                        .style(ButtonStyle::Filled).tab_index(0_isize)
                        .start_icon(Icon::new(IconName::Plus).size(IconSize::XSmall))
                        .aria_label("New worktree").aria_expanded(self.form_open)
                        .tooltip(Tooltip::text("Create or open a worktree"))
                        .disabled(self.inventory.is_none())
                        .on_click(cx.listener(|this, _, window, cx| this.toggle_form(window, cx))))))
            .when_some(repository, |this, repository| {
                let path = display_text(&repository.display().to_string());
                let name = display_text(&repository.file_name().map(|name| name.to_string_lossy()).unwrap_or_else(|| repository.to_string_lossy()));
                this.child(h_flex().flex_none().mx_2().mb_1().px_2().py_1p5().gap_2().min_w_0()
                    .rounded_md().border_1().border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().element_background)
                    .child(Icon::new(IconName::Folder).size(IconSize::Small).color(Color::Muted))
                    .child(div().id("timber-repository").flex_1().min_w_0().text_sm().font_weight(gpui::FontWeight::SEMIBOLD).truncate().child(name)
                        .tooltip(move |_, cx| Tooltip::simple(path.clone(), cx)))
                    .child(Label::new(rows.len().to_string()).size(LabelSize::Small).color(Color::Muted)))
            })
            .when(self.form_open, |this| {
                this.child(v_flex().flex_none().mx_2().mb_2().p_2().gap_1p5().rounded_lg()
                    .border_1().border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().element_background)
                    .capture_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                        match event.keystroke.key.as_str() {
                            "escape" => { this.toggle_form(window, cx); cx.stop_propagation(); }
                            "enter" if this.input.as_ref().is_some_and(|input| input.focus_handle(cx).contains_focused(window, cx)) => {
                                this.submit_operation(MutationCommand::New, window, cx); cx.stop_propagation();
                            }
                            _ => {}
                        }
                    }))
                    .child(Label::new("Ticket or branch name").size(LabelSize::Small).color(Color::Muted))
                    .when_some(self.input.clone(), |this, input| this.child(
                        div().px_2().py_1p5().rounded_md().border_1()
                            .border_color(if input.focus_handle(cx).contains_focused(window, cx) { cx.theme().colors().border_focused } else { cx.theme().colors().border })
                            .bg(cx.theme().colors().editor_background).child(input)))
                    .child(div().text_xs().text_color(cx.theme().colors().text_muted)
                        .child("Timber resolves the ticket, runs setup, and opens your configured agent."))
                    .child(h_flex().flex_wrap().justify_end().gap_1()
                        .child(Button::new("timber-cancel", "Cancel").size(ButtonSize::Compact).tab_index(0_isize)
                            .on_click(cx.listener(|this, _, window, cx| this.toggle_form(window, cx))))
                        .child(Button::new("timber-open", "Open").size(ButtonSize::Compact).style(ButtonStyle::Filled).tab_index(0_isize).disabled(busy)
                            .on_click(cx.listener(|this, _, window, cx| this.submit_operation(MutationCommand::Open, window, cx))))
                        .child(Button::new("timber-create", "Create").size(ButtonSize::Compact).style(ButtonStyle::Tinted(ui::TintColor::Accent)).tab_index(0_isize).disabled(busy)
                            .on_click(cx.listener(|this, _, window, cx| this.submit_operation(MutationCommand::New, window, cx)))))
                    .when(busy, |this| this.child(div().text_xs().text_color(cx.theme().colors().text_accent).child("Operation running. See Operations for output."))))
            })
            .when_some(self.error.clone(), |this, error| {
                this.child(div().px_3().py_2().text_xs().text_color(cx.theme().status().error).child(error))
            })
            .when(self.cwd.is_none(), |this| {
                this.child(div().px_3().py_2().text_sm().text_color(cx.theme().colors().text_muted).child("Open a local repository to list worktrees."))
            })
            .child(v_flex().id("timber-worktree-list").flex_1().min_h_0().min_w_0().px_1p5().overflow_y_scroll()
                .children(rows.into_iter().enumerate().map(|(index, tree)| {
                    let branch = display_text(tree.branch.as_deref().unwrap_or("Detached HEAD"));
                    let label = display_text(&tree.path.file_name().map(|name| name.to_string_lossy()).unwrap_or_else(|| tree.path.to_string_lossy()));
                    let path = display_text(&tree.path.display().to_string());
                    let summary = tree.summary();
                    let selected = self.retained.iter().any(|(id, retained)| {
                        Some(*id) == active_workspace && retained.tree.canonical_path.is_some() && retained.tree.canonical_path == tree.canonical_path
                    });
                    let (status, color) = if !tree.path_exists {
                        ("Missing".to_owned(), Color::Error)
                    } else if tree.prunable.is_some() {
                        ("Prunable".to_owned(), Color::Warning)
                    } else if tree.locked.is_some() {
                        ("Locked".to_owned(), Color::Warning)
                    } else {
                        match &tree.status {
                            FileStatus::Known { modified, untracked, .. } if *modified != 0 || *untracked != 0 => (format!("{modified} modified · {untracked} untracked"), Color::Warning),
                            FileStatus::Known { .. } => ("Clean".to_owned(), Color::Success),
                            FileStatus::Unknown { .. } => ("Status unknown".to_owned(), Color::Muted),
                        }
                    };
                    let cleanup_tree = tree.clone();
                    let cleanup_disabled = tree.is_primary || !tree.path_exists || tree.canonical_path.is_none() || busy;
                    h_flex().mb_0p5().gap_0p5().child(div().flex_1().min_w_0().child(ButtonLike::new(("timber-worktree", index))
                        .aria_label(format!("{label}, {branch}, {summary}, {path}"))
                        .full_width().auto_height().tab_index(0_isize)
                        .toggle_state(selected).selected_style(ButtonStyle::Tinted(ui::TintColor::Accent))
                        .tooltip({ let path = path.clone(); move |_, cx| Tooltip::simple(format!("{path}\n{summary}"), cx) })
                        .child(v_flex().w_full().min_w_0().py_1p5().pl_1().gap_0p5().text_left()
                            .border_l_2().border_color(if selected { cx.theme().colors().text_accent } else { gpui::transparent_black() })
                            .child(h_flex().min_w_0().gap_1p5()
                                .child(div().flex_1().min_w_0().text_sm().font_weight(gpui::FontWeight::MEDIUM).truncate().child(label))
                                .when(tree.is_primary, |this| this.child(div().px_1().rounded_sm().border_1().border_color(cx.theme().colors().border).child(Label::new("primary").size(LabelSize::XSmall).color(Color::Muted)))))
                            .child(h_flex().min_w_0().gap_1()
                                .child(Icon::new(IconName::GitBranch).size(IconSize::XSmall).color(Color::Muted))
                                .child(div().flex_1().min_w_0().text_xs().font_buffer(cx).text_color(cx.theme().colors().text_muted).truncate().child(branch)))
                            .child(h_flex().min_w_0().child(div().min_w_0().px_1().rounded_sm().bg(cx.theme().colors().element_background).text_xs().text_color(color.color(cx)).truncate().child(status)))
                            .when(self.show_paths, |this| this.child(div().text_xs().font_buffer(cx).text_color(cx.theme().colors().text_muted).overflow_hidden().child(path))))
                        .on_click(cx.listener(move |this, _, window, cx| this.activate_worktree(tree.clone(), None, window, cx)))))
                        .child(IconButton::new(("timber-done", index), IconName::Check)
                            .icon_size(IconSize::Small).tab_index(0_isize)
                            .aria_label("Finish worktree").tooltip(Tooltip::text("Done: remove worktree"))
                            .disabled(cleanup_disabled)
                            .on_click(cx.listener(move |this, _, window, cx| this.open_cleanup(cleanup_tree.clone(), window, cx))))
                }))
                .children(retained_rows.into_iter().map(|(id, workspace, path)| {
                    let path = display_text(&path.display().to_string());
                    div().py_1().child(ButtonLike::new(("timber-retained", id))
                        .full_width().auto_height().tab_index(0_isize)
                        .aria_label(format!("Retained editor, unavailable: {path}"))
                        .toggle_state(Some(id) == active_workspace)
                        .selected_style(ButtonStyle::Tinted(ui::TintColor::Accent))
                        .tooltip({ let path = path.clone(); move |_, cx| Tooltip::simple(path.clone(), cx) })
                        .child(v_flex().w_full().min_w_0().py_1().text_left()
                            .child(Label::new("Retained editor").size(LabelSize::Small))
                            .child(Label::new("Unavailable").size(LabelSize::XSmall).color(Color::Warning))
                            .when(self.show_paths, |this| this.child(div().text_xs().overflow_hidden().child(path))))
                        .on_click(cx.listener(move |this, _, window, cx| this.activate_retained(workspace.clone(), window, cx))))
                })))
            .child(operations)
    }
}

impl WorkspaceSidebar for TimberSidebar {
    fn width(&self, _cx: &App) -> Pixels {
        self.width
    }
    fn set_width(&mut self, width: Option<Pixels>, cx: &mut Context<Self>) {
        self.width = width.unwrap_or(px(262.)).clamp(px(180.), px(500.));
        cx.notify();
    }
    fn has_notifications(&self, _cx: &App) -> bool {
        false
    }
    fn is_threads_list_view_active(&self) -> bool {
        false
    }
    fn side(&self, _cx: &App) -> SidebarSide {
        SidebarSide::Left
    }
}

impl EventEmitter<SidebarEvent> for TimberSidebar {}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_settings::AgentSettings;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use project::{DisableAiSettings, Project};
    use settings::{Settings, SettingsStore};

    #[test]
    fn inventory_watch_filters_unrelated_changes() {
        let repository = Path::new("/repo/.git");
        let roots = vec![PathBuf::from("/repo"), PathBuf::from("/worktrees/feature")];
        for (path, expected) in [
            ("/repo/.git/worktrees", true),
            ("/repo/.git/worktrees/feature/gitdir", true),
            ("/worktrees/feature", true),
            ("/worktrees/feature/.git", true),
            ("/worktrees", true),
            ("/repo/file.txt", false),
            ("/repo/.git/HEAD", false),
            ("/worktrees/feature/file.txt", false),
            ("/other/.git/worktrees/feature", false),
            ("/worktrees/feature-other", false),
        ] {
            assert_eq!(
                inventory_path_changed(Path::new(path), repository, &roots),
                expected,
                "{path}"
            );
        }
    }

    fn tree(path: &str) -> WorktreeRecord {
        serde_json::from_value(serde_json::json!({
            "path": path, "canonicalPath": path, "branch": "feature", "head": null,
            "isPrimary": false, "detached": false, "pathExists": true,
            "locked": null, "prunable": null,
            "status": {"kind": "known", "modified": [], "untracked": [], "ignored": []}
        }))
        .expect("complete worktree fixture")
    }

    fn remember(sidebar: &mut TimberSidebar, workspace: &Entity<Workspace>, path: &str, cx: &App) {
        sidebar.selected_common_git_dir = Some("/repo/.git".into());
        sidebar.retained.insert(
            workspace.entity_id(),
            RetainedWorktree {
                roots: PathList::new(&workspace.read(cx).root_paths(cx)),
                workspace: workspace.downgrade(),
                common_git_dir: "/repo/.git".into(),
                tree: tree(path),
            },
        );
    }

    #[gpui::test]
    async fn test_selector_activates_retained_workspaces_in_current_window(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            DisableAiSettings::register(cx);
        });
        let filesystem = FakeFs::new(cx.executor());
        filesystem
            .insert_tree("/root_a", serde_json::json!({"file.txt": "A"}))
            .await;
        filesystem
            .insert_tree("/root_b", serde_json::json!({"file.txt": "B"}))
            .await;
        let project_a = Project::test(filesystem.clone(), [Path::new("/root_a")], cx).await;
        let project_b = Project::test(filesystem, [Path::new("/root_b")], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));
        let workspace_a =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        let workspace_b = multi_workspace.update_in(cx, |multi_workspace, window, cx| {
            multi_workspace.test_add_workspace(project_b, window, cx)
        });
        cx.update(|_window, cx| {
            DisableAiSettings::override_global(DisableAiSettings { disable_ai: true }, cx);
            let mut settings = AgentSettings::get_global(cx).clone();
            settings.enabled = false;
            AgentSettings::override_global(settings, cx);
        });
        cx.run_until_parked();

        let sidebar = cx.new(|cx| {
            TimberSidebar::with_loader(
                multi_workspace.clone(),
                Arc::new(|_| futures::future::pending().boxed()),
                cx,
            )
        });

        sidebar.update(cx, |sidebar, cx| {
            remember(sidebar, &workspace_a, "/root_a", cx);
            remember(sidebar, &workspace_b, "/root_b", cx);
        });
        multi_workspace.update(cx, |multi_workspace, cx| {
            multi_workspace.register_sidebar(sidebar.clone(), cx);
            multi_workspace.open_sidebar(cx);
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("ICON-RefreshTitle").is_some(),
            "Timber sidebar must expose its Refresh worktrees control with AI disabled"
        );

        for expected_workspace in [workspace_a, workspace_b] {
            sidebar.update_in(cx, |sidebar, window, cx| {
                sidebar.activate_retained(expected_workspace.downgrade(), window, cx);
            });
            cx.run_until_parked();
            multi_workspace.read_with(cx, |multi_workspace, _| {
                assert_eq!(multi_workspace.workspace(), &expected_workspace);
                assert_eq!(multi_workspace.workspaces().count(), 2);
            });
            sidebar.read_with(cx, |sidebar, _| assert!(sidebar.error.is_none()));
        }
    }
    #[gpui::test]
    async fn external_deletion_refreshes_from_primary_and_retains_active_edits(
        cx: &mut TestAppContext,
    ) {
        use fs::Fs as _;
        use workspace::item::test::TestItem;
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            DisableAiSettings::register(cx);
        });
        let filesystem = FakeFs::new(cx.executor());
        filesystem
            .insert_tree(
                "/repo",
                serde_json::json!({".git": {"worktrees": {}}, "file": "A"}),
            )
            .await;
        filesystem
            .insert_tree("/linked", serde_json::json!({"file": "B"}))
            .await;
        let project = Project::test(filesystem.clone(), [Path::new("/linked")], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        let item = cx.new(|cx| {
            let mut item = TestItem::new(cx).with_dirty(true);
            item.state = "recover these edits".into();
            item
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(item.clone()), None, true, window, cx);
        });
        let (request_sender, requests) = async_channel::unbounded();
        let (sender, receiver) = async_channel::unbounded();
        let sidebar = cx.new(|cx| {
            TimberSidebar::with_loader(
                multi_workspace.clone(),
                Arc::new(move |cwd| {
                    request_sender.try_send(cwd).expect("record request");
                    let receiver = receiver.clone();
                    async move { receiver.recv().await.expect("test inventory") }.boxed()
                }),
                cx,
            )
        });
        cx.run_until_parked();
        assert_eq!(
            requests.try_recv().expect("initial request"),
            PathBuf::from("/linked")
        );
        let mut inventory = Inventory {
            repository: crate::timber_cli::RepositoryIdentity {
                common_git_dir: "/repo/.git".into(),
                primary_worktree_root: "/repo".into(),
                current_worktree_root: "/linked".into(),
            },
            worktrees: vec![tree("/repo"), tree("/linked")],
        };
        sender
            .send(Ok(inventory.clone()))
            .await
            .expect("initial inventory");
        cx.run_until_parked();
        filesystem
            .remove_dir(
                Path::new("/linked"),
                fs::RemoveOptions {
                    recursive: true,
                    ..Default::default()
                },
            )
            .await
            .expect("external deletion");
        cx.run_until_parked();
        cx.advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(
            requests.try_recv().expect("automatic refresh"),
            PathBuf::from("/repo")
        );
        sidebar.update(cx, |sidebar, cx| {
            for _ in 0..50 {
                sidebar.schedule_inventory_refresh(cx);
            }
            assert_eq!(
                sidebar
                    .inventory
                    .as_ref()
                    .expect("last good inventory")
                    .worktrees
                    .len(),
                2
            );
        });
        assert!(
            requests.try_recv().is_err(),
            "events must not interrupt or duplicate the in-flight listing"
        );
        inventory.worktrees = vec![tree("/repo")];
        inventory.repository.current_worktree_root = "/repo".into();
        sender
            .send(Ok(inventory))
            .await
            .expect("reconciled inventory");
        cx.run_until_parked();
        sidebar.read_with(cx, |sidebar, cx| {
            assert_eq!(
                sidebar
                    .inventory
                    .as_ref()
                    .expect("inventory")
                    .worktrees
                    .len(),
                1
            );
            assert_eq!(sidebar.recovery_rows(cx).len(), 1);
        });
        multi_workspace.read_with(cx, |multi_workspace, _| {
            assert_eq!(multi_workspace.workspace(), &workspace)
        });
        workspace.read_with(cx, |workspace, cx| {
            assert!(
                workspace
                    .items_of_type::<TestItem>(cx)
                    .any(|candidate| candidate == item)
            );
            assert_eq!(item.read(cx).state, "recover these edits");
            assert!(item.read(cx).is_dirty);
        });
    }

    #[gpui::test]
    async fn refresh_preserves_good_inventory_and_rejects_stale_results(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            DisableAiSettings::register(cx);
        });
        let filesystem = FakeFs::new(cx.executor());
        filesystem
            .insert_tree("/repo", serde_json::json!({"file.txt": "A"}))
            .await;
        let project = Project::test(filesystem, [Path::new("/repo")], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let (sender, receiver) = async_channel::unbounded();
        let sidebar = cx.new(|cx| {
            TimberSidebar::with_loader(
                multi_workspace.clone(),
                Arc::new(move |_| {
                    let receiver = receiver.clone();
                    async move { receiver.recv().await.expect("test inventory") }.boxed()
                }),
                cx,
            )
        });
        cx.run_until_parked();
        let inventory = Inventory {
            repository: crate::timber_cli::RepositoryIdentity {
                common_git_dir: "/repo/.git".into(),
                primary_worktree_root: "/repo".into(),
                current_worktree_root: "/repo".into(),
            },
            worktrees: Vec::new(),
        };
        sender.send(Ok(inventory)).await.expect("send inventory");
        cx.run_until_parked();
        multi_workspace.read_with(cx, |multi_workspace, _| {
            assert_eq!(multi_workspace.workspaces().count(), 1)
        });
        sidebar.update(cx, |sidebar, cx| sidebar.refresh(cx));
        sender
            .send(Err(anyhow::anyhow!("listing failed")))
            .await
            .expect("send failure");
        cx.run_until_parked();
        sidebar.read_with(cx, |sidebar, _| {
            assert!(sidebar.inventory.is_some());
            assert!(
                sidebar
                    .error
                    .as_ref()
                    .is_some_and(|error| error.contains("listing failed"))
            );
        });
        sidebar.update(cx, |sidebar, cx| {
            let generation = sidebar.generation.get();
            sidebar.generation.set(generation + 1);
            sidebar.apply_inventory(
                generation,
                Path::new("/repo"),
                Err(anyhow::anyhow!("stale error")),
                cx,
            );
            assert!(
                !sidebar
                    .error
                    .as_ref()
                    .is_some_and(|error| error.contains("stale error"))
            );
        });
    }
    #[gpui::test]
    async fn retained_recovery_is_scoped_and_keeps_unsaved_items(cx: &mut TestAppContext) {
        use fs::Fs as _;
        use workspace::item::test::TestItem;
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            DisableAiSettings::register(cx);
        });
        let filesystem = FakeFs::new(cx.executor());
        filesystem
            .insert_tree("/root_a", serde_json::json!({"file.txt": "A"}))
            .await;
        filesystem
            .insert_tree("/root_b", serde_json::json!({"file.txt": "B"}))
            .await;
        let project_a = Project::test(filesystem.clone(), [Path::new("/root_a")], cx).await;
        let project_b = Project::test(filesystem.clone(), [Path::new("/root_b")], cx).await;
        let (multi_workspace, cx) = cx
            .add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
        let workspace_a =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        let workspace_b = multi_workspace.update_in(cx, |multi_workspace, window, cx| {
            multi_workspace.test_add_workspace(project_b, window, cx)
        });
        let item = cx.new(|cx| {
            let mut item = TestItem::new(cx).with_dirty(true);
            item.state = "unsaved edits".into();
            item
        });
        workspace_b.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(item.clone()), None, true, window, cx)
        });
        let sidebar = cx.new(|cx| {
            TimberSidebar::with_loader(
                multi_workspace.clone(),
                Arc::new(|_| futures::future::pending().boxed()),
                cx,
            )
        });
        cx.run_until_parked();
        sidebar.update(cx, |sidebar, cx| {
            remember(sidebar, &workspace_a, "/root_a", cx);
            remember(sidebar, &workspace_b, "/root_b", cx);
            sidebar.inventory = None;
            sidebar.cwd = Some("/unseen_repo".into());
            sidebar.selected_common_git_dir = None;
            assert!(
                sidebar.recovery_rows(cx).is_empty(),
                "unseen repository must not expose previous rows"
            );
            sidebar.cwd = Some("/root_a".into());
            sidebar.selected_common_git_dir = Some("/repo/.git".into());
            assert_eq!(sidebar.recovery_rows(cx).len(), 1);
        });
        filesystem
            .remove_dir(
                Path::new("/root_b"),
                fs::RemoveOptions {
                    recursive: true,
                    ..Default::default()
                },
            )
            .await
            .expect("remove worktree externally");
        cx.run_until_parked();
        sidebar.update_in(cx, |sidebar, window, cx| {
            sidebar.activate_retained(workspace_b.downgrade(), window, cx)
        });
        multi_workspace.read_with(cx, |multi_workspace, _| {
            assert_eq!(multi_workspace.workspace(), &workspace_b)
        });
        workspace_b.read_with(cx, |workspace, cx| {
            assert!(
                workspace
                    .items_of_type::<TestItem>(cx)
                    .any(|candidate| candidate == item)
            );
            assert_eq!(item.read(cx).state, "unsaved edits");
            assert!(item.read(cx).is_dirty);
        });
        let root_id = project_a.read_with(cx, |project, cx| {
            project
                .visible_worktrees(cx)
                .next()
                .expect("root")
                .read(cx)
                .id()
        });
        project_a.update(cx, |project, cx| project.remove_worktree(root_id, cx));
        sidebar.update(cx, |sidebar, cx| {
            sidebar.forget_changed_workspaces(cx);
            assert!(
                !sidebar.retained.contains_key(&workspace_a.entity_id()),
                "changed roots invalidate cached identity"
            );
        });
    }

    #[gpui::test]
    async fn rejected_selection_invalidates_pending_activation(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            DisableAiSettings::register(cx);
        });
        let filesystem = FakeFs::new(cx.executor());
        filesystem
            .insert_tree("/repo", serde_json::json!({"file.txt": "A"}))
            .await;
        let project = Project::test(filesystem, [Path::new("/repo")], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let sidebar = cx.new(|cx| {
            TimberSidebar::with_loader(
                multi_workspace,
                Arc::new(|_| futures::future::pending().boxed()),
                cx,
            )
        });
        cx.run_until_parked();
        sidebar.update_in(cx, |sidebar, window, cx| {
            sidebar.inventory = Some(Inventory {
                repository: crate::timber_cli::RepositoryIdentity {
                    common_git_dir: "/repo/.git".into(),
                    primary_worktree_root: "/repo".into(),
                    current_worktree_root: "/repo".into(),
                },
                worktrees: vec![tree("/repo")],
            });
            let pending_generation = sidebar.activation_generation.get();
            let mut missing = tree("/missing");
            missing.canonical_path = None;
            missing.path_exists = false;
            sidebar.activate_worktree(missing, None, window, cx);
            assert_ne!(pending_generation, sidebar.activation_generation.get());
            assert!(sidebar.error.is_some());
        });
    }
    #[gpui::test]
    async fn opening_worktree_caches_recovery_before_next_listing(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            DisableAiSettings::register(cx);
        });
        let app_state = cx.update(workspace::AppState::test);
        app_state
            .fs
            .as_fake()
            .insert_tree("/repo_a", serde_json::json!({"file.txt": "A"}))
            .await;
        app_state
            .fs
            .as_fake()
            .insert_tree("/repo_b", serde_json::json!({"file.txt": "B"}))
            .await;
        let project = Project::test(app_state.fs.clone(), [Path::new("/repo_a")], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let sidebar = cx.new(|cx| {
            TimberSidebar::with_loader(
                multi_workspace.clone(),
                Arc::new(|path| {
                    if path == Path::new("/repo_b") {
                        async {
                            Ok(Inventory {
                                repository: crate::timber_cli::RepositoryIdentity {
                                    common_git_dir: "/repo/.git".into(),
                                    primary_worktree_root: "/repo_a".into(),
                                    current_worktree_root: "/repo_b".into(),
                                },
                                worktrees: vec![tree("/repo_a"), tree("/repo_b")],
                            })
                        }
                        .boxed()
                    } else {
                        futures::future::pending().boxed()
                    }
                }),
                cx,
            )
        });
        cx.run_until_parked();
        sidebar.update_in(cx, |sidebar, window, cx| {
            sidebar.verifier = Arc::new(|_, _, _| async { Ok(()) }.boxed());
            sidebar.selected_common_git_dir = Some("/repo/.git".into());
            sidebar.inventory = Some(Inventory {
                repository: crate::timber_cli::RepositoryIdentity {
                    common_git_dir: "/repo/.git".into(),
                    primary_worktree_root: "/repo_a".into(),
                    current_worktree_root: "/repo_a".into(),
                },
                worktrees: vec![tree("/repo_a"), tree("/repo_b")],
            });
            sidebar.open_from_terminal(PathBuf::from("/repo_b"), Vec::new(), window, cx);
        });
        cx.run_until_parked();
        let workspace_b = multi_workspace.read_with(cx, |multi_workspace, cx| {
            assert_eq!(multi_workspace.workspaces().count(), 2);
            assert_eq!(
                multi_workspace
                    .workspace()
                    .read(cx)
                    .root_paths(cx)
                    .first()
                    .map(|path| path.as_ref()),
                Some(Path::new("/repo_b"))
            );
            multi_workspace.workspace().clone()
        });
        sidebar.update(cx, |sidebar, cx| {
            assert!(sidebar.retained.contains_key(&workspace_b.entity_id()));
            assert!(
                sidebar.recovery_rows(cx).is_empty(),
                "clean unavailable editors do not create recovery rows"
            );
        });
        let dirty_item = cx.new(|cx| workspace::item::test::TestItem::new(cx).with_dirty(true));
        workspace_b.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(dirty_item), None, true, window, cx);
        });
        sidebar.update(cx, |sidebar, cx| {
            assert!(sidebar.retained.contains_key(&workspace_b.entity_id()));
            sidebar.inventory = None;
            assert!(
                sidebar
                    .recovery_rows(cx)
                    .iter()
                    .any(|(id, _, _)| *id == workspace_b.entity_id()),
                "new editor remains recoverable while its first refresh is pending"
            );
        });
    }
    #[gpui::test]
    async fn switching_listed_worktrees_does_not_reload_inventory(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            DisableAiSettings::register(cx);
        });
        let filesystem = FakeFs::new(cx.executor());
        filesystem
            .insert_tree("/repo_a", serde_json::json!({"file.txt": "A"}))
            .await;
        filesystem
            .insert_tree("/repo_b", serde_json::json!({"file.txt": "B"}))
            .await;
        let project_a = Project::test(filesystem.clone(), [Path::new("/repo_a")], cx).await;
        let project_b = Project::test(filesystem, [Path::new("/repo_b")], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));
        let workspace_a =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        let loads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let sidebar = cx.new(|cx| {
            TimberSidebar::with_loader(
                multi_workspace.clone(),
                {
                    let loads = loads.clone();
                    Arc::new(move |_| {
                        loads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        futures::future::pending().boxed()
                    })
                },
                cx,
            )
        });
        cx.run_until_parked();
        sidebar.update(cx, |sidebar, _| {
            sidebar.inventory = Some(Inventory {
                repository: crate::timber_cli::RepositoryIdentity {
                    common_git_dir: "/repo/.git".into(),
                    primary_worktree_root: "/repo_a".into(),
                    current_worktree_root: "/repo_a".into(),
                },
                worktrees: vec![tree("/repo_a"), tree("/repo_b")],
            });
            sidebar.selected_common_git_dir = Some("/repo/.git".into());
            sidebar.loading = false;
        });
        multi_workspace.update_in(cx, |multi_workspace, window, cx| {
            multi_workspace.test_add_workspace(project_b, window, cx);
        });
        cx.run_until_parked();
        multi_workspace.update_in(cx, |multi_workspace, window, cx| {
            multi_workspace.activate(workspace_a, None, window, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            loads.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "same-repository selection must reuse the inventory"
        );
        sidebar.read_with(cx, |sidebar, _| assert!(!sidebar.loading));
    }
}
