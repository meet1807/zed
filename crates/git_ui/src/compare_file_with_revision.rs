use crate::commit_view::{GitBlob, build_buffer, worktree_id_for_repo_path};
use anyhow::{Context as _, Result};
use editor::Editor;
use futures::channel::oneshot;
use fuzzy::{StringMatch, StringMatchCandidate};
use git::repository::{Branch, FileRevisionEntry, RepoPath};
use git::stash::StashEntry;
use git_ui_core::file_diff_view::FileDiffView;
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement,
    ParentElement, Render, SharedString, Subscription, Task, WeakEntity, Window, rems,
};
use language::{Buffer, Point};
use multi_buffer::ToPoint as _;
use picker::{Picker, PickerDelegate};
use project::git_store::Repository;
use std::sync::Arc;
use ui::{HighlightedLabel, ListItem, ListItemSpacing, prelude::*};
use util::{ResultExt as _, paths::PathStyle};
use workspace::{ModalView, Workspace, notifications::NotifyTaskExt};

const HISTORY_PAGE_SIZE: usize = 20;

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(compare_file_with_revision);
    workspace.register_action(compare_file_with_branch_or_tag);
}

fn compare_file_with_revision(
    workspace: &mut Workspace,
    _: &git::CompareFileWithRevision,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    start_compare(workspace, ComparePickerKind::History, window, cx);
}

fn compare_file_with_branch_or_tag(
    workspace: &mut Workspace,
    _: &git::CompareFileWithBranchOrTag,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    start_compare(workspace, ComparePickerKind::Branches, window, cx);
}

fn start_compare(
    workspace: &mut Workspace,
    kind: ComparePickerKind,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(target) = compare_target(workspace, cx) else {
        return;
    };

    let workspace_handle = workspace.weak_handle();
    let repository = target.repository.clone();
    let repo_path = target.repo_path.clone();
    window
        .spawn(cx, async move |cx| {
            let Some(revision) =
                pick_source(kind, repository, repo_path, workspace_handle.clone(), cx).await
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

#[derive(Clone, Copy)]
enum ComparePickerKind {
    History,
    Branches,
    Stashes,
}

#[derive(Clone, Debug)]
enum CompareChoice {
    Revision(String),
    Branches,
    Stashes,
}

#[derive(Clone)]
struct CompareTarget {
    buffer: Entity<Buffer>,
    repository: Entity<Repository>,
    repo_path: RepoPath,
    cursor: Point,
}

fn compare_target(workspace: &Workspace, cx: &App) -> Option<CompareTarget> {
    let editor = workspace.active_item_as::<Editor>(cx)?;
    let editor = editor.read(cx);
    let buffer = editor.active_buffer(cx)?;
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let cursor = editor.selections.newest_anchor().head().to_point(&snapshot);
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
        cursor,
    })
}

fn pick_source(
    kind: ComparePickerKind,
    repository: Entity<Repository>,
    repo_path: RepoPath,
    workspace: WeakEntity<Workspace>,
    cx: &mut gpui::AsyncWindowContext,
) -> Task<Option<String>> {
    cx.spawn(async move |cx| {
        let mut kind = kind;
        loop {
            let choice = match kind {
                ComparePickerKind::History => {
                    pick_history(repository.clone(), repo_path.clone(), workspace.clone(), cx)
                        .await?
                }
                ComparePickerKind::Branches => {
                    pick_revision(repository.clone(), workspace.clone(), cx).await?
                }
                ComparePickerKind::Stashes => {
                    pick_stash(repository.clone(), repo_path.clone(), workspace.clone(), cx)
                        .await?
                }
            };
            match choice {
                CompareChoice::Revision(revision) => return Some(revision),
                CompareChoice::Branches => kind = ComparePickerKind::Branches,
                CompareChoice::Stashes => kind = ComparePickerKind::Stashes,
            }
        }
    })
}

fn pick_history(
    repository: Entity<Repository>,
    repo_path: RepoPath,
    workspace: WeakEntity<Workspace>,
    cx: &mut gpui::AsyncWindowContext,
) -> Task<Option<CompareChoice>> {
    cx.spawn(async move |cx| {
        let (tx, rx) = oneshot::channel();
        let entries = repository
            .update(cx, |repository, _| {
                repository.log_for_path(repo_path.clone(), 0, HISTORY_PAGE_SIZE)
            })
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.toggle_modal(window, cx, |window, cx| {
                    HistoryPicker::new(repository, repo_path, entries, tx, window, cx)
                });
            })
            .ok()?;
        rx.await.ok()
    })
}

