use gpui::prelude::*;
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, Pixels, Render, SharedString,
    WeakEntity, Window, px,
};
use std::path::{Path, PathBuf};
use ui::ActiveTheme as _;
use ui::{Button, prelude::*};
use util::ResultExt as _;
use util::path_list::PathList;
use workspace::{MultiWorkspace, OpenMode, Sidebar as WorkspaceSidebar, SidebarEvent, SidebarSide};

pub struct TimberSidebar {
    multi_workspace: WeakEntity<MultiWorkspace>,
    focus_handle: FocusHandle,
    width: Pixels,
    worktree_paths: Vec<PathBuf>,
    error: Option<SharedString>,
}

impl TimberSidebar {
    pub fn new(multi_workspace: Entity<MultiWorkspace>, cx: &mut Context<Self>) -> Self {
        let worktree_paths = std::env::var_os("TIMBER_WORKTREE_PATHS")
            .map(|paths| std::env::split_paths(&paths).collect())
            .unwrap_or_default();
        Self {
            multi_workspace: multi_workspace.downgrade(),
            focus_handle: cx.focus_handle(),
            width: px(240.),
            worktree_paths,
            error: None,
        }
    }

    fn activate_path(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let task = self
            .multi_workspace
            .update(cx, |multi_workspace, cx| {
                multi_workspace.find_or_create_local_workspace(
                    PathList::new(&[path.to_path_buf()]),
                    None,
                    None,
                    OpenMode::Activate,
                    None,
                    window,
                    cx,
                )
            })
            .log_err();
        let Some(task) = task else {
            return;
        };
        let weak_self = cx.weak_entity();
        cx.spawn(async move |_, cx| {
            let error = task.await.err().map(|error| error.to_string());
            weak_self
                .update(cx, |this, cx| {
                    this.error = error.map(Into::into);
                    cx.notify();
                })
                .log_err();
        })
        .detach();
    }
}

impl Focusable for TimberSidebar {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TimberSidebar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active_paths = self
            .multi_workspace
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace.workspace().read(cx).root_paths(cx)
            })
            .log_err()
            .unwrap_or_default();
        div()
            .track_focus(&self.focus_handle)
            .flex()
            .flex_col()
            .size_full()
            .bg(cx.theme().colors().surface_background)
            .child(
                div()
                    .h(px(34.))
                    .px_3()
                    .flex()
                    .items_center()
                    .text_xs()
                    .child("WORKTREES"),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(div().px_3().py_2().text_sm().child(error))
            })
            .children(self.worktree_paths.iter().map(|path| {
                let is_active = active_paths
                    .iter()
                    .any(|active_path| active_path.as_ref() == path.as_path());
                let label: SharedString = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string())
                    .into();
                let accessible_label = path.display().to_string();
                let path = path.clone();
                Button::new(accessible_label.clone(), label)
                    .aria_label(accessible_label)
                    .tab_index(0_isize)
                    .toggle_state(is_active)
                    .selected_style(ui::ButtonStyle::Tinted(ui::TintColor::Accent))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.activate_path(&path, window, cx);
                    }))
            }))
    }
}

impl WorkspaceSidebar for TimberSidebar {
    fn width(&self, _cx: &App) -> Pixels {
        self.width
    }

    fn set_width(&mut self, width: Option<Pixels>, cx: &mut Context<Self>) {
        self.width = width.unwrap_or(px(240.)).clamp(px(180.), px(500.));
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
    use fs::FakeFs;
    use gpui::TestAppContext;
    use project::{DisableAiSettings, Project};
    use settings::{Settings, SettingsStore};

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
        let sidebar = cx.new(|cx| TimberSidebar::new(multi_workspace.clone(), cx));

        for (path, expected_workspace) in [("/root_a", workspace_a), ("/root_b", workspace_b)] {
            sidebar.update_in(cx, |sidebar, window, cx| {
                sidebar.activate_path(Path::new(path), window, cx);
            });
            cx.run_until_parked();
            multi_workspace.read_with(cx, |multi_workspace, _| {
                assert_eq!(multi_workspace.workspace(), &expected_workspace);
                assert_eq!(multi_workspace.workspaces().count(), 2);
            });
            sidebar.read_with(cx, |sidebar, _| assert!(sidebar.error.is_none()));
        }
    }
}
