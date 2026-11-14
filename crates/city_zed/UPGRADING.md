# Upgrading city_zed to a new Zed tag

Every Zed API `city_zed` uses, with its place at `v1.28.1` (`e8bc16acee`). At a tag bump:

1. Give the new tag its own base branch on the fork, at exactly the tag's commit: `git push origin <new-commit>:refs/heads/eof-<new>`. Older base branches stay as they are, so each tag's pull request keeps its exact review base.
2. Rebase the fork branch onto the tag: `git rebase --onto <new> <old> <branch>`. `script/fork-apply` stops when `crates/city_zed` is not there yet.
3. Start from the tag's lock and run fork-apply (eof is private): `git checkout <new> -- Cargo.lock && CARGO_NET_GIT_FETCH_WITH_CLI=true script/fork-apply`. Do this whenever the eof pin moves too, so Zed's own crates keep the tag's versions and only what eof needs newer moves.
4. Move eof's `gpui` pin to the same tag, point `city_zed` at that eof revision and repeat step 3.
5. Find the upstream changes the compiler will name, then check each line below:
   - `git diff <old> <new> -- Cargo.toml | grep agent-client-protocol`
   - `git diff <old> <new> -- crates/acp_thread/src/acp_thread.rs | grep -A3 'pub enum'`
6. Build and test. Without Xcode's Metal toolchain the shaders compile only at run time, hence the features. The release channel is fixed at build time and the tag's is stable, which would share the installed Zed's single-instance port and database and check for updates, so build on the dev channel:
   - `ZED_RELEASE_CHANNEL=dev ZED_UPDATE_EXPLANATION="This build does not update itself." cargo build --profile release-fast -p zed --features gpui_platform/runtime_shaders`
   - `cargo test -p city_zed --features gpui_apple/runtime_shaders`
7. Push the branch with `--force-with-lease=<branch>:<its old head>` and retarget its pull request: `gh pr edit <number> --base eof-<new>`. Its diff is then only the fork's own change.
8. Once it is merged, set `FORK_BRANCH` in eof's `xtask/src/nightly.rs` to `eof-<new>`, so eof's nightly checks the new base branch.

The only Zed files `script/fork-apply` changes are:
- the root `Cargo.toml`;
- `crates/zed/Cargo.toml`;
- `crates/zed/src/main.rs`;
- `crates/zed/src/zed.rs`, where `test_action_namespaces` lists every action namespace the binary registers and gains `city`;
- `assets/keymaps/default-macos.json`, whose `CityView` block fork-apply replaces on every run with eof's `crates/input_keys/keymap.json` at the pinned revision; `zed_keymap_holds_eofs_key_table` fails when they differ;
- `Cargo.lock`.

The fork's own change to Zed, carried by the rebase, is `GitRepository::blob_sizes` in `crates/git/src/repository.rs`, with an `unimplemented!()` `FakeGitRepository::blob_sizes` in `crates/fs/src/fake_git_repo.rs`.

Removing the city takes the `city_zed::init` line, the `CityView` keymap block and the `city` namespace in `test_action_namespaces`.

## Registration and the tab (`city_item.rs`)

| API | at v1.28.1 |
|---|---|
| `App::observe_new` | `crates/gpui/src/app.rs:2646` |
| `Workspace::register_action` | `crates/workspace/src/workspace.rs:8698` |
| `Workspace::items_of_type`, `activate_item`, `add_item_to_active_pane` | `workspace.rs:4405`, `:5761`, `:5055` |
| `Workspace::project`, `panel::<AgentPanel>` | `workspace.rs:2939`, `:4942` |
| `workspace::Item`, `tab_content_text` | `crates/workspace/src/item.rs:170`, `:187` |
| `gpui_apple::AppleTextSystem::new` (`font-kit` feature) | `crates/gpui_apple/src/text_system.rs:84` |
| `<dyn Fs>::global`, `paths::settings_file` | `crates/fs/src/fs.rs:285`, `crates/paths/src/paths.rs:278` |

## Files from Zed (`zed_feed.rs`)

