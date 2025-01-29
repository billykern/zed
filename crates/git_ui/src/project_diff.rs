use std::{
    any::{Any, TypeId},
    sync::mpsc,
};

use anyhow::Result;
use collections::{HashMap, HashSet};
use editor::{Editor, EditorEvent};
use futures::StreamExt;
use gpui::{
    actions, AnyElement, AnyView, App, AppContext, AsyncWindowContext, Entity, EventEmitter,
    FocusHandle, Focusable, Render, Subscription, Task, WeakEntity,
};
use language::{Anchor, Buffer, Capability};
use multi_buffer::MultiBuffer;
use project::{buffer_store::BufferChangeSet, git::GitState, Project, ProjectPath};
use theme::ActiveTheme;
use ui::prelude::*;
use util::ResultExt as _;
use workspace::{
    item::{BreadcrumbText, Item, ItemEvent, ItemHandle, TabContentParams},
    ItemNavHistory, ToolbarItemLocation, Workspace,
};

actions!(project_diff, [Deploy]);

pub(crate) struct ProjectDiff {
    multibuffer: Entity<MultiBuffer>,
    buffers_to_show: HashMap<ProjectPath, Entity<Buffer>>, // tbd.
    editor: Entity<Editor>,
    project: Entity<Project>,
    git_state: Entity<GitState>,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    worker: Task<Result<()>>,
    update_needed: postage::watch::Sender<()>,

    git_state_subscription: Subscription,
}

struct DiffBuffer {
    buffer: Entity<Buffer>,
    change_set: Entity<BufferChangeSet>,
}

impl ProjectDiff {
    pub(crate) fn register(
        workspace: &mut Workspace,
        _window: Option<&mut Window>,
        _: &mut Context<Workspace>,
    ) {
        workspace.register_action(Self::deploy);
    }

    fn deploy(
        workspace: &mut Workspace,
        _: &Deploy,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        if let Some(existing) = workspace.item_of_type::<Self>(cx) {
            workspace.activate_item(&existing, true, true, window, cx);
        } else {
            let workspace_handle = cx.entity().downgrade();
            let project_diff =
                cx.new(|cx| Self::new(workspace.project().clone(), workspace_handle, window, cx));
            workspace.add_item_to_active_pane(Box::new(project_diff), None, true, window, cx);
        }
    }

    fn new(
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let multibuffer = cx.new(|_| MultiBuffer::new(Capability::ReadWrite));

        let editor = cx.new(|cx| {
            let mut diff_display_editor = Editor::for_multibuffer(
                multibuffer.clone(),
                Some(project.clone()),
                true,
                window,
                cx,
            );
            diff_display_editor.set_expand_all_diff_hunks(cx);
            diff_display_editor
        });

        let git_state = project.read(cx).git_state().clone();
        let git_state_subscription = cx.subscribe_in(
            &git_state,
            window,
            move |this, _git_state, event, _window, _cx| match event {
                project::git::Event::RepositoriesUpdated => {
                    *this.update_needed.borrow_mut() = ();
                }
            },
        );

        let (mut send, recv) = postage::watch::channel::<()>();
        let worker = window.spawn(cx, {
            let this = cx.weak_entity();
            |cx| Self::worker(this, recv, cx)
        });
        // Kick off the worker
        *send.borrow_mut() = ();

        Self {
            project,
            git_state: git_state.clone(),
            workspace,
            focus_handle,
            buffers_to_show: HashMap::default(),
            editor,
            multibuffer,
            update_needed: send,
            worker,
            git_state_subscription,
        }
    }

    fn buffers_to_load(&mut self, cx: &mut Context<Self>) -> Vec<Task<Result<DiffBuffer>>> {
        let Some(repo) = self.git_state.read(cx).active_repository() else {
            self.multibuffer.update(cx, |multibuffer, cx| {
                multibuffer.clear(cx);
            });
            return vec![];
        };

        let mut loaded_buffers = self
            .multibuffer
            .read(cx)
            .all_buffers()
            .iter()
            .filter_map(|buffer| {
                let file = buffer.read(cx).file()?;
                let project_path = ProjectPath {
                    worktree_id: file.worktree_id(cx),
                    path: file.path().clone(),
                };

                Some((project_path, buffer.clone()))
            })
            .collect::<HashMap<_, _>>();

        let mut result = vec![];
        for entry in repo.status() {
            if !entry.status.has_changes() {
                continue;
            }
            let Some(project_path) = repo.repo_path_to_project_path(&entry.repo_path) else {
                continue;
            };

            loaded_buffers.remove(&project_path);
            let load_buffer = self
                .project
                .update(cx, |project, cx| project.open_buffer(project_path, cx));

            let project = self.project.clone();
            result.push(cx.spawn(|_, mut cx| async move {
                let buffer = load_buffer.await?;
                let changes = project
                    .update(&mut cx, |project, cx| {
                        project.open_unstaged_changes(buffer.clone(), cx)
                    })?
                    .await?;

                Ok(DiffBuffer {
                    buffer,
                    change_set: changes,
                })
            }));
        }
        self.multibuffer.update(cx, |multibuffer, cx| {
            for (_, buffer) in loaded_buffers {
                multibuffer.remove_excerpts_for_buffer(&buffer, cx);
            }
        });
        result
    }

