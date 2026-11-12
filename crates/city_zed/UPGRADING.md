# Upgrading city_zed to a new Zed tag

Every Zed API `city_zed` uses, with its place at `v1.28.1` (`e8bc16acee`). At a tag bump:

1. Bring the fork's commits, with `crates/city_zed` and `script/fork-apply`, onto the new tag. `script/fork-apply` stops when `crates/city_zed` is not there yet.
2. Run `CARGO_NET_GIT_FETCH_WITH_CLI=true script/fork-apply` (eof is private).
3. Move eof's `gpui` pin to the same tag, point `city_zed` at that eof revision and run `script/fork-apply` again to resolve the lock.
4. Find the upstream changes the compiler will name, then check each line below:
   - `git diff <old> <new> -- Cargo.toml | grep agent-client-protocol`
   - `git diff <old> <new> -- crates/acp_thread/src/acp_thread.rs | grep -A3 'pub enum'`
5. Build and test. Without Xcode's Metal toolchain the shaders compile only at run time, hence the features:
   - `cargo build --profile release-fast -p zed --features gpui_platform/runtime_shaders`
   - `cargo test -p city_zed --features gpui_apple/runtime_shaders`

The only Zed files `script/fork-apply` changes are:
- the root `Cargo.toml`;
- `crates/zed/Cargo.toml`;
- `crates/zed/src/main.rs`;
- `assets/keymaps/default-macos.json`;
- `Cargo.lock`.

Removing the city takes both the `city_zed::init` line and the `CityView` keymap block.

## Registration and the tab (`city_item.rs`)

| API | at v1.28.1 |
|---|---|
| `App::observe_new` | `crates/gpui/src/app.rs:2646` |
| `Workspace::register_action` | `crates/workspace/src/workspace.rs:8698` |
| `Workspace::items_of_type`, `activate_item`, `add_item_to_active_pane` | `workspace.rs:4405`, `:5761`, `:5055` |
| `Workspace::project`, `panel::<AgentPanel>` | `workspace.rs:2939`, `:4942` |
| `workspace::Item`, `tab_content_text` | `crates/workspace/src/item.rs:170`, `:187` |
| `Window::on_next_frame` (runs before that frame draws) | `crates/gpui/src/window.rs:2922` |
| `gpui_apple::AppleTextSystem::new` (`font-kit` feature) | `crates/gpui_apple/src/text_system.rs:84` |
| `<dyn Fs>::global`, `paths::settings_file` | `crates/fs/src/fs.rs:285`, `crates/paths/src/paths.rs:278` |

## Files from Zed (`zed_feed.rs`)

| API | at v1.28.1 |
|---|---|
| `Project::is_local`, `visible_worktrees`, `worktree_for_id`, `opened_buffers`, `buffer_store` | `crates/project/src/project.rs:1106`, `:2522`, `:2585`, `:2360`, `:6767` |
| `project::Event::WorktreeUpdatedEntries`, `UpdatedEntriesSet` | `project.rs:384`, `crates/worktree/src/worktree.rs:4068` |
| `Worktree::snapshot`; `Snapshot::id`, `abs_path`, `files`, `entry_for_path` | `worktree.rs:823`, `:2554`, `:2570`, `:2835`, `:2907` |
| `Entry { id, mtime, is_ignored }`, `is_file` | `worktree.rs:3987`, `:4196` |
| `BufferStoreEvent::BufferAdded` | `crates/project/src/buffer_store.rs:92` |
| `BufferEvent::{Edited { source }, DirtyChanged, Saved, Reloaded}`, `BufferEditSource` | `crates/language/src/buffer.rs:333`-`:341`, `:311` |
| `Buffer::file`, `is_dirty`, `saved_mtime`; `File::path`, `worktree_id` | `buffer.rs:1523`, `:2534`, `:1533`, `:373`, `:395` |
| `text::Buffer::version`, `text` | `crates/text/src/text.rs:2269`, `:2193` |
| `RelPath::as_unix_str`, `fs::MTime` | `crates/path/src/rel_path.rs:268`, `fs.rs:341` |

## Settings (`zed_settings.rs`)

| API | at v1.28.1 |
|---|---|
| `settings::watch_config_file` | `crates/settings/src/settings_file.rs:171` |
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
| `StubAgentConnection` (`acp_thread` `test-support`) | `connection.rs:1014` |
| `AcpThread::handle_session_update`, `mark_activity_unconfirmed`, `register_subagent` | `acp_thread.rs:4944`, `:4372`, `:6134` |
| `Buffer::end_transaction_with_source` | `buffer.rs:2629` |