| API | at v1.28.1 |
|---|---|
| `Project::is_local`, `visible_worktrees`, `worktree_for_id`, `opened_buffers`, `buffer_store` | `crates/project/src/project.rs:3152`, `:2522`, `:2585`, `:2360`, `:6767` |
| `project::Event::WorktreeUpdatedEntries`, `UpdatedEntriesSet` | `project.rs:384`, `crates/worktree/src/worktree.rs:4068` |
| `Worktree::snapshot`, `is_single_file`; `Snapshot::id`, `abs_path`, `files`, `entry_for_path` | `worktree.rs:823`, `:1275`, `:2554`, `:2570`, `:2835`, `:2907` |
| `Entry { id, is_ignored, is_external, is_fifo }`, `is_file` | `worktree.rs:3987`, `:4196` |
| `BufferStoreEvent::BufferAdded` | `crates/project/src/buffer_store.rs:92` |
| `BufferEvent::{Edited { source }, DirtyChanged, Saved, FileHandleChanged, Reloaded}`, `BufferEditSource` | `crates/language/src/buffer.rs:333`-`:341`, `:311` |
| `Buffer::file`, `is_dirty`; `File::path`, `worktree_id` | `buffer.rs:1523`, `:2534`, `:373`, `:395` |
| `text::BufferSnapshot::version`, `text`, `len` (a text over eof's size cap is not handed over) | `crates/text/src/text.rs:2269`, `:2193`, `:2131` |
| `RelPath::as_unix_str` | `crates/path/src/rel_path.rs:268` |
| The default `file_scan_exclusions`: eof's `city_host::UNSHOWN` copies them, and `unshown_copies_zeds_default_exclusions` fails when they differ | `assets/settings/default.json:1788` |
| `Project::fs`; `Fs::open_sync` (the reader blocks on it, so it must not hop onto the background executor), `ReadSeek` | `project.rs:2396`; `fs.rs:137`, `:79` |

## Git (`zed_git.rs`)

The city's branch changes come from the repository Zed's git store finds for the folder, read through that repository's backend. Each answer must stay the standalone app's: `the_city_tab_counts_a_branch_as_the_standalone_app_does` and `the_city_tab_says_plainly_why_it_has_no_branch` hold eof's answers for the same repositories.

| API | at v1.28.1 |
|---|---|
| `Project::git_store`, `ProjectPath`; `GitStore::repository_and_path_for_project_path` | `crates/project/src/project.rs:6771`, `:463`; `crates/project/src/git_store.rs:3202` |
| `GitStoreEvent::{RepositoryAdded, RepositoryRemoved, RepositoryUpdated}`, `RepositoryEvent::{StatusesChanged, HeadChanged, BranchListChanged}` | `git_store.rs:871`, `:856` |
| `Worktree::as_local`, `LocalWorktree::scan_complete`: the port's first answer waits for the folder's scan, then for the repository it finds | `crates/worktree/src/worktree.rs:776`, `:1616` |
| `Repository::send_job` (the one way to its backend), `RepositoryState::Local`, `LocalRepositoryState::backend`; `RepositorySnapshot::id` through `Repository`'s `Deref` | `git_store.rs:6780`, `:826`, `:770`; `:602`, `:761` |
| `GitRepository::head_sha`, `default_branch(include_remote_name)` (the base's order, which eof's port copies), `diff_tree`, `status` | `crates/git/src/repository.rs:831`, `:1091`, `:846`, `:845` |
| `GitRepository::load_revisions`: one `cat-file --batch` for all texts, by the names `load_index_text` (`:path`), `load_committed_text` (`HEAD:path`) and `load_blob_content` (the blob id) take one at a time; it fails the batch on a name with a line break. `capped_texts` gives it only the ids of blobs within eof's size cap | `repository.rs:828`, `:793`, `:801`, `:805` |
| `GitRepository::blob_sizes`, the fork's own (not in the tag): one `cat-file --batch-check` sizing every name without reading it, so `capped_texts` leaves a blob over the cap unread | `repository.rs:833`, `RealGitRepository` `:1888` on the fork branch |
| Git's own words in `diff_tree`'s error (`no merge base found`, `multiple merge bases found`), which the port's reasons are read from | `repository.rs:1969` |
| `--no-optional-locks` on every command, so reading never writes the index; `git_status_args` (`--untracked-files=all --no-renames`, the standalone app's) | `repository.rs:3806`, `:3662` |
| `DiffTreeType::MergeBase`, `TreeDiff`, `TreeDiffStatus`, `GitStatus`, `FileStatus`, `TrackedStatus`, `StatusCode` | `crates/git/src/status.rs:501`, `:516`, `:521`, `:431`, `:10`, `:31`, `:37` |
| `TreeDiff`'s parser skips a committed type change (git's `T`, such as a file turned symlink), so the tab leaves out a change the standalone app counts as modified | `status.rs:556`-`:561` |
| `GitStatus`'s parser folds a file taken out of the index but kept on disk (git's `D ` and `??`) into one index-Deleted, worktree-Added entry, which `zed_git.rs` gives back as untracked | `status.rs:462`-`:484` |
| `RepoPath`; `RelPath::empty_arc`, `strip_prefix` | `repository.rs:4040`; `crates/path/src/rel_path.rs:45`, `:200` |

## Settings (`zed_settings.rs`)