fn pick_revision(
    repository: Entity<Repository>,
    workspace: WeakEntity<Workspace>,
    cx: &mut gpui::AsyncWindowContext,
) -> Task<Option<CompareChoice>> {
    cx.spawn(async move |cx| {
        let (tx, rx) = oneshot::channel();
        let branches = repository.read_with(cx, |repository, _| repository.branch_list.clone());
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

fn pick_stash(
    repository: Entity<Repository>,
    repo_path: RepoPath,
    workspace: WeakEntity<Workspace>,
    cx: &mut gpui::AsyncWindowContext,
) -> Task<Option<CompareChoice>> {
    cx.spawn(async move |cx| {
        let (tx, rx) = oneshot::channel();
        let historical = repository
            .update(cx, |repository, _| repository.historical_paths(repo_path))
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        let stash_entries = repository.read_with(cx, |repository, _| {
            repository.cached_stash().entries
        });
        let mut matching = Vec::new();
        for entry in stash_entries.iter() {
            let revision = format!("stash@{{{}}}", entry.index);
            let paths = repository
                .update(cx, |repository, _| repository.changed_paths_at(revision))
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or_default();
            if paths.iter().any(|path| historical.contains(path)) {
                matching.push(entry.clone());
            }
        }
        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.toggle_modal(window, cx, |window, cx| {
                    StashRevisionPicker::new(matching, tx, window, cx)
                });
            })
            .ok()?;
        rx.await.ok()
    })
}

fn short_revision(revision: &str, resolved_sha: &str) -> String {
    let looks_like_sha = revision.len() >= 7
        && revision
            .chars()
            .all(|character| character.is_ascii_hexdigit());
    if looks_like_sha {
        resolved_sha.chars().take(7).collect()
    } else {
        revision.to_string()
    }
}

fn open_file_diff_against_revision(
    target: CompareTarget,
    revision: String,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<Result<Entity<FileDiffView>>> {
    let workspace_handle = workspace.weak_handle();
    let cursor = target.cursor;
    window.spawn(cx, async move |cx| {
        let revision = revision.trim().to_string();
        anyhow::ensure!(!revision.is_empty(), "revision cannot be empty");

        let details = target
            .repository
            .update(cx, |repository, _| repository.show(revision.clone()));
        let details = details
            .await
            .context("failed to resolve revision")?
            .with_context(|| format!("unknown git revision '{revision}'"))?;
        let short_ref = short_revision(&revision, details.sha.as_ref());

        let loaded = target
            .repository
            .update(cx, |repository, cx| {
                repository.load_file_at_revision(
                    target.repo_path.clone(),
                    revision.clone(),
                    true,
                    cx,
                )
            })
            .await?;

        let (loaded_path, content, missing) = match loaded {
            Some((path, content)) => (path, content, false),
            None => (target.repo_path.clone(), String::new(), true),
        };

        let (language_registry, worktree_id) = workspace_handle.read_with(cx, |workspace, cx| {
            let project = workspace.project().read(cx);
            (
                project.languages().clone(),
                worktree_id_for_repo_path(
                    target.repository.read(cx),
                    project,
                    &loaded_path,
                    cx,
                ),
            )
        })?;
        let worktree_id = worktree_id.context("project has no worktrees")?;

        let file_name = loaded_path
            .file_name()
            .map(|name| name.to_string())
            .unwrap_or_else(|| loaded_path.display(PathStyle::local()).to_string());
        let display_name = if missing {
            format!("{file_name} (not in {short_ref})")
        } else {
            format!("{file_name} ({short_ref})")
        };
        let file = Arc::new(GitBlob {
            path: loaded_path,
            worktree_id,
            is_deleted: missing,
            is_binary: false,
            display_name,
        }) as Arc<dyn language::File>;

        let old_buffer = build_buffer(content, file, &language_registry, cx).await?;
        FileDiffView::open_from_buffers_in(
            old_buffer,
            target.buffer,
            Some(cursor),
            workspace_handle,
            cx,
        )
        .await
    })
}

fn confirm_choice(
    tx: &mut Option<oneshot::Sender<CompareChoice>>,
    choice: CompareChoice,
    cx: &mut Context<Picker<impl PickerDelegate>>,
) {
    if let Some(tx) = tx.take() {
        tx.send(choice).ok();
    }
    cx.emit(DismissEvent);
}

struct HistoryPicker {
    picker: Entity<Picker<HistoryPickerDelegate>>,
    _subscription: Subscription,
}

impl HistoryPicker {
    fn new(
        repository: Entity<Repository>,
        repo_path: RepoPath,
        entries: Vec<FileRevisionEntry>,
        tx: oneshot::Sender<CompareChoice>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let picker = cx.new(|cx| {
            Picker::uniform_list(
                HistoryPickerDelegate::new(repository, repo_path, entries, tx),
                window,
                cx,
            )
            .initial_width(rems(42.))
        });
        let _subscription = cx.subscribe(&picker, |_, _, _, cx| cx.emit(DismissEvent));
        Self {
            picker,
            _subscription,
        }
    }
}

impl ModalView for HistoryPicker {}
impl EventEmitter<DismissEvent> for HistoryPicker {}
impl Focusable for HistoryPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}
impl Render for HistoryPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().child(self.picker.clone()).on_mouse_down_out(
            cx.listener(|this, _, window, cx| {
                this.picker.update(cx, |this, cx| {
                    this.cancel(&Default::default(), window, cx);
                })
            }),
        )
    }
}

