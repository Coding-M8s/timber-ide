use crate::timber_cli::{self, CleanupOptions, CleanupPreview, FileStatus, WorktreeRecord};
use crate::timber_operations::{OperationPhase, OperationRequest, TimberOperations};
use crate::timber_sidebar::TimberSidebar;
use editor::{Editor, EditorEvent};
use gpui::{prelude::*, *};
use menu::{Cancel, Confirm};
use std::time::Duration;
use terminal::Terminal;
use ui::{Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, MultiWorkspace, Workspace};

pub(crate) struct CleanupModal {
    sidebar: WeakEntity<TimberSidebar>,
    multi_workspace: WeakEntity<MultiWorkspace>,
    operations: Entity<TimberOperations>,
    request: OperationRequest,
    tree: WorktreeRecord,
    options: CleanupOptions,
    base: Entity<Editor>,
    focus_handle: FocusHandle,
    preview: Option<CleanupPreview>,
    preview_task: Option<Task<()>>,
    preview_generation: u64,
    loading: bool,
    error: Option<SharedString>,
    operation_id: Option<u64>,
    _subscriptions: Vec<Subscription>,
}

impl CleanupModal {
    pub(crate) fn new(
        sidebar: WeakEntity<TimberSidebar>,
        multi_workspace: WeakEntity<MultiWorkspace>,
        operations: Entity<TimberOperations>,
        request: OperationRequest,
        tree: WorktreeRecord,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let base = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Use saved or configured base", window, cx);
            editor
        });
        let subscriptions = vec![
            cx.subscribe(&base, |this, editor, event, cx| {
                if matches!(event, EditorEvent::BufferEdited) && this.operation_id.is_none() {
                    let text = editor.read(cx).text(cx);
                    this.options.base = (!text.trim().is_empty()).then(|| text.trim().to_owned());
                    this.load_preview(cx);
                }
            }),
            cx.observe(&operations, |this, operations, cx| {
                let Some(id) = this.operation_id else { return; };
                let Some(operation) = operations.read(cx).operations.iter().find(|operation| operation.id == id) else { return; };
                if operation.phase == OperationPhase::NeedsRefresh {
                    this.error = operation.reconciliation_error.clone().or_else(|| operation.error.clone()).map(Into::into);
                } else if operation.phase == OperationPhase::Finished {
                    this.operation_id = None;
                    let removed = operation.inventory.as_ref().is_some_and(|inventory| {
                        inventory.repository.common_git_dir == this.request.common_git_dir
                            && !inventory.worktrees.iter().any(|tree| tree.path == this.tree.path)
                    });
                    if operation.error.is_none() && removed {
                        cx.emit(DismissEvent);
                    } else {
                        this.error = Some(operation.error.clone().unwrap_or_else(|| "The worktree is still present. Review the preview before retrying.".to_owned()).into());
                        this.preview = None;
                    }
                    this.base.update(cx, |editor, _| editor.set_read_only(false));
                }
                cx.notify();
            }),
        ];
        let mut modal = Self {
            sidebar,
            multi_workspace,
            operations,
            request,
            tree,
            options: CleanupOptions {
                keep_branch: false,
                discard_ignored: true,
                base: None,
            },
            base,
            focus_handle: cx.focus_handle(),
            preview: None,
            preview_task: None,
            preview_generation: 0,
            loading: false,
            error: None,
            operation_id: None,
            _subscriptions: subscriptions,
        };
        modal.load_preview(cx);
        modal
    }

    fn load_preview(&mut self, cx: &mut Context<Self>) {
        if self.operation_id.is_some() {
            return;
        }
        self.preview_generation = self.preview_generation.wrapping_add(1);
        let generation = self.preview_generation;
        self.preview = None;
        self.error = None;
        self.loading = true;
        let cwd = self.request.cwd.clone();
        let repository = self.request.common_git_dir.clone();
        let selected = self.tree.clone();
        let options = self.options.clone();
        self.preview_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(200))
                .await;
            let timeout = cx.background_executor().timer(timber_cli::TIMEOUT);
            let result = cx
                .background_spawn(async move {
                    timber_cli::preview_cleanup(cwd, repository, selected, options, timeout).await
                })
                .await;
            this.update(cx, |this, cx| {
                if this.preview_generation != generation {
                    return;
                }
                this.loading = false;
                match result {
                    Ok(preview) => this.preview = Some(preview),
                    Err(error) => this.error = Some(format!("{error:#}").into()),
                }
                cx.notify();
            })
            .log_err();
        }));
        cx.notify();
    }

    fn target_workspaces(&self, cx: &App) -> Vec<Entity<Workspace>> {
        self.multi_workspace
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace
                    .workspaces()
                    .filter(|workspace| {
                        let roots = workspace.read(cx).root_paths(cx);
                        roots.len() == 1
                            && roots.first().is_some_and(|root| {
                                root.as_ref() == self.tree.path.as_path()
                                    || Some(root.as_ref()) == self.tree.canonical_path.as_deref()
                            })
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    fn target_terminals(&self, cx: &App) -> Vec<Entity<Terminal>> {
        let mut terminals = Vec::new();
        for workspace in self.target_workspaces(cx) {
            for terminal in workspace
                .read(cx)
                .project()
                .read(cx)
                .local_terminal_handles()
            {
                if let Some(terminal) = terminal.upgrade()
                    && !terminals.contains(&terminal)
                {
                    terminals.push(terminal);
                }
            }
        }
        terminals
    }

    fn confirm(&mut self, _: &Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        if self.loading || self.operation_id.is_some() {
            return;
        }
        let Some(preview) = self.preview.clone() else {
            return;
        };
        let result = (|| {
            self.sidebar.update(cx, |sidebar, cx| {
                sidebar.retain_cleanup_editors(&preview.tree, &self.request.common_git_dir, cx)
            })??;
            let mut request = self.request.clone();
            request.cleanup = Some(preview);
            // Reserve the repository before touching any explicitly selected sessions.
            let id = self
                .operations
                .update(cx, |operations, cx| operations.start(request, cx))?;
            for terminal in self.target_terminals(cx) {
                terminal.update(cx, |terminal, _| terminal.terminate_process());
            }
            self.operation_id = Some(id);
            self.base.update(cx, |editor, _| editor.set_read_only(true));
            self.preview = None;
            Ok::<_, anyhow::Error>(())
        })();
        self.error = result.err().map(|error| format!("{error:#}").into());
        cx.notify();
    }

    fn cancel(&mut self, _: &Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl Focusable for CleanupModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
impl EventEmitter<DismissEvent> for CleanupModal {}
impl ModalView for CleanupModal {}

impl Render for CleanupModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self.operation_id.is_some();
        let busy = self.operations.read(cx).busy(&self.request.common_git_dir);
        let needs_refresh = self.operation_id.is_some_and(|id| {
            self.operations.read(cx).operations.iter().any(|operation| {
                operation.id == id && operation.phase == OperationPhase::NeedsRefresh
            })
        });
        let dirty: Vec<_> = self
            .target_workspaces(cx)
            .iter()
            .flat_map(|workspace| {
                workspace
                    .read(cx)
                    .items(cx)
                    .filter(|item| item.is_dirty(cx))
                    .map(|item| item.tab_content_text(0, cx).to_string())
                    .collect::<Vec<_>>()
            })
            .collect();
        let terminals = self.target_terminals(cx).len();
        let (modified, untracked, ignored) = match &self
            .preview
            .as_ref()
            .map(|preview| &preview.tree)
            .unwrap_or(&self.tree)
            .status
        {
            FileStatus::Known {
                modified,
                untracked,
                ignored,
            } => (*modified, *untracked, *ignored),
            FileStatus::Unknown { .. } => (0, 0, 0),
        };
        let branch =
            timber_cli::display_text(self.tree.branch.as_deref().unwrap_or("Detached HEAD"));
        let path = timber_cli::display_text(&self.tree.path.display().to_string());
        let label = timber_cli::display_text(
            &self
                .tree
                .path
                .file_name()
                .map(|name| name.to_string_lossy())
                .unwrap_or_else(|| self.tree.path.to_string_lossy()),
        );
        let can_remove = self.preview.is_some() && !self.loading && !running && !busy;
        v_flex().key_context("TimberCleanupModal").track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cancel)).on_action(cx.listener(Self::confirm))
            .elevation_2(cx).w(rems(36.)).max_w(window.viewport_size().width - px(32.))
            .max_h(window.viewport_size().height - px(64.)).overflow_hidden()
            .child(v_flex().flex_none().px_5().pt_4().pb_3().gap_1()
                .child(Headline::new(format!("Finish \"{label}\"?")).size(HeadlineSize::Small))
                .child(Label::new("Remove this worktree and close its editors and terminal sessions.").size(LabelSize::Small).color(Color::Muted)))
            .child(v_flex().id("cleanup-details").flex_1().min_h_0().overflow_y_scroll().px_5().pb_4().gap_3()
                .child(v_flex().p_3().gap_2().rounded_md().border_1().border_color(cx.theme().colors().border)
                    .child(Label::new("REMOVE WORKTREE").size(LabelSize::XSmall).color(Color::Muted))
                    .child(div().text_sm().font_buffer(cx).child(path))
                    .child(Label::new(if self.tree.branch.is_none() { "Detached HEAD; no local branch to delete".to_owned() }
                        else if self.options.keep_branch { format!("Keep local branch {branch}") }
                        else { format!("Delete local branch {branch}, including any unmerged commits") }).size(LabelSize::Small)
                        .color(if self.options.keep_branch { Color::Success } else { Color::Warning }))
                    .child(Label::new("Remote branches are left untouched.").size(LabelSize::Small).color(Color::Muted)))
                .child(v_flex().gap_2()
                    .child(Button::new("cleanup-keep-branch", "Keep the local branch").tab_index(0_isize)
                        .toggle_state(self.options.keep_branch)
                        .start_icon(Icon::new(if self.options.keep_branch { IconName::Check } else { IconName::Circle }).size(IconSize::Small))
                        .disabled(running || self.tree.branch.is_none())
                        .on_click(cx.listener(|this, _, _, cx| { this.options.keep_branch = !this.options.keep_branch; this.load_preview(cx); })))
                    .child(Button::new("cleanup-discard-ignored", "Discard all ignored files, including .env").tab_index(0_isize)
                        .toggle_state(self.options.discard_ignored)
                        .start_icon(Icon::new(if self.options.discard_ignored { IconName::Check } else { IconName::Circle }).size(IconSize::Small))
                        .disabled(running)
                        .on_click(cx.listener(|this, _, _, cx| { this.options.discard_ignored = !this.options.discard_ignored; this.load_preview(cx); })))
                    .child(Label::new("Turn off to refuse deletion while ignored files exist; files are not copied elsewhere.").size(LabelSize::XSmall).color(Color::Muted))
                    .child(Label::new("Base branch (optional)").size(LabelSize::Small))
                    .child(div().px_2().py_1p5().border_1().border_color(cx.theme().colors().border).rounded_md().child(self.base.clone())))
                .when(modified > 0 || untracked > 0 || ignored > 0, |this| this.child(v_flex().p_3().gap_1().rounded_md().bg(cx.theme().colors().element_background)
                    .child(Label::new(format!("{modified} modified · {untracked} untracked · {ignored} ignored")).size(LabelSize::Small).color(Color::Warning))
                    .child(Label::new("Modified and untracked files on disk will be discarded. This cannot be undone.").size(LabelSize::Small).color(Color::Warning))))
                .when(!dirty.is_empty(), |this| this.child(v_flex().gap_1()
                    .child(Label::new(format!("{} unsaved editors will be closed and their edits discarded", dirty.len())).size(LabelSize::Small).color(Color::Warning))
                    .children(dirty.into_iter().map(|name| Label::new(timber_cli::display_text(&name)).size(LabelSize::XSmall)))
                    .child(Label::new("Save any edits you want to keep before removing this worktree.").size(LabelSize::XSmall).color(Color::Muted))))
                .when(terminals > 0, |this| this.child(Label::new(format!("{terminals} terminal sessions in this worktree will be stopped. Other worktrees are unaffected.")).size(LabelSize::Small).color(Color::Warning)))
                .when(self.loading, |this| this.child(Label::new("Checking worktree and file state…").size(LabelSize::Small).color(Color::Muted)))
                .when(running && !needs_refresh, |this| this.child(Label::new("Removing worktree… Output is available in Operations.").size(LabelSize::Small).color(Color::Accent)))
                .when_some(self.error.clone(), |this, error| this.child(div().text_sm().text_color(cx.theme().status().error).child(error))))
            .child(h_flex().flex_none().px_5().py_3().border_t_1().border_color(cx.theme().colors().border).justify_between()
                .child(Button::new("cleanup-preview", if needs_refresh { "Refresh worktrees" } else { "Update preview" }).tab_index(0_isize).disabled((running && !needs_refresh) || self.loading)
                    .tooltip(Tooltip::text("Recheck this target and its current file state"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if needs_refresh {
                            if let Some(id) = this.operation_id { this.operations.update(cx, |operations, cx| operations.reconcile(id, cx)); }
                        } else { this.load_preview(cx); }
                    })))
                .child(h_flex().gap_2()
                    .child(Button::new("cleanup-cancel", if running { "Hide" } else { "Cancel" }).tab_index(0_isize)
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))))
                    .child(Button::new("cleanup-remove", "Remove worktree").tab_index(0_isize).style(ButtonStyle::Tinted(ui::TintColor::Error))
                        .disabled(!can_remove).on_click(cx.listener(|this, _, window, cx| this.confirm(&Confirm, window, cx))))))
    }
}