| API | at v1.28.1 |
|---|---|
| `settings::watch_config_file` (its text is only a change signal: the file is read again under the store's lock) | `crates/settings/src/settings_file.rs:171` |
| `Fs::metadata` (`None` for a missing file), `load`, `canonicalize`, `atomic_write`; the store's start-time `load` blocks the main thread on `metadata` and `load`, so they must finish without it | `crates/fs/src/fs.rs:148`, `:138`, `:145`, `:142` |
| `settings_json::parse_json_with_comments`, `replace_value_in_json_text` (`editing` feature), `infer_json_indent_size` | `crates/settings_json/src/settings_json.rs:782`, `:105`, `:662` |

## Agents (`zed_agents.rs`): the fast-moving part

`agent_ui` and `acp_thread` change often upstream. `agent_ui` is used only in `follow`, `rescan` and `root_thread`, and in `city_item.rs`'s `open`. The rest of the module reads `AcpThread`.

| API | at v1.28.1 |
|---|---|
| `agent_ui::AgentPanel`, `ConversationView` exports | `crates/agent_ui/src/agent_ui.rs:79`, `:88` |
| `AgentPanel::conversation_views` | `crates/agent_ui/src/agent_panel.rs:4145` |
| `ConversationView::root_thread_view`, `ThreadView.thread` | `crates/agent_ui/src/conversation_view.rs:896`, `conversation_view/thread_view.rs:737` |
| `workspace::Event::PanelAdded` | `workspace.rs:1520` |
| `AcpThreadEvent` | `crates/acp_thread/src/acp_thread.rs:3831` |
| `AcpThread::foreground_activity`, `ForegroundActivity` (the state Zed itself shows) | `acp_thread.rs:4422`, `crates/acp_thread/src/submission.rs:352` |
| `AcpThread::session_id`, `status`, `entries`, `title`, `connection`, `subagent`, `subagents` | `acp_thread.rs:4280`, `:4341`, `:4202`, `:4191`, `:4179`, `:6143`, `:6147` |
| `ThreadStatus`, `AgentThreadEntry`, `ToolCallStatus` | `acp_thread.rs:3913`, `:565`, `:2027` |
| `ToolCall { id, label, locations }`, `kind`, `status`; `ToolCallLocation` | `acp_thread.rs:1181`, `:1499`, `:1505`, `:1145` |
| `AgentConnection::agent_id` | `crates/acp_thread/src/connection.rs:96` |
| `Markdown::source` (the call's label) | `crates/markdown/src/markdown.rs:997` |
| `agent_client_protocol::v2::schema::{ToolKind, StopReason}` | `agent-client-protocol` 5.0.0, `agent-client-protocol-schema` 1.15.1 |

## Tests only

| API | at v1.28.1 |
|---|---|
| `Project::test`, `RealFs` | `project.rs:2177` |
| `FakeFs::new`, `insert_file`, `pause_events`, `unpause_events_and_flush`; `Fs::create_dir` (`project` re-exports `fs`) | `fs.rs:1845`, `:1951`, `:2051`, `:2055`; `:105` |
| `StubAgentConnection` (`acp_thread` `test-support`) | `connection.rs:1014` |
| `AcpThread::handle_session_update`, `mark_activity_unconfirmed`, `register_subagent` | `acp_thread.rs:4944`, `:4372`, `:6134` |
| `Buffer::end_transaction_with_source`, `reload`, `edit`; `Project::open_local_buffer` | `buffer.rs:2629`, `:1669`, `:2854`; `project.rs:3263` |
| `StubAgentConnection::defer_next_prompt_response`, `with_receipt_submissions` | `connection.rs:1128`, `:1237` |
| `AcpThread::update_session_state`, `request_permission`, `cancel_permission_request`, `is_waiting_for_confirmation` | `acp_thread.rs:4774`, `:6661`, `:6870`, `:4478` |
| `agent_ui::test_support::{init_test, open_thread_with_connection}`, `AgentPanel::test_new`, `active_agent_thread`, `ThreadMetadataStore::init_global` (`agent_ui` `test-support`) | `crates/agent_ui/src/test_support.rs:106`, `:228`, `crates/agent_ui/src/agent_panel.rs:6729`, `:4158`, `crates/agent_ui/src/thread_metadata_store.rs:554` |
| `agent::ThreadStore::init_global`, `LanguageModelRegistry::test` (`language_model` `test-support`) | `crates/agent/src/thread_store.rs:18`, `crates/language_model/src/registry.rs:136` |
| `MultiWorkspace::test_new`, `workspace`; `Workspace::add_panel` | `crates/workspace/src/multi_workspace.rs:1642`, `:1304`; `workspace.rs:2837` |
| `file_scan_exclusions`; `SettingsStore::raw_default_settings` | `crates/settings_content/src/project.rs:166`; `crates/settings/src/settings_store.rs:511` |
| `Project::expand_entry`, `RelPath::from_unix_str` | `project.rs:2864`, `rel_path.rs:122` |
| `remote_connection` (`test-support`): no test calls it, but the test build turns on `remote`'s `Mock` connection, which `remote_connection` matches only with its own `test-support` | `crates/remote_connection/src/remote_connection.rs:256` |
