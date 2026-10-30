use crate::commit_view::{GitBlob, build_buffer, worktree_id_for_repo_path};
use anyhow::{Context as _, Result};
use editor::Editor;
use futures::channel::oneshot;
use fuzzy::{StringMatch, StringMatchCandidate};
use git::repository::{Branch, RepoPath};
use git_ui_core::file_diff_view::FileDiffView;
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement,
    ParentElement, Render, SharedString, Subscription, Task, WeakEntity, Window, rems,
};
use language::Buffer;
use picker::{Picker, PickerDelegate};
use project::git_store::Repository;
use std::sync::Arc;
use ui::{HighlightedLabel, ListItem, ListItemSpacing, prelude::*};
use util::{ResultExt as _, paths::PathStyle};
use workspace::{ModalView, Workspace, notifications::NotifyTaskExt};

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(compare_file_with_revision);
}

fn compare_file_with_revision(
    workspace: &mut Workspace,
    _: &git::CompareFileWithRevision,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(target) = compare_target(workspace, cx) else {
        return;
    };

    let workspace_handle = workspace.weak_handle();
    let repository = target.repository.clone();
    window
        .spawn(cx, async move |cx| {
            let Some(revision) = pick_revision(repository, workspace_handle.clone(), cx).await
            else {
                return Ok(());
            };
            workspace_handle
                .update_in(cx, |workspace, window, cx| {
                    open_file_diff_against_revision(target, revision, workspace, window, cx)
                        .detach_and_notify_err(workspace.weak_handle(), window, cx);
                })
                .ok();
            anyhow::Ok(())
        })
        .detach();
}

#[derive(Clone)]
struct CompareTarget {
    buffer: Entity<Buffer>,
    repository: Entity<Repository>,
    repo_path: RepoPath,
}

fn compare_target(workspace: &Workspace, cx: &App) -> Option<CompareTarget> {
    let editor = workspace.active_item_as::<Editor>(cx)?;
    let buffer = editor.read(cx).active_buffer(cx)?;
    let buffer_id = buffer.read(cx).remote_id();
    let (repository, repo_path) = workspace
        .project()
        .read(cx)
        .git_store()
        .read(cx)
        .repository_and_path_for_buffer_id(buffer_id, cx)?;
    Some(CompareTarget {
        buffer,
        repository,
        repo_path,
    })
}

fn pick_revision(
    repository: Entity<Repository>,
    workspace: WeakEntity<Workspace>,
    cx: &mut gpui::AsyncWindowContext,
) -> Task<Option<String>> {
    cx.spawn(async move |cx| {
        let (tx, rx) = oneshot::channel();
        let branches = repository
            .read_with(cx, |repository, _| repository.branch_list.clone());
        let tags = repository
            .update(cx, |repository, _| repository.tags())
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();

        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.toggle_modal(window, cx, |window, cx| {
                    RevisionPicker::new(branches, tags, tx, window, cx)
                });
            })
            .ok()?;

        rx.await.ok()
    })
}

