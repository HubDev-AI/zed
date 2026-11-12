//! The city's files as Zed sees them: the first folder's worktree snapshot, its entry updates,
//! and the unsaved text of open buffers, turned into city_shell's source-agnostic input.

use std::{
    collections::{HashMap, HashSet},
    io::{Read as _, Seek as _, SeekFrom},
    path::PathBuf,
    sync::Arc,
};

use city_host::{Author, HostAuthor};
use city_model::PathsChanged;
use city_shell::{HostChange, Reader, Source};
use fs::{Fs, ReadSeek};
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use gpui::{App, AppContext as _, Context, Entity, EntityId, Subscription};
use language::{Buffer, BufferEditSource, BufferEvent};
use project::{
    Entry, MTime, Project, ProjectEntryId, UpdatedEntriesSet, WorktreeId,
    buffer_store::BufferStoreEvent,
};
use util::ResultExt as _;

/// The first visible folder of a local project as a city source, or `None` when there is none.
/// The snapshot may still be filling at startup; the files it lacks arrive as entry updates,
/// which the feed takes from the same point on.
pub(crate) fn source(
    project: &Entity<Project>,
    sender: UnboundedSender<HostChange>,
    changes: UnboundedReceiver<HostChange>,
    cx: &mut App,
) -> Option<Source> {
    if !project.read(cx).is_local() {
        return None;
    }
    let worktree = project.read(cx).visible_worktrees(cx).next()?;
    let snapshot = worktree.read(cx).snapshot();
    let root = snapshot.abs_path().to_path_buf();
    let listing: Vec<String> = snapshot
        .files(false, 0)
        .filter(|entry| shown(entry))
        .map(|entry| entry.path.as_unix_str().to_string())
        .collect();
    let shown = listing.iter().cloned().collect();
    let feed = cx.new(|cx| ZedFeed::new(project, snapshot.id(), shown, sender, cx));
    let reader = Arc::new(ZedReader {
        fs: project.read(cx).fs().clone(),
        root: root.clone(),
    });
    Some(Source {
        root,
        listing,
        reader,
        changes,
        keep_alive: Box::new(feed),
    })
}

/// A file the city shows: what Zed's own file traversal lists, less named pipes, which block the
/// reader that opens them.
fn shown(entry: &Entry) -> bool {
    entry.is_file() && !entry.is_fifo && (!entry.is_ignored || entry.is_always_included)
}

/// Saved contents of the folder's files, read through Zed's `Fs`.
struct ZedReader {
    fs: Arc<dyn Fs>,
    root: PathBuf,
}

impl ZedReader {
    /// `open_sync` is the one `Fs` read that does not hop onto the background executor, which is
    /// where the city calls the reader from; blocking on the others there could wait on itself.
    fn open(&self, path: &str) -> Option<Box<dyn ReadSeek + Send + Sync>> {
        futures::executor::block_on(self.fs.open_sync(&self.root.join(path))).ok()
    }
}

impl Reader for ZedReader {
    fn bytes(&self, path: &str) -> Option<u64> {
        let mut file = self.open(path)?;
        // A folder opens and seeks like a file but fails to read.
        file.by_ref().take(1).read_to_end(&mut Vec::new()).ok()?;
        file.seek(SeekFrom::End(0)).ok()
    }

    fn head(&self, path: &str, max: u64) -> Option<Vec<u8>> {
        let mut head = Vec::new();
        self.open(path)?.take(max).read_to_end(&mut head).ok()?;
        Some(head)
    }
}

/// Unsaved text a buffer has handed to the city: its path, its version, and who wrote it.
struct Held {
    path: String,
    version: clock::Global,
    author: Option<HostAuthor>,
}

struct ZedFeed {
    worktree_id: WorktreeId,
    /// The files the city shows, as root-relative paths.
    shown: HashSet<String>,
    /// The path and version of the unsaved text each buffer has handed to the city.
    held: HashMap<EntityId, Held>,
    /// Paths Zed just saved from a buffer, with the saved mtime: the worktree reports the same
    /// write once more, and the city has already reread it.
    saved: HashMap<String, Option<MTime>>,
    buffers: HashMap<EntityId, [Subscription; 2]>,
    sender: UnboundedSender<HostChange>,
    _subscriptions: [Subscription; 2],
}

