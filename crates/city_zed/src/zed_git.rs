//! The city's git as Zed reads it: the repository Zed's git store holds for the city's folder,
//! read through that repository's own git backend, so the city counts what Zed's git panel does.

use std::sync::Arc;

use city_host::git::{Git, GitResult, Kind, Status, Text, TreeChange};
use futures::{
    FutureExt as _,
    channel::{
        mpsc::{UnboundedReceiver, UnboundedSender, unbounded},
        oneshot,
    },
    future::Shared,
};
use git::{
    repository::{GitRepository, RepoPath},
    status::{DiffTreeType, FileStatus, StatusCode, TreeDiffStatus},
};
use gpui::{App, AppContext as _, Context, Entity, Subscription, Task};
use parking_lot::Mutex;
use project::{
    Project, ProjectPath, WorktreeId,
    git_store::{
        GitStore, GitStoreEvent, LocalRepositoryState, RepositoryEvent, RepositoryId,
        RepositoryState,
    },
};
use util::{ResultExt as _, rel_path::RelPath};

const NO_GIT: &str = "not a git repository";

/// The folder's repository, read from whichever thread the city reads it on.
pub(crate) struct ZedGit {
    repository: Mutex<Option<Found>>,
    /// Resolves once the repository found after Zed's scan of the folder is set, so the first
    /// answer is not an unscanned "not a git repository"; cancelled if the follower goes first.
    scanned: Shared<oneshot::Receiver<()>>,
    listeners: Mutex<Vec<UnboundedSender<()>>>,
}

#[derive(Clone)]
struct Found {
    backend: Arc<dyn GitRepository>,
    /// The folder's path below the repository's top.
    prefix: RepoPath,
}

/// Keeps a [`ZedGit`] on the repository Zed's git store finds for the folder, which may arrive
/// after the city opens, and tells its listeners when that repository's state changes.
pub(crate) struct GitFollower {
    git: Arc<ZedGit>,
    root: ProjectPath,
    repository: Option<RepositoryId>,
    scanned: Option<oneshot::Sender<()>>,
    _scanning: Task<()>,
    _finding: Task<()>,
    _subscription: Option<Subscription>,
}

/// The git of `worktree_id`'s folder, and the follower that must be held while the city reads it.
pub(crate) fn follow(
    project: &Entity<Project>,
    worktree_id: WorktreeId,
    cx: &mut App,
) -> (Arc<ZedGit>, Entity<GitFollower>) {
    let (scanned_sender, scanned) = oneshot::channel();
    let git = Arc::new(ZedGit {
        repository: Mutex::default(),
        scanned: scanned.shared(),
        listeners: Mutex::default(),
    });
    let git_store = project.read(cx).git_store().clone();
    let scan = (project.read(cx).worktree_for_id(worktree_id, cx))
        .and_then(|worktree| Some(worktree.read(cx).as_local()?.scan_complete()));
    let follower = cx.new(|cx| GitFollower {
        git: git.clone(),
        root: ProjectPath {
            worktree_id,
            path: RelPath::empty_arc(),
        },
        repository: None,
        scanned: Some(scanned_sender),
        // Zed's git store takes in the repositories a scan finds before this task resumes, as
        // `Project::git_scans_complete` relies on.
        _scanning: cx.spawn(async move |this, cx| {
            if let Some(scan) = scan {
                scan.await;
            }
            this.update(cx, |this: &mut GitFollower, cx| {
                this._subscription = Some(cx.subscribe(
                    &git_store,
                    |this: &mut GitFollower, git_store, event, cx| match event {
                        GitStoreEvent::RepositoryAdded | GitStoreEvent::RepositoryRemoved(_) => {
                            this.find(&git_store, cx)
                        }
                        GitStoreEvent::RepositoryUpdated(
                            id,
                            RepositoryEvent::StatusesChanged
                            | RepositoryEvent::HeadChanged
                            | RepositoryEvent::BranchListChanged,
                            _,
                        ) if this.repository == Some(*id) => this.git.touched(),
                        _ => {}
                    },
                ));
                this.find(&git_store, cx);
            })
            .log_err();
        }),
        _finding: Task::ready(()),
        _subscription: None,
    });
    (git, follower)
}

impl GitFollower {
    fn find(&mut self, git_store: &Entity<GitStore>, cx: &mut Context<Self>) {
        let found = git_store
            .read(cx)
            .repository_and_path_for_project_path(&self.root, cx);
        self.repository = found.as_ref().map(|(repository, _)| repository.read(cx).id);
        let Some((repository, prefix)) = found else {
            self._finding = Task::ready(());
            self.set(None);
            return;
        };
        let backend = repository.update(cx, |repository, _| {
            repository.send_job("city", None, |state, _| async move {
                match state {
                    RepositoryState::Local(LocalRepositoryState { backend, .. }) => Some(backend),
                    RepositoryState::Remote(_) => None,
                }
            })
        });
        self._finding = cx.spawn(async move |this, cx| {
            let backend = backend.await.ok().flatten();
            this.update(cx, |this, _| {
                this.set(backend.map(|backend| Found { backend, prefix }))
            })
            .log_err();
        });
    }

    fn set(&mut self, found: Option<Found>) {
        self.git.set(found);
        if let Some(scanned) = self.scanned.take() {
            scanned.send(()).ok();
        }
    }
}

impl ZedGit {
    fn set(&self, found: Option<Found>) {
        *self.repository.lock() = found;
        self.touched();
    }