#[derive(Clone)]
enum HistoryCandidate {
    Commit(FileRevisionEntry),
    LoadMore,
    Branches,
    Stashes,
}

impl HistoryCandidate {
    fn label(&self) -> String {
        match self {
            Self::Commit(entry) => {
                format!(
                    "{}  {}  {}  {}",
                    entry.sha.chars().take(7).collect::<String>(),
                    entry.subject,
                    entry.author_name,
                    entry.author_when
                )
            }
            Self::LoadMore => "Load more…".into(),
            Self::Branches => "Choose a Branch or Tag…".into(),
            Self::Stashes => "Choose a Stash…".into(),
        }
    }
}

struct HistoryPickerDelegate {
    repository: Entity<Repository>,
    repo_path: RepoPath,
    commits: Vec<FileRevisionEntry>,
    has_more: bool,
    candidates: Vec<HistoryCandidate>,
    matches: Vec<StringMatch>,
    selected_index: usize,
    query: String,
    tx: Option<oneshot::Sender<CompareChoice>>,
}

impl HistoryPickerDelegate {
    fn new(
        repository: Entity<Repository>,
        repo_path: RepoPath,
        entries: Vec<FileRevisionEntry>,
        tx: oneshot::Sender<CompareChoice>,
    ) -> Self {
        let has_more = entries.len() == HISTORY_PAGE_SIZE;
        Self {
            repository,
            repo_path,
            commits: entries,
            has_more,
            candidates: Vec::new(),
            matches: Vec::new(),
            selected_index: 0,
            query: String::new(),
            tx: Some(tx),
        }
    }

    fn rebuild_candidates(&self) -> Vec<HistoryCandidate> {
        let mut candidates = vec![HistoryCandidate::Branches, HistoryCandidate::Stashes];
        candidates.extend(self.commits.iter().cloned().map(HistoryCandidate::Commit));
        if self.has_more {
            candidates.push(HistoryCandidate::LoadMore);
        }
        candidates
    }
}