fn open_file_diff_against_revision(
    target: CompareTarget,
    revision: String,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<Result<Entity<FileDiffView>>> {
    let workspace_handle = workspace.weak_handle();
    window.spawn(cx, async move |cx| {
        let revision = revision.trim().to_string();
        anyhow::ensure!(!revision.is_empty(), "revision cannot be empty");

        let show_result = target
            .repository
            .update(cx, |repository, _| repository.show(revision.clone()));
        show_result
            .await
            .context("failed to resolve revision")?
            .with_context(|| format!("unknown git revision '{revision}'"))?;

        let content = target
            .repository
            .update(cx, |repository, cx| {
                repository.load_text_at_revision(target.repo_path.clone(), revision.clone(), cx)
            })
            .await?
            .with_context(|| {
                format!(
                    "'{}' does not exist at revision '{revision}'",
                    target.repo_path.display(PathStyle::local())
                )
            })?;

        let (language_registry, worktree_id) =
            workspace_handle.read_with(cx, |workspace, cx| {
                let project = workspace.project().read(cx);
                (
                    project.languages().clone(),
                    worktree_id_for_repo_path(
                        target.repository.read(cx),
                        project,
                        &target.repo_path,
                        cx,
                    ),
                )
            })?;
        let worktree_id = worktree_id.context("project has no worktrees")?;

        let file_name = target
            .repo_path
            .file_name()
            .map(|name| name.to_string())
            .unwrap_or_else(|| target.repo_path.display(PathStyle::local()).to_string());
        let display_name = format!("{file_name} @ {revision}");
        let file = Arc::new(GitBlob {
            path: target.repo_path.clone(),
            worktree_id,
            is_deleted: false,
            is_binary: false,
            display_name,
        }) as Arc<dyn language::File>;

        let old_buffer = build_buffer(content, file, &language_registry, cx).await?;
        FileDiffView::open_from_buffers_in(old_buffer, target.buffer, workspace_handle, cx).await
    })
}

struct RevisionPicker {
    picker: Entity<Picker<RevisionPickerDelegate>>,
    _subscription: Subscription,
}

impl RevisionPicker {
    fn new(
        branches: Arc<[Branch]>,
        tags: Vec<String>,
        tx: oneshot::Sender<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let picker = cx.new(|cx| {
            Picker::uniform_list(RevisionPickerDelegate::new(branches, tags, tx), window, cx)
                .initial_width(rems(34.))
        });
        let _subscription = cx.subscribe(&picker, |_, _, _, cx| cx.emit(DismissEvent));
        Self {
            picker,
            _subscription,
        }
    }
}

impl ModalView for RevisionPicker {}
impl EventEmitter<DismissEvent> for RevisionPicker {}

impl Focusable for RevisionPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for RevisionPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .child(self.picker.clone())
            .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                this.picker.update(cx, |this, cx| {
                    this.cancel(&Default::default(), window, cx);
                })
            }))
    }
}

#[derive(Clone)]
enum RevisionCandidate {
    Head,
    Branch { name: SharedString },
    Tag { name: SharedString },
    Typed { query: SharedString },
}

impl RevisionCandidate {
    fn revision(&self) -> &str {
        match self {
            Self::Head => "HEAD",
            Self::Branch { name } | Self::Tag { name } | Self::Typed { query: name } => name,
        }
    }

    fn label(&self) -> String {
        match self {
            Self::Head => "HEAD".to_string(),
            Self::Branch { name } => format!("{name}  branch"),
            Self::Tag { name } => format!("{name}  tag"),
            Self::Typed { query } => format!("Use revision '{query}'"),
        }
    }
}

struct RevisionPickerDelegate {
    candidates: Vec<RevisionCandidate>,
    matches: Vec<StringMatch>,
    selected_index: usize,
    query: String,
    tx: Option<oneshot::Sender<String>>,
}

impl RevisionPickerDelegate {
    fn new(branches: Arc<[Branch]>, tags: Vec<String>, tx: oneshot::Sender<String>) -> Self {
        let mut candidates = vec![RevisionCandidate::Head];
        for branch in branches.iter() {
            candidates.push(RevisionCandidate::Branch {
                name: branch.name().to_string().into(),
            });
        }
        for tag in tags {
            candidates.push(RevisionCandidate::Tag { name: tag.into() });
        }
        Self {
            candidates,
            matches: Vec::new(),
            selected_index: 0,
            query: String::new(),
            tx: Some(tx),
        }
    }

    fn confirm_revision(&mut self, revision: String, cx: &mut Context<Picker<Self>>) {
        let revision = revision.trim().to_string();
        if revision.is_empty() {
            return;
        }
        self.tx.take().map(|tx| tx.send(revision));
        cx.emit(DismissEvent);
    }
}