    fn register_buffer(&mut self, diff_buffer: DiffBuffer, cx: &mut App) {
        let buffer = diff_buffer.buffer;
        let change_set = diff_buffer.change_set;

        let snapshot = buffer.read(cx).snapshot();
        let diff_hunk_ranges = change_set
            .read(cx)
            .diff_hunks_intersecting_range(Anchor::MIN..Anchor::MAX, &snapshot)
            .map(|diff_hunk| diff_hunk.buffer_range)
            .collect::<Vec<_>>();

        self.multibuffer.update(cx, |multibuffer, cx| {
            multibuffer.set_excerpts_for_buffer(
                buffer,
                diff_hunk_ranges,
                editor::DEFAULT_MULTIBUFFER_CONTEXT,
                cx,
            );
        })
    }

    pub async fn worker(
        this: WeakEntity<Self>,
        mut recv: postage::watch::Receiver<()>,
        mut cx: AsyncWindowContext,
    ) -> Result<()> {
        while let Some(_) = recv.next().await {
            let buffers_to_load = this.update(&mut cx, |this, cx| this.buffers_to_load(cx))?;
            for buffer_to_load in buffers_to_load {
                if let Some(buffer) = buffer_to_load.await.log_err() {
                    this.update(&mut cx, |this, cx| this.register_buffer(buffer, cx))?;
                }
            }
        }

        Ok(())
    }
}

impl EventEmitter<EditorEvent> for ProjectDiff {}

impl Focusable for ProjectDiff {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for ProjectDiff {
    type Event = EditorEvent;

    fn to_item_events(event: &EditorEvent, f: impl FnMut(ItemEvent)) {
        Editor::to_item_events(event, f)
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.deactivated(window, cx));
    }

    fn navigate(
        &mut self,
        data: Box<dyn Any>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.editor
            .update(cx, |editor, cx| editor.navigate(data, window, cx))
    }

    fn tab_tooltip_text(&self, _: &App) -> Option<SharedString> {
        Some("Project Diff".into())
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, _: &App) -> AnyElement {
        Label::new("No changes")
            .color(if params.selected {
                Color::Default
            } else {
                Color::Muted
            })
            .into_any_element()
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("project diagnostics")
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        self.editor.for_each_project_item(cx, f)
    }

    fn is_singleton(&self, _: &App) -> bool {
        false
    }

    fn set_nav_history(
        &mut self,
        nav_history: ItemNavHistory,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, _| {
            editor.set_nav_history(Some(nav_history));
        });
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<workspace::WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<Self>>
    where
        Self: Sized,
    {
        Some(
            cx.new(|cx| ProjectDiff::new(self.project.clone(), self.workspace.clone(), window, cx)),
        )
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.multibuffer.read(cx).is_dirty(cx)
    }

    fn has_conflict(&self, cx: &App) -> bool {
        self.multibuffer.read(cx).has_conflict(cx)
    }

    fn can_save(&self, _: &App) -> bool {
        true
    }

    fn save(
        &mut self,
        format: bool,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.editor.save(format, project, window, cx)
    }

    fn save_as(
        &mut self,
        _: Entity<Project>,
        _: ProjectPath,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) -> Task<Result<()>> {
        unreachable!()
    }

    fn reload(
        &mut self,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.editor.reload(project, window, cx)
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        _: &'a App,
    ) -> Option<AnyView> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.to_any())
        } else if type_id == TypeId::of::<Editor>() {
            Some(self.editor.to_any())
        } else {
            None
        }
    }

    fn breadcrumb_location(&self, _: &App) -> ToolbarItemLocation {
        ToolbarItemLocation::PrimaryLeft
    }

    fn breadcrumbs(&self, theme: &theme::Theme, cx: &App) -> Option<Vec<BreadcrumbText>> {
        self.editor.breadcrumbs(theme, cx)
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.added_to_workspace(workspace, window, cx)
        });
    }
}

impl Render for ProjectDiff {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .bg(cx.theme().colors().editor_background)
            .flex()
            .items_center()
            .justify_center()
            .size_full()
            .child(self.editor.clone())
    }
}