impl PickerDelegate for HistoryPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "compare file with revision"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Select a commit from this file's history…".into()
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
        let candidates = self.rebuild_candidates();
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
                    delegate.selected_index = delegate
                        .selected_index
                        .min(delegate.matches.len().saturating_sub(1));
                })
                .log_err();
        })
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(hit) = self.matches.get(self.selected_index) else {
            return;
        };
        let Some(candidate) = self.candidates.get(hit.candidate_id).cloned() else {
            return;
        };
        match candidate {
            HistoryCandidate::Commit(entry) => {
                confirm_choice(&mut self.tx, CompareChoice::Revision(entry.sha), cx);
            }
            HistoryCandidate::Branches => {
                confirm_choice(&mut self.tx, CompareChoice::Branches, cx);
            }
            HistoryCandidate::Stashes => {
                confirm_choice(&mut self.tx, CompareChoice::Stashes, cx);
            }
            HistoryCandidate::LoadMore => {
                let skip = self.commits.len();
                let repository = self.repository.clone();
                let repo_path = self.repo_path.clone();
                cx.spawn_in(window, async move |picker, cx| {
                    let more = repository
                        .update(cx, |repository, _| {
                            repository.log_for_path(repo_path, skip, HISTORY_PAGE_SIZE)
                        })
                        .await
                        .ok()
                        .and_then(Result::ok)
                        .unwrap_or_default();
                    picker
                        .update_in(cx, |picker, window, cx| {
                            let has_more = more.len() == HISTORY_PAGE_SIZE;
                            picker.delegate.commits.extend(more);
                            picker.delegate.has_more = has_more;
                            let query = picker.delegate.query.clone();
                            picker.delegate.update_matches(query, window, cx).detach();
                        })
                        .ok();
                })
                .detach();
            }
        }
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
            ListItem::new(format!("history-picker-{ix}"))
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

struct RevisionPicker {
    picker: Entity<Picker<RevisionPickerDelegate>>,
    _subscription: Subscription,
}

impl RevisionPicker {
    fn new(
        branches: Arc<[Branch]>,
        tags: Vec<String>,
        tx: oneshot::Sender<CompareChoice>,
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
        v_flex().child(self.picker.clone()).on_mouse_down_out(
            cx.listener(|this, _, window, cx| {
                this.picker.update(cx, |this, cx| {
                    this.cancel(&Default::default(), window, cx);
                })
            }),
        )
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
    tx: Option<oneshot::Sender<CompareChoice>>,
}

impl RevisionPickerDelegate {
    fn new(branches: Arc<[Branch]>, tags: Vec<String>, tx: oneshot::Sender<CompareChoice>) -> Self {
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
}

impl PickerDelegate for RevisionPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "compare file with branch or tag"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Select a branch, tag, or type a revision (HEAD~3, SHA)…".into()
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
                    delegate.selected_index = delegate
                        .selected_index
                        .min(delegate.matches.len().saturating_sub(1));
                })
                .log_err();
        })
    }

    fn confirm(&mut self, _: bool, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(hit) = self.matches.get(self.selected_index)
            && let Some(candidate) = self.candidates.get(hit.candidate_id)
        {
            confirm_choice(
                &mut self.tx,
                CompareChoice::Revision(candidate.revision().to_string()),
                cx,
            );
            return;
        }
        let trimmed = self.query.trim();
        if !trimmed.is_empty() {
            confirm_choice(
                &mut self.tx,
                CompareChoice::Revision(trimmed.to_string()),
                cx,
            );
        }
    }

    fn confirm_input(&mut self, _: bool, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let trimmed = self.query.trim();
        if !trimmed.is_empty() {
            confirm_choice(
                &mut self.tx,
                CompareChoice::Revision(trimmed.to_string()),
                cx,
            );
        }
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

struct StashRevisionPicker {
    picker: Entity<Picker<StashRevisionDelegate>>,
    _subscription: Subscription,
}

impl StashRevisionPicker {
    fn new(
        entries: Vec<StashEntry>,
        tx: oneshot::Sender<CompareChoice>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let picker = cx.new(|cx| {
            Picker::uniform_list(StashRevisionDelegate::new(entries, tx), window, cx)
                .initial_width(rems(40.))
        });
        let _subscription = cx.subscribe(&picker, |_, _, _, cx| cx.emit(DismissEvent));
        Self {
            picker,
            _subscription,
        }
    }
}

impl ModalView for StashRevisionPicker {}
impl EventEmitter<DismissEvent> for StashRevisionPicker {}
impl Focusable for StashRevisionPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}
impl Render for StashRevisionPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().child(self.picker.clone()).on_mouse_down_out(
            cx.listener(|this, _, window, cx| {
                this.picker.update(cx, |this, cx| {
                    this.cancel(&Default::default(), window, cx);
                })
            }),
        )
    }
}