    fn touched(&self) {
        (self.listeners.lock()).retain(|listener| listener.unbounded_send(()).is_ok());
    }

    async fn found(&self) -> Result<Found, String> {
        (self.scanned.clone().await).map_err(|_| NO_GIT.to_string())?;
        (self.repository.lock().clone()).ok_or_else(|| NO_GIT.to_string())
    }
}

impl Found {
    /// `path`, given relative to the repository's top, relative to the folder instead; `None`
    /// when it is outside the folder.
    fn under_root(&self, path: &RepoPath) -> Option<String> {
        let path = path.strip_prefix(&self.prefix).ok()?;
        Some(path.as_unix_str().to_string())
    }

    fn top(&self, path: &str) -> String {
        match self.prefix.is_empty() {
            true => path.to_string(),
            false => format!("{}/{path}", self.prefix.as_unix_str()),
        }
    }
}

impl Git for ZedGit {
    fn head(&self) -> GitResult<'_, Option<String>> {
        async move {
            let found = self.found().await?;
            Ok(found.backend.head_sha().await)
        }
        .boxed()
    }

    fn default_branch(&self) -> GitResult<'_, Option<String>> {
        async move {
            let found = self.found().await?;
            let branch = found.backend.default_branch(true).await.map_err(reason)?;
            Ok(branch.map(String::from))
        }
        .boxed()
    }

    fn committed(&self, base: &str) -> GitResult<'_, Vec<(String, TreeChange)>> {
        let base = base.to_string();
        async move {
            let found = self.found().await?;
            let request = DiffTreeType::MergeBase {
                base: base.clone().into(),
                head: "HEAD".into(),
            };
            let diff = found.backend.diff_tree(request).await.map_err(|error| {
                let reason = reason(error);
                if reason.contains("no merge base") {
                    format!("no merge base with {base}")
                } else if reason.contains("multiple merge bases") {
                    format!("more than one merge base with {base}")
                } else {
                    reason
                }
            })?;
            Ok((diff.entries.into_iter())
                .filter_map(|(path, change)| {
                    let change = match change {
                        TreeDiffStatus::Added => TreeChange::Added,
                        TreeDiffStatus::Modified { old } => TreeChange::Modified {
                            old: old.to_string(),
                        },
                        TreeDiffStatus::Deleted { old } => TreeChange::Deleted {
                            old: old.to_string(),
                        },
                    };
                    Some((found.under_root(&path)?, change))
                })
                .collect())
        }
        .boxed()
    }

    fn status(&self) -> GitResult<'_, Vec<(String, Status)>> {
        async move {
            let found = self.found().await?;
            let listed = (found.backend)
                .status(std::slice::from_ref(&found.prefix))
                .await
                .map_err(reason)?;
            Ok((listed.entries.iter())
                .filter_map(|(path, status)| {
                    let status = match status {
                        FileStatus::Untracked => Status::Untracked,
                        FileStatus::Ignored => return None,
                        FileStatus::Unmerged(_) => Status::Conflicted,
                        // Zed folds git's `D ` and `??` lines for a file taken out of the index
                        // but kept on disk into one entry; git and the standalone app call it
                        // untracked.
                        FileStatus::Tracked(tracked)
                            if tracked.index_status == StatusCode::Deleted
                                && tracked.worktree_status == StatusCode::Added =>
                        {
                            Status::Untracked
                        }
                        FileStatus::Tracked(tracked) => Status::Tracked {
                            staged: kind(tracked.index_status),
                            unstaged: kind(tracked.worktree_status),
                        },
                    };
                    Some((found.under_root(path)?, status))
                })
                .collect())
        }
        .boxed()
    }

    /// Every text in one `load_revisions` batch, by the names `load_blob_content`,
    /// `load_committed_text` and `load_index_text` give it one at a time.
    fn texts(&self, texts: Vec<Text>) -> GitResult<'_, Vec<Option<Vec<u8>>>> {
        async move {
            let found = self.found().await?;
            let names: Vec<Option<String>> = (texts.into_iter())
                .map(|text| match text {
                    Text::Blob(oid) => oid,
                    Text::Head(path) => format!("HEAD:{}", found.top(&path)),
                    // Stage 0 named outright, so a path such as `1:x.rs` is not read as a stage.
                    Text::Index(path) => format!(":0:{}", found.top(&path)),
                })
                // A name with a line break fails the whole batch.
                .map(|name| (!name.contains('\n')).then_some(name))
                .collect();
            let wanted = names.iter().flatten().cloned().collect();
            let mut loaded = (found.backend.load_revisions(wanted).await)
                .map_err(reason)?
                .into_iter();
            Ok((names.iter())
                .map(|name| name.as_ref().and_then(|_| loaded.next().flatten()))
                .collect())
        }
        .boxed()
    }

    fn changes(&self) -> UnboundedReceiver<()> {
        let (sender, receiver) = unbounded();
        self.listeners.lock().push(sender);
        receiver
    }
}

fn kind(code: StatusCode) -> Option<Kind> {
    match code {
        StatusCode::Added => Some(Kind::Added),
        StatusCode::Deleted => Some(Kind::Deleted),
        StatusCode::Modified | StatusCode::TypeChanged => Some(Kind::Modified),
        StatusCode::Renamed | StatusCode::Copied | StatusCode::Unmodified => None,
    }
}

fn reason(error: impl std::fmt::Display) -> String {
    format!("{error:#}")
}