impl ZedFeed {
    fn new(
        project: &Entity<Project>,
        worktree_id: WorktreeId,
        shown: HashSet<String>,
        sender: UnboundedSender<HostChange>,
        cx: &mut Context<Self>,
    ) -> Self {
        let buffer_store = project.read(cx).buffer_store().clone();
        let mut feed = Self {
            worktree_id,
            shown,
            held: HashMap::default(),
            saved: HashMap::default(),
            buffers: HashMap::default(),
            sender,
            _subscriptions: [
                cx.subscribe(project, |this, project, event: &project::Event, cx| {
                    if let project::Event::WorktreeUpdatedEntries(id, entries) = event {
                        this.worktree_changed(&project, *id, entries, cx);
                    }
                }),
                cx.subscribe(&buffer_store, |this, _, event: &BufferStoreEvent, cx| {
                    if let BufferStoreEvent::BufferAdded(buffer) = event {
                        this.watch_buffer(buffer, cx);
                    }
                }),
            ],
        };
        for buffer in project.read(cx).opened_buffers(cx) {
            feed.watch_buffer(&buffer, cx);
            feed.buffer_changed(&buffer, None, cx);
        }
        feed
    }

    fn send(&self, change: HostChange) {
        self.sender.unbounded_send(change).log_err();
    }

    fn worktree_changed(
        &mut self,
        project: &Entity<Project>,
        id: WorktreeId,
        entries: &UpdatedEntriesSet,
        cx: &mut Context<Self>,
    ) {
        if id != self.worktree_id {
            return;
        }
        let Some(worktree) = project.read(cx).worktree_for_id(id, cx) else {
            return;
        };
        let snapshot = worktree.read(cx).snapshot();
        let files_now: HashMap<String, (ProjectEntryId, Option<MTime>)> = entries
            .iter()
            .filter_map(|(path, _, _)| {
                let entry = snapshot.entry_for_path(path)?;
                shown(entry).then(|| (path.as_unix_str().to_string(), (entry.id, entry.mtime)))
            })
            .collect();
        let batch = entries
            .iter()
            .map(|(path, entry_id, _)| (path.as_unix_str().to_string(), *entry_id));
        let mut change = paths_changed(
            batch,
            |path| files_now.get(path).map(|(id, _)| *id),
            &mut self.shown,
        );
        // A held file shows its unsaved text, and the city rereads the disk when the text is
        // dropped; Zed's save reports the write before or after `Saved`, so both orders end in
        // that one reread.
        let held: HashSet<&String> = self.held.values().map(|held| &held.path).collect();
        change.modified.retain(|path| {
            let saved = self.saved.remove(path);
            !held.contains(path)
                && saved
                    .is_none_or(|saved| files_now.get(path).map(|(_, mtime)| *mtime) != Some(saved))
        });
        if !change.is_empty() {
            self.send(HostChange::Paths {
                change,
                author: None,
            });
        }
    }

    fn watch_buffer(&mut self, buffer: &Entity<Buffer>, cx: &mut Context<Self>) {
        let id = buffer.entity_id();
        if self.buffers.contains_key(&id) {
            return;
        }
        let events = cx.subscribe(
            buffer,
            |this, buffer, event: &BufferEvent, cx| match event {
                BufferEvent::Edited { source } => {
                    this.buffer_changed(&buffer, edit_author(*source), cx)
                }
                BufferEvent::DirtyChanged | BufferEvent::Reloaded => {
                    this.buffer_changed(&buffer, None, cx)
                }
                BufferEvent::Saved => this.buffer_saved(&buffer, cx),
                _ => {}
            },
        );
        let release = cx.observe_release(buffer, move |this, _, _| this.buffer_released(id));
        self.buffers.insert(id, [events, release]);
    }

    fn path_in_worktree(&self, buffer: &Buffer, cx: &App) -> Option<String> {
        buffer
            .file()
            .filter(|file| file.worktree_id(cx) == self.worktree_id)
            .map(|file| file.path().as_unix_str().to_string())
    }

    /// Hands the buffer's text to the city while it differs from the disk, credited to `author`,
    /// and takes it back once it does not. One edit also flips the dirty bit, so a version
    /// already handed over is not sent again. A reload from disk leaves the buffer clean, so its
    /// untagged edit never reaches the city as the user's.
    fn buffer_changed(&mut self, buffer: &Entity<Buffer>, author: Option<HostAuthor>, cx: &App) {
        let id = buffer.entity_id();
        let buffer = buffer.read(cx);
        let version = buffer.version();
        let held = self
            .path_in_worktree(buffer, cx)
            .filter(|_| buffer.is_dirty());
        if let Some(held_now) = self.held.get(&id) {
            if held.as_ref() == Some(&held_now.path) && held_now.version == version {
                return;
            }
        }
        if let Some(old) = self.held.remove(&id) {
            if held.as_ref() != Some(&old.path) {
                self.send(HostChange::Text {
                    path: old.path,
                    text: None,
                    author: None,
                });
            }
        }
        if let Some(path) = held {
            self.held.insert(
                id,
                Held {
                    path: path.clone(),
                    version,
                    author: author.clone(),
                },
            );
            self.send(HostChange::Text {
                path,
                text: Some(Arc::from(buffer.text())),
                author,
            });
        }
    }