struct StashRevisionDelegate {
    entries: Vec<StashEntry>,
    matches: Vec<StringMatch>,
    selected_index: usize,
    tx: Option<oneshot::Sender<CompareChoice>>,
}

impl StashRevisionDelegate {
    fn new(entries: Vec<StashEntry>, tx: oneshot::Sender<CompareChoice>) -> Self {
        Self {
            entries,
            matches: Vec::new(),
            selected_index: 0,
            tx: Some(tx),
        }
    }
}

impl PickerDelegate for StashRevisionDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "compare file with stash"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Select a stash that touches this file…".into()
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some("No stashes touch this file".into())
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
        let labels = self
            .entries
            .iter()
            .map(|entry| format!("stash@{{{}}}  {}", entry.index, entry.message))
            .collect::<Vec<_>>();
        cx.spawn_in(window, async move |picker, cx| {
            let string_candidates = labels
                .iter()
                .enumerate()
                .map(|(index, label)| StringMatchCandidate::new(index, label))
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
                    picker.delegate.matches = matches;
                    picker.delegate.selected_index = picker
                        .delegate
                        .selected_index
                        .min(picker.delegate.matches.len().saturating_sub(1));
                })
                .log_err();
        })
    }

    fn confirm(&mut self, _: bool, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(hit) = self.matches.get(self.selected_index)
            && let Some(entry) = self.entries.get(hit.candidate_id)
        {
            confirm_choice(
                &mut self.tx,
                CompareChoice::Revision(format!("stash@{{{}}}", entry.index)),
                cx,
            );
        }
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
            ListItem::new(format!("stash-revision-{ix}"))
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
    use editor::SelectionEffects;
    use editor::test::editor_test_context::assert_state_with_diff;
    use git::repository::Branch;
    use git::stash::GitStash;
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
                "greeting.txt": "working greeting\n",
            }),
        )
        .await;

        let dot_git = Path::new(path!("/project/.git"));
        fs.set_head_for_repo(
            dot_git,
            &[
                ("file.txt", "at head\n".into()),
                ("later.txt", "added later\n".into()),
                ("greeting.txt", "at head greeting\n".into()),
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
        fs.set_ref_for_repo(dot_git, "refs/tags/before-rename", "sha-rename");
        fs.push_commit_history_for_repo(
            dot_git,
            "sha-rename",
            &[("hello.txt", "old name contents\n".into())],
        );
        fs.push_commit_history_for_repo(dot_git, "sha-tag", &[("file.txt", "at tag\n".into())]);
        fs.push_commit_history_for_repo(
            dot_git,
            "sha-feature",
            &[("file.txt", "at feature\n".into())],
        );
        fs.set_follow_aliases_for_repo(dot_git, "greeting.txt", &["greeting.txt", "hello.txt"]);

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
        editor.update_in(cx, |editor, window, cx| {
            editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                selections.select_ranges([Point::new(0, 0)..Point::new(0, 0)]);
            });
        });
        let target = workspace
            .read_with(cx, |workspace, cx| compare_target(workspace, cx))
            .expect("file should belong to a repository");

        for (revision, old_line, tab) in [
            ("feature", "at feature", "file.txt (feature) ↔ file.txt"),
            ("v1.0", "at tag", "file.txt (v1.0) ↔ file.txt"),
            ("sha-tag", "at tag", "file.txt (sha-tag) ↔ file.txt"),
            ("HEAD~1", "at feature", "file.txt (HEAD~1) ↔ file.txt"),
            (
                "origin/topic",
                "at feature",
                "file.txt (origin/topic) ↔ file.txt",
            ),
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

            diff_view.read_with(cx, |diff_view, cx| {
                assert_eq!(diff_view.tab_content_text(0, cx), tab);
            });
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
        }
    }

    #[gpui::test]
    async fn test_compare_file_missing_at_revision_is_empty_left(cx: &mut TestAppContext) {
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
        let diff_view = workspace
            .update_in(cx, |workspace, window, cx| {
                open_file_diff_against_revision(target, "v1.0".into(), workspace, window, cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();
        diff_view.read_with(cx, |diff_view, cx| {
            assert_eq!(
                diff_view.tab_content_text(0, cx),
                "later.txt (not in v1.0) ↔ later.txt"
            );
        });
        assert_state_with_diff(
            &diff_view.read_with(cx, |diff_view, cx| {
                diff_view.editor().read(cx).rhs_editor().clone()
            }),
            cx,
            &unindent(
                "
                + ˇadded later
                ",
            ),
        );
    }

    #[gpui::test]
    async fn test_compare_follows_rename(cx: &mut TestAppContext) {
        init_test(cx);
        let (_fs, project) = setup_compare_repo(cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let editor = open_file_editor(&project, &workspace, "greeting.txt", cx).await;
        cx.focus(&editor);

        let target = workspace
            .read_with(cx, |workspace, cx| compare_target(workspace, cx))
            .expect("file should belong to a repository");
        let diff_view = workspace
            .update_in(cx, |workspace, window, cx| {
                open_file_diff_against_revision(
                    target,
                    "before-rename".into(),
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
            &unindent(
                "
                - old name contents
                + ˇworking greeting
                ",
            ),
        );
        diff_view.read_with(cx, |diff_view, cx| {
            assert_eq!(
                diff_view.tab_content_text(0, cx),
                "hello.txt (before-rename) ↔ greeting.txt"
            );
        });
    }

    #[gpui::test]
    async fn test_compare_preserves_cursor(cx: &mut TestAppContext) {
        init_test(cx);
        let (_fs, project) = setup_compare_repo(cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let editor = open_file_editor(&project, &workspace, "file.txt", cx).await;
        cx.focus(&editor);

        let buffer = editor.read_with(cx, |editor, cx| editor.active_buffer(cx).unwrap());
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(0..buffer.len(), "line one\nline two\nline three\n")], None, cx);
        });
        editor.update_in(cx, |editor, window, cx| {
            editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                selections.select_ranges([Point::new(1, 5)..Point::new(1, 5)]);
            });
        });

        let target = workspace
            .read_with(cx, |workspace, cx| compare_target(workspace, cx))
            .expect("file should belong to a repository");
        assert_eq!(target.cursor, Point::new(1, 5));
        let diff_view = workspace
            .update_in(cx, |workspace, window, cx| {
                open_file_diff_against_revision(target, "feature".into(), workspace, window, cx)
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
                - at feature
                + line one
                + line ˇtwo
                + line three
                ",
            ),
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

        workspace.update_in(cx, |workspace, window, cx| {
            compare_file_with_revision(workspace, &git::CompareFileWithRevision, window, cx);
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            assert!(workspace.active_modal::<HistoryPicker>(cx).is_none());
            assert!(workspace.active_item_as::<FileDiffView>(cx).is_none());
        });
    }

    #[gpui::test]
    async fn test_action_opens_history_picker(cx: &mut TestAppContext) {
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
                workspace.active_modal::<HistoryPicker>(cx).is_some(),
                "default action should open the file-history picker"
            );
        });
    }

    #[gpui::test]
    async fn test_history_picker_lists_commits_and_switches(cx: &mut TestAppContext) {
        init_test(cx);
        let (_fs, project) = setup_compare_repo(cx).await;
        let repository = project.read_with(cx, |project, cx| {
            project.active_repository(cx).expect("repository")
        });
        let (tx, rx) = oneshot::channel();
        let entries = repository
            .update(cx, |repository, _| {
                repository.log_for_path(RepoPath::new("file.txt").unwrap(), 0, HISTORY_PAGE_SIZE)
            })
            .await
            .unwrap()
            .unwrap();
        assert!(entries.iter().any(|entry| entry.sha == "sha-feature"));

        let window = cx.add_window(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let picker = window
            .update(cx, |_, window, cx| {
                cx.new(|cx| {
                    HistoryPicker::new(
                        repository,
                        RepoPath::new("file.txt").unwrap(),
                        entries,
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
        assert!(labels.iter().any(|label| label.contains("sha-fea")));
        assert!(labels.iter().any(|label| label.contains("Branch or Tag")));
        assert!(labels.iter().any(|label| label.contains("Stash")));

        picker.update_in(cx, |picker, window, cx| {
            picker.picker.update(cx, |picker, cx| {
                let stash_ix = picker
                    .delegate
                    .matches
                    .iter()
                    .position(|hit| hit.string.contains("Stash"))
                    .unwrap();
                picker.delegate.selected_index = stash_ix;
                picker.delegate.confirm(false, window, cx);
            });
        });
        assert!(matches!(rx.await.unwrap(), CompareChoice::Stashes));
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
        let window = cx.add_window(|window, cx| Workspace::test_new(project.clone(), window, cx));
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
        match rx.await.unwrap() {
            CompareChoice::Revision(revision) => assert_eq!(revision, "HEAD~3"),
            other => panic!("expected revision, got {other:?}"),
        }
    }

    #[gpui::test]
    async fn test_stash_picker_filters_to_file(cx: &mut TestAppContext) {
        init_test(cx);
        let (fs, project) = setup_compare_repo(cx).await;
        let dot_git = Path::new(path!("/project/.git"));
        fs.push_commit_history_for_repo(
            dot_git,
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            &[("file.txt", "stashed contents\n".into())],
        );
        fs.with_git_state(dot_git, true, |state| {
            state.stash_entries = GitStash {
                entries: vec![
                    StashEntry {
                        index: 0,
                        oid: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                            .parse()
                            .unwrap(),
                        message: "unrelated".into(),
                        branch: None,
                        timestamp: 0,
                    },
                    StashEntry {
                        index: 1,
                        oid: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                            .parse()
                            .unwrap(),
                        message: "touches file".into(),
                        branch: None,
                        timestamp: 0,
                    },
                ]
                .into(),
            };
        })
        .unwrap();

        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let editor = open_file_editor(&project, &workspace, "file.txt", cx).await;
        cx.focus(&editor);

        let repository = workspace
            .read_with(cx, |workspace, cx| compare_target(workspace, cx))
            .unwrap()
            .repository;
        let (tx, rx) = oneshot::channel();
        let historical = [RepoPath::new("file.txt").unwrap()];
        let stash_entries = repository.read_with(cx, |repository, _| {
            repository.cached_stash().entries
        });
        let mut matching = Vec::new();
        for entry in stash_entries.iter() {
            let revision = format!("stash@{{{}}}", entry.index);
            let paths = repository
                .update(cx, |repository, _| repository.changed_paths_at(revision))
                .await
                .unwrap()
                .unwrap();
            if paths.iter().any(|path| historical.contains(path)) {
                matching.push(entry.clone());
            }
        }
        assert_eq!(matching.len(), 1);
        assert_eq!(matching[0].index, 1);

        let picker = cx.new_window_entity(|window, cx| {
            StashRevisionPicker::new(matching, tx, window, cx)
        });
        picker
            .update_in(cx, |picker, window, cx| {
                picker.picker.update(cx, |picker, cx| {
                    picker.delegate.update_matches(String::new(), window, cx)
                })
            })
            .await;
        picker.update_in(cx, |picker, window, cx| {
            picker.picker.update(cx, |picker, cx| {
                picker.delegate.confirm(false, window, cx);
            });
        });
        match rx.await.unwrap() {
            CompareChoice::Revision(revision) => assert_eq!(revision, "stash@{1}"),
            other => panic!("expected stash revision, got {other:?}"),
        }
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