impl PickerDelegate for RevisionPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "compare file with revision"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Select a branch, tag, or type a revision (HEAD~3, SHA)…".into()
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        if self.query.trim().is_empty() {
            Some("No revisions".into())
        } else {
            Some("Press enter to use the typed revision".into())
        }
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        self.query = query.clone();
        let mut candidates = self.candidates.clone();
        let trimmed = query.trim();
        if !trimmed.is_empty()
            && !candidates
                .iter()
                .any(|candidate| candidate.revision() == trimmed)
        {
            candidates.insert(
                0,
                RevisionCandidate::Typed {
                    query: trimmed.to_string().into(),
                },
            );
        }
        cx.spawn_in(window, async move |picker, cx| {
            let string_candidates = candidates
                .iter()
                .enumerate()
                .map(|(index, candidate)| StringMatchCandidate::new(index, &candidate.label()))
                .collect::<Vec<_>>();
            let matches = if query.is_empty() {
                string_candidates
                    .into_iter()
                    .enumerate()
                    .map(|(index, candidate)| StringMatch {
                        candidate_id: index,
                        string: candidate.string,
                        positions: Vec::new(),
                        score: 0.0,
                    })
                    .collect()
            } else {
                fuzzy::match_strings(
                    &string_candidates,
                    &query,
                    true,
                    true,
                    10000,
                    &Default::default(),
                    cx.background_executor().clone(),
                )
                .await
            };
            picker
                .update(cx, |picker, _| {
                    let delegate = &mut picker.delegate;
                    delegate.candidates = candidates;
                    delegate.matches = matches;
                    delegate.selected_index = if delegate.matches.is_empty() {
                        0
                    } else {
                        delegate
                            .selected_index
                            .min(delegate.matches.len().saturating_sub(1))
                    };
                })
                .log_err();
        })
    }

    fn confirm(&mut self, _: bool, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(hit) = self.matches.get(self.selected_index)
            && let Some(candidate) = self.candidates.get(hit.candidate_id)
        {
            self.confirm_revision(candidate.revision().to_string(), cx);
            return;
        }
        self.confirm_revision(self.query.clone(), cx);
    }

    fn confirm_input(&mut self, _: bool, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.confirm_revision(self.query.clone(), cx);
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let hit = self.matches.get(ix)?;
        Some(
            ListItem::new(format!("revision-picker-{ix}"))
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(HighlightedLabel::new(
                    hit.string.clone(),
                    hit.positions.clone(),
                )),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use editor::test::editor_test_context::assert_state_with_diff;
    use git::repository::Branch;
    use gpui::{TestAppContext, VisualTestContext};
    use language::language_settings::AllLanguageSettings;
    use project::project_settings::ProjectSettings;
    use project::{FakeFs, Project, WorktreeSettings};
    use serde_json::json;
    use settings::{DiffViewStyle, Settings as _, SettingsStore};
    use std::path::Path;
    use theme::LoadThemes;
    use unindent::unindent;
    use util::{path, rel_path::rel_path};
    use workspace::{Item, MultiWorkspace, WorkspaceSettings};

    fn init_test(cx: &mut TestAppContext) {
        zlog::init_test();
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.editor.diff_view_style = Some(DiffViewStyle::Unified);
                });
            });
            theme_settings::init(LoadThemes::JustBase, cx);
            AllLanguageSettings::register(cx);
            editor::init(cx);
            ProjectSettings::register(cx);
            WorktreeSettings::register(cx);
            WorkspaceSettings::register(cx);
            crate::init(cx);
        });
    }

    async fn setup_compare_repo(
        cx: &mut TestAppContext,
    ) -> (std::sync::Arc<FakeFs>, Entity<Project>) {
        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "file.txt": "working copy\n",
                "later.txt": "added later\n",
            }),
        )
        .await;

        let dot_git = Path::new(path!("/project/.git"));
        fs.set_head_for_repo(
            dot_git,
            &[
                ("file.txt", "at head\n".into()),
                ("later.txt", "added later\n".into()),
            ],
            "sha-head",
        );
        fs.set_branch_name(dot_git, Some("main"));
        fs.with_git_state(dot_git, true, |state| {
            state.branches.insert("feature".into());
            state.branches.insert("origin/topic".into());
        })
        .unwrap();
        fs.set_ref_for_repo(dot_git, "refs/heads/main", "sha-head");
        fs.set_ref_for_repo(dot_git, "refs/heads/feature", "sha-feature");
        fs.set_ref_for_repo(dot_git, "refs/remotes/origin/topic", "sha-feature");
        fs.set_ref_for_repo(dot_git, "refs/tags/v1.0", "sha-tag");
        fs.push_commit_history_for_repo(dot_git, "sha-tag", &[("file.txt", "at tag\n".into())]);
        fs.push_commit_history_for_repo(
            dot_git,
            "sha-feature",
            &[("file.txt", "at feature\n".into())],
        );

        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        cx.run_until_parked();
        (fs, project)
    }

    async fn open_file_editor(
        project: &Entity<Project>,
        workspace: &Entity<Workspace>,
        relative_path: &str,
        cx: &mut gpui::VisualTestContext,
    ) -> Entity<Editor> {
        let worktree_id = project.read_with(cx, |project, cx| {
            project.worktrees(cx).next().unwrap().read(cx).id()
        });
        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_path(
                    (worktree_id, rel_path(relative_path)),
                    None,
                    true,
                    window,
                    cx,
                )
            })
            .await
            .unwrap()
            .downcast::<Editor>()
            .unwrap()
    }

    #[gpui::test]
    async fn test_compare_file_with_branch_tag_and_sha(cx: &mut TestAppContext) {
        init_test(cx);
        let (_fs, project) = setup_compare_repo(cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let editor = open_file_editor(&project, &workspace, "file.txt", cx).await;
        cx.focus(&editor);

        let buffer = editor.read_with(cx, |editor, cx| editor.active_buffer(cx).unwrap());
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(0..buffer.len(), "unsaved working copy\n")], None, cx);
        });
        let target = workspace
            .read_with(cx, |workspace, cx| compare_target(workspace, cx))
            .expect("file should belong to a repository");

        for (revision, old_line) in [
            ("feature", "at feature"),
            ("v1.0", "at tag"),
            ("sha-tag", "at tag"),
            ("HEAD~1", "at feature"),
            ("origin/topic", "at feature"),
        ] {
            let diff_view = workspace
                .update_in(cx, |workspace, window, cx| {
                    open_file_diff_against_revision(
                        target.clone(),
                        revision.to_string(),
                        workspace,
                        window,
                        cx,
                    )
                })
                .await
                .unwrap();
            cx.run_until_parked();

            assert_state_with_diff(
                &diff_view.read_with(cx, |diff_view, cx| {
                    diff_view.editor().read(cx).rhs_editor().clone()
                }),
                cx,
                &unindent(&format!(
                    "
                    - {old_line}
                    + ˇunsaved working copy
                    "
                )),
            );
            diff_view.read_with(cx, |diff_view, cx| {
                assert_eq!(
                    diff_view.tab_content_text(0, cx),
                    format!("file.txt @ {revision} ↔ file.txt")
                );
            });
        }
    }

    #[gpui::test]
    async fn test_compare_file_missing_at_revision(cx: &mut TestAppContext) {
        init_test(cx);
        let (_fs, project) = setup_compare_repo(cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let editor = open_file_editor(&project, &workspace, "later.txt", cx).await;
        cx.focus(&editor);

        let target = workspace
            .read_with(cx, |workspace, cx| compare_target(workspace, cx))
            .expect("file should belong to a repository");
        let error = workspace
            .update_in(cx, |workspace, window, cx| {
                open_file_diff_against_revision(target, "v1.0".into(), workspace, window, cx)
            })
            .await
            .expect_err("later.txt should be absent at v1.0");
        assert!(
            error
                .to_string()
                .contains("does not exist at revision 'v1.0'"),
            "unexpected error: {error:#}"
        );
    }

    #[gpui::test]
    async fn test_compare_file_unknown_revision(cx: &mut TestAppContext) {
        init_test(cx);
        let (_fs, project) = setup_compare_repo(cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let editor = open_file_editor(&project, &workspace, "file.txt", cx).await;
        cx.focus(&editor);

        let target = workspace
            .read_with(cx, |workspace, cx| compare_target(workspace, cx))
            .expect("file should belong to a repository");
        let error = workspace
            .update_in(cx, |workspace, window, cx| {
                open_file_diff_against_revision(
                    target,
                    "does-not-exist".into(),
                    workspace,
                    window,
                    cx,
                )
            })
            .await
            .expect_err("unknown revision should fail");
        assert!(
            error.to_string().contains("unknown git revision"),
            "unexpected error: {error:#}"
        );
    }

    #[gpui::test]
    async fn test_action_noop_outside_repository(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(
            path!("/plain"),
            json!({
                "notes.txt": "not in git\n",
            }),
        )
        .await;
        let project = Project::test(fs, [path!("/plain").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let editor = open_file_editor(&project, &workspace, "notes.txt", cx).await;
        cx.focus(&editor);

        workspace.read_with(cx, |workspace, cx| {
            assert!(compare_target(workspace, cx).is_none());
            assert!(workspace.active_modal::<RevisionPicker>(cx).is_none());
        });

        workspace.update_in(cx, |workspace, window, cx| {
            compare_file_with_revision(workspace, &git::CompareFileWithRevision, window, cx);
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            assert!(workspace.active_modal::<RevisionPicker>(cx).is_none());
            assert!(workspace.active_item_as::<FileDiffView>(cx).is_none());
        });
    }

    #[gpui::test]
    async fn test_action_opens_revision_picker(cx: &mut TestAppContext) {
        init_test(cx);
        let (_fs, project) = setup_compare_repo(cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let editor = open_file_editor(&project, &workspace, "file.txt", cx).await;
        cx.focus(&editor);

        workspace.update_in(cx, |workspace, window, cx| {
            compare_file_with_revision(workspace, &git::CompareFileWithRevision, window, cx);
        });
        cx.run_until_parked();

        workspace.read_with(cx, |workspace, cx| {
            assert!(
                workspace.active_modal::<RevisionPicker>(cx).is_some(),
                "action should open the revision picker for a file in a repository"
            );
        });
    }

    #[gpui::test]
    async fn test_revision_picker_lists_branches_and_tags(cx: &mut TestAppContext) {
        init_test(cx);
        let (_fs, project) = setup_compare_repo(cx).await;
        let repository = project.read_with(cx, |project, cx| {
            project.active_repository(cx).expect("repository")
        });
        repository.update(cx, |repository, cx| {
            repository.set_branch_list_for_test(
                vec![
                    Branch {
                        is_head: true,
                        ref_name: "refs/heads/main".into(),
                        upstream: None,
                        most_recent_commit: None,
                    },
                    Branch {
                        is_head: false,
                        ref_name: "refs/heads/feature".into(),
                        upstream: None,
                        most_recent_commit: None,
                    },
                    Branch {
                        is_head: false,
                        ref_name: "refs/remotes/origin/topic".into(),
                        upstream: None,
                        most_recent_commit: None,
                    },
                ],
                cx,
            );
        });

        let (tx, rx) = oneshot::channel();
        let tags = repository
            .update(cx, |repository, _| repository.tags())
            .await
            .expect("tags job should complete")
            .expect("tags should load");
        assert_eq!(tags, vec!["v1.0".to_string()]);

        let window = cx.add_window(|window, cx| {
            let workspace = Workspace::test_new(project.clone(), window, cx);
            workspace
        });
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let picker = window
            .update(cx, |_, window, cx| {
                cx.new(|cx| {
                    RevisionPicker::new(
                        repository.read_with(cx, |repository, _| repository.branch_list.clone()),
                        tags,
                        tx,
                        window,
                        cx,
                    )
                })
            })
            .unwrap();

        picker
            .update_in(cx, |picker, window, cx| {
                picker.picker.update(cx, |picker, cx| {
                    picker.delegate.update_matches(String::new(), window, cx)
                })
            })
            .await;
        cx.run_until_parked();

        let labels = picker.read_with(cx, |picker, cx| {
            picker
                .picker
                .read(cx)
                .delegate
                .matches
                .iter()
                .map(|hit| hit.string.clone())
                .collect::<Vec<_>>()
        });
        assert!(labels.iter().any(|label| label == "HEAD"));
        assert!(labels.iter().any(|label| label.contains("feature")));
        assert!(labels.iter().any(|label| label.contains("origin/topic")));
        assert!(labels.iter().any(|label| label.contains("v1.0")));

        picker
            .update_in(cx, |picker, window, cx| {
                picker.picker.update(cx, |picker, cx| {
                    picker.delegate.update_matches("HEAD~3".into(), window, cx)
                })
            })
            .await;
        cx.run_until_parked();

        picker.update_in(cx, |picker, window, cx| {
            picker.picker.update(cx, |picker, cx| {
                picker.delegate.confirm(false, window, cx);
            });
        });
        assert_eq!(rx.await.unwrap(), "HEAD~3");
    }

    #[gpui::test]
    async fn test_compare_uses_repository_owning_the_file(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(
            path!("/workspace"),
            json!({
                "repo_a": {
                    ".git": {},
                    "a.txt": "a working\n",
                },
                "repo_b": {
                    ".git": {},
                    "b.txt": "b working\n",
                },
            }),
        )
        .await;

        fs.set_head_for_repo(
            Path::new(path!("/workspace/repo_a/.git")),
            &[("a.txt", "a head\n".into())],
            "sha-a",
        );
        fs.set_ref_for_repo(
            Path::new(path!("/workspace/repo_a/.git")),
            "refs/heads/topic",
            "sha-a-topic",
        );
        fs.push_commit_history_for_repo(
            Path::new(path!("/workspace/repo_a/.git")),
            "sha-a-topic",
            &[("a.txt", "a topic\n".into())],
        );

        fs.set_head_for_repo(
            Path::new(path!("/workspace/repo_b/.git")),
            &[("b.txt", "b head\n".into())],
            "sha-b",
        );
        fs.set_ref_for_repo(
            Path::new(path!("/workspace/repo_b/.git")),
            "refs/heads/topic",
            "sha-b-topic",
        );
        fs.push_commit_history_for_repo(
            Path::new(path!("/workspace/repo_b/.git")),
            "sha-b-topic",
            &[("b.txt", "b topic\n".into())],
        );

        let project = Project::test(
            fs,
            [
                path!("/workspace/repo_a").as_ref(),
                path!("/workspace/repo_b").as_ref(),
            ],
            cx,
        )
        .await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        cx.run_until_parked();

        let worktree_b = project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .find(|worktree| worktree.read(cx).abs_path().ends_with(Path::new("repo_b")))
                .unwrap()
                .read(cx)
                .id()
        });
        let editor = workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_path((worktree_b, rel_path("b.txt")), None, true, window, cx)
            })
            .await
            .unwrap()
            .downcast::<Editor>()
            .unwrap();
        cx.focus(&editor);

        let target = workspace
            .read_with(cx, |workspace, cx| compare_target(workspace, cx))
            .expect("b.txt should belong to repo_b");
        let diff_view = workspace
            .update_in(cx, |workspace, window, cx| {
                open_file_diff_against_revision(target, "topic".into(), workspace, window, cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();

        assert_state_with_diff(
            &diff_view.read_with(cx, |diff_view, cx| {
                diff_view.editor().read(cx).rhs_editor().clone()
            }),
            cx,
            &unindent(
                "
                - b topic
                + ˇb working
                ",
            ),
        );
    }
}