    /// The city rereads a saved file from the buffer's side, so the worktree's report of the same
    /// write is dropped whether or not the buffer's dirty bit cleared first.
    fn buffer_saved(&mut self, buffer: &Entity<Buffer>, cx: &App) {
        let id = buffer.entity_id();
        let buffer = buffer.read(cx);
        let Some(path) = self.path_in_worktree(buffer, cx) else {
            return;
        };
        self.saved.insert(path.clone(), buffer.saved_mtime());
        let held = self.held.remove(&id);
        let author = held.as_ref().and_then(|held| held.author.clone());
        let stale = held.map(|held| held.path).filter(|old| *old != path);
        self.send(HostChange::Text {
            path,
            text: None,
            author,
        });
        if let Some(old) = stale {
            self.send(HostChange::Text {
                path: old,
                text: None,
                author: None,
            });
        }
    }

    fn buffer_released(&mut self, id: EntityId) {
        self.buffers.remove(&id);
        if let Some(held) = self.held.remove(&id) {
            self.send(HostChange::Text {
                path: held.path,
                text: None,
                author: None,
            });
        }
    }
}

/// Who wrote a buffer edit, as far as the buffer says: an agent tool, or a person here. Edits
/// from another replica name no one.
fn edit_author(source: BufferEditSource) -> Option<HostAuthor> {
    let author = match source {
        BufferEditSource::Agent => Author::Agent,
        BufferEditSource::User => Author::User,
        BufferEditSource::Remote => return None,
    };
    Some(HostAuthor {
        author,
        thread: None,
    })
}

/// One worktree batch as the city's changes. The batch's change kinds are only hints: whether a
/// path is a shown file now comes from `now`, which gives its entry id. A path removed and added
/// in one batch (a save that writes a new file over the old one) is therefore an edit, and an id
/// that leaves one path and appears at another is a rename.
fn paths_changed(
    batch: impl IntoIterator<Item = (String, ProjectEntryId)>,
    now: impl Fn(&str) -> Option<ProjectEntryId>,
    shown: &mut HashSet<String>,
) -> PathsChanged {
    let mut seen = HashSet::new();
    let mut gone: HashMap<ProjectEntryId, String> = HashMap::default();
    let mut here = Vec::new();
    for (path, id) in batch {
        if !seen.insert(path.clone()) {
            continue;
        }
        match now(&path) {
            Some(current) => here.push((current, path)),
            None if shown.contains(&path) => {
                gone.insert(id, path);
            }
            None => {}
        }
    }
    let mut change = PathsChanged::default();
    for (id, path) in here {
        if shown.contains(&path) {
            change.modified.push(path);
        } else if let Some(from) = gone.remove(&id) {
            shown.remove(&from);
            shown.insert(path.clone());
            change.renamed.push((from, path));
        } else {
            shown.insert(path.clone());
            change.created.push(path);
        }
    }
    for path in gone.into_values() {
        shown.remove(&path);
        change.deleted.push(path);
    }
    change
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        fs,
        time::{Duration, Instant},
    };

    use city_host::{Author, HostAuthor};
    use city_model::PathsChanged;
    use city_shell::{HostChange, Source};
    use futures::channel::mpsc::UnboundedReceiver;
    use gpui::TestAppContext;
    use language::BufferEditSource;
    use project::{Project, ProjectEntryId, RealFs};
    use settings::SettingsStore;

    use super::{paths_changed, source};

    fn shown(paths: &[&str]) -> HashSet<String> {
        paths.iter().map(|path| path.to_string()).collect()
    }

    fn id(n: u64) -> ProjectEntryId {
        ProjectEntryId::from_proto(n)
    }

    // A save that writes a new file over the old one gives the path a new entry id; the city must
    // see one edit, not a deletion and a new building.
    #[test]
    fn a_path_removed_and_added_in_one_batch_is_an_edit() {
        let mut files = shown(&["src/main.rs"]);
        let batch = [
            ("src/main.rs".to_string(), id(1)),
            ("src/main.rs".to_string(), id(2)),
        ];
        let change = paths_changed(batch, |_| Some(id(2)), &mut files);
        assert_eq!(change.modified, ["src/main.rs"]);
        assert!(
            change.created.is_empty() && change.deleted.is_empty() && change.renamed.is_empty()
        );
    }

    // The entry id keeps a moved file the same building, whichever half of the move comes first.
    #[test]
    fn an_id_that_moves_to_a_new_path_is_a_rename() {
        let mut files = shown(&["src/parse.rs"]);
        let batch = [
            ("src/read.rs".to_string(), id(7)),
            ("src/parse.rs".to_string(), id(7)),
        ];
        let change = paths_changed(
            batch,
            |path| (path == "src/read.rs").then_some(id(7)),
            &mut files,
        );
        assert_eq!(
            change.renamed,
            [("src/parse.rs".to_string(), "src/read.rs".to_string())]
        );
        assert_eq!(files, shown(&["src/read.rs"]));
    }

    // A shown file that turns ignored or disappears leaves the city; paths the city never showed,
    // such as folders, change nothing.
    #[test]
    fn paths_that_stop_being_shown_files_are_deleted() {
        let mut files = shown(&["a.rs", "b.rs"]);
        let batch = [
            ("a.rs".to_string(), id(1)),
            ("src".to_string(), id(3)),
            ("c.rs".to_string(), id(4)),
        ];
        let change = paths_changed(batch, |path| (path == "c.rs").then_some(id(4)), &mut files);
        assert_eq!(change.deleted, ["a.rs"]);
        assert_eq!(change.created, ["c.rs"]);
        assert_eq!(files, shown(&["b.rs", "c.rs"]));
    }

    const SETTLE: Duration = Duration::from_secs(2);
    const POLL: Duration = Duration::from_millis(20);

    /// Every message the feed sends while Zed settles, after `SETTLE` of quiet on disk.
    fn settled(
        changes: &mut UnboundedReceiver<HostChange>,
        cx: &mut TestAppContext,
    ) -> Vec<HostChange> {
        let mut sent = Vec::new();
        let started = Instant::now();
        while started.elapsed() < SETTLE {
            cx.executor().advance_clock(project::FS_WATCH_LATENCY);
            cx.run_until_parked();
            while let Ok(change) = changes.try_recv() {
                sent.push(change);
            }
            std::thread::sleep(POLL);
        }
        sent
    }

    fn paths(sent: &[HostChange]) -> PathsChanged {
        let mut merged = PathsChanged::default();
        for change in sent {
            if let HostChange::Paths { change, .. } = change {
                merged.merge(change.clone());
            }
        }
        merged
    }

    // Through Zed's real worktree and buffers: unsaved text reaches the city as text, a save
    // reaches it once (the worktree's report of the same write is dropped), and a rename on disk
    // keeps the file.
    #[gpui::test]
    async fn zed_tells_the_city_of_unsaved_text_saves_and_renames(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
        });
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(root.join("src/parse.rs"), "pub fn parse() {}\n").unwrap();
        let project = Project::test(RealFs::new(None, cx.executor()), [root.as_path()], cx).await;
        let Source {
            mut listing,
            reader,
            mut changes,
            keep_alive,
            ..
        } = cx
            .update(|cx| {
                let (sender, changes) = futures::channel::mpsc::unbounded();
                source(&project, sender, changes, cx)
            })
            .unwrap();
        listing.sort();
        assert_eq!(listing, ["src/main.rs", "src/parse.rs"]);
        assert_eq!(reader.bytes("src/main.rs"), Some(13));
        assert_eq!(reader.head("src/main.rs", 2).as_deref(), Some(&b"fn"[..]));
        assert_eq!(reader.bytes("src"), None, "a folder is no file");
        assert_eq!(reader.bytes("src/gone.rs"), None);
        settled(&mut changes, cx);

        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(root.join("src/main.rs"), cx)
            })
            .await
            .unwrap();
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(0..0, "// held\n")], None, cx)
        });
        let sent = settled(&mut changes, cx);
        assert_eq!(
            sent,
            [HostChange::Text {
                path: "src/main.rs".into(),
                text: Some("// held\nfn main() {}\n".into()),
                author: Some(HostAuthor {
                    author: Author::User,
                    thread: None,
                }),
            }]
        );

        project
            .update(cx, |project, cx| project.save_buffer(buffer.clone(), cx))
            .await
            .unwrap();
        let sent = settled(&mut changes, cx);
        let rereads = sent
            .iter()
            .filter(|change| match change {
                HostChange::Text {
                    path, text: None, ..
                } => path == "src/main.rs",
                HostChange::Paths { change, .. } => {
                    change.modified.iter().any(|p| p == "src/main.rs")
                }
                HostChange::Text { .. } | HostChange::Agent { .. } => false,
            })
            .count();
        assert_eq!(rereads, 1, "{sent:?}");

        // A write to disk under the clean buffer reloads it with an untagged edit; that is not
        // the user typing, so no text reaches the city as the user's.
        fs::write(root.join("src/main.rs"), "fn main() { reloaded() }\n").unwrap();
        let sent = settled(&mut changes, cx);
        assert!(
            sent.iter().all(|change| !matches!(
                change,
                HostChange::Text {
                    author: Some(HostAuthor {
                        author: Author::User,
                        ..
                    }),
                    ..
                }
            )),
            "{sent:?}"
        );
        assert_eq!(paths(&sent).modified, ["src/main.rs"], "{sent:?}");

        buffer.update(cx, |buffer, cx| {
            buffer.start_transaction();
            buffer.edit([(0..0, "// agent\n")], None, cx);
            buffer.end_transaction_with_source(BufferEditSource::Agent, cx);
        });
        let sent = settled(&mut changes, cx);
        assert!(
            matches!(
                sent.as_slice(),
                [HostChange::Text {
                    text: Some(_),
                    author: Some(HostAuthor {
                        author: Author::Agent,
                        thread: None,
                    }),
                    ..
                }]
            ),
            "{sent:?}"
        );

        fs::rename(root.join("src/parse.rs"), root.join("src/read.rs")).unwrap();
        let sent = settled(&mut changes, cx);
        assert_eq!(
            paths(&sent).renamed,
            [("src/parse.rs".to_string(), "src/read.rs".to_string())],
            "{sent:?}"
        );
        drop(keep_alive);
    }

    async fn make_fifo(path: &std::path::Path) {
        let status = util::command::new_command("mkfifo")
            .arg(path)
            .status()
            .await
            .unwrap();
        assert!(status.success());
    }

    // Opening a named pipe blocks until a writer comes, which would stall the city's first scan
    // forever, whether the pipe was there at the start or appears later.
    #[gpui::test]
    async fn a_named_pipe_is_never_shown(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
        });
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        fs::write(root.join("plain.txt"), "plain\n").unwrap();
        make_fifo(&root.join("pipe")).await;
        let project = Project::test(RealFs::new(None, cx.executor()), [root.as_path()], cx).await;
        let mut source = cx.update(|cx| {
            let (sender, changes) = futures::channel::mpsc::unbounded();
            source(&project, sender, changes, cx).unwrap()
        });
        assert_eq!(source.listing, ["plain.txt"]);
        settled(&mut source.changes, cx);
        make_fifo(&root.join("later")).await;
        let sent = settled(&mut source.changes, cx);
        assert!(
            !paths(&sent).created.iter().any(|path| path == "later"),
            "{sent:?}"
        );
    }

    // Zed's file_scan_inclusions bring a git-ignored file back into the worktree; the city lists it
    // as Zed does, so an edit must not then remove it.
    #[gpui::test]
    async fn an_ignored_file_zed_scans_anyway_stays_shown_when_edited(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
        });
        cx.update(|cx| {
            gpui::BorrowAppContext::update_global::<SettingsStore, _>(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.project.worktree.file_scan_inclusions =
                        Some(settings::SplicingVec::from(vec![
                            "build/keep.txt".to_string(),
                        ]));
                });
            });
        });
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".gitignore"), "build/\n").unwrap();
        fs::create_dir_all(root.join("build")).unwrap();
        fs::write(root.join("build/keep.txt"), "keep\n").unwrap();
        fs::write(root.join("build/other.txt"), "other\n").unwrap();
        let project = Project::test(RealFs::new(None, cx.executor()), [root.as_path()], cx).await;
        settled_project(cx);
        let mut source = cx.update(|cx| {
            let (sender, changes) = futures::channel::mpsc::unbounded();
            source(&project, sender, changes, cx).unwrap()
        });
        settled(&mut source.changes, cx);
        fs::write(root.join("build/keep.txt"), "keep, edited\n").unwrap();
        let sent = settled(&mut source.changes, cx);
        assert!(source.listing.iter().any(|path| path == "build/keep.txt"));
        assert!(!source.listing.iter().any(|path| path == "build/other.txt"));
        assert_eq!(paths(&sent).modified, ["build/keep.txt"], "{sent:?}");
        assert!(paths(&sent).deleted.is_empty(), "{sent:?}");
    }

    fn settled_project(cx: &mut TestAppContext) {
        for _ in 0..50 {
            cx.executor().advance_clock(project::FS_WATCH_LATENCY);
            cx.run_until_parked();
            std::thread::sleep(POLL);
        }
    }
}
