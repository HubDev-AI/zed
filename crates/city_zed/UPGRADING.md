# Upgrading city_zed to a new Zed tag

Every Zed API `city_zed` uses, with its place at `v1.27.0` (`c84af5b99c`). At a tag bump:

1. Run `script/fork-apply` on the new tag.
2. Move eof's `gpui` pin to the same tag.
3. Check each line below.

The only Zed files `script/fork-apply` changes are:
- the root `Cargo.toml`;
- `crates/zed/Cargo.toml`;
- `crates/zed/src/main.rs`;
- `assets/keymaps/default-macos.json`;
- `Cargo.lock`.

Removing the city takes both the `city_zed::init` line and the `CityView` keymap block.

## Registration and the tab (`city_item.rs`)

| API | at v1.27.0 |
|---|---|
| `App::observe_new` | `crates/gpui/src/app.rs:2615` |
| `Workspace::register_action` | `crates/workspace/src/workspace.rs:8705` |
| `Workspace::items_of_type`, `activate_item`, `add_item_to_active_pane` | `workspace.rs:4408`, `:5766`, `:5058` |
| `Workspace::project`, `panel::<AgentPanel>` | `workspace.rs:2942`, `:4945` |
| `workspace::Item`, `tab_content_text` | `crates/workspace/src/item.rs:170`, `:187` |
| `Window::on_next_frame` (runs before that frame draws) | `crates/gpui/src/window.rs:2755` |
| `gpui_apple::AppleTextSystem::new` (`font-kit` feature) | `crates/gpui_apple/src/text_system.rs:84` |
| `<dyn Fs>::global`, `paths::settings_file` | `crates/fs/src/fs.rs:285`, `crates/paths/src/paths.rs:278` |

## Files from Zed (`zed_feed.rs`)

| API | at v1.27.0 |
|---|---|
| `Project::is_local`, `visible_worktrees`, `worktree_for_id`, `opened_buffers`, `buffer_store` | `crates/project/src/project.rs:1100`, `:2507`, `:2570`, `:2355`, `:6725` |
| `project::Event::WorktreeUpdatedEntries`, `UpdatedEntriesSet` | `project.rs:378`, `crates/worktree/src/worktree.rs:4067` |
| `Worktree::snapshot`; `Snapshot::id`, `abs_path`, `files`, `entry_for_path` | `worktree.rs:823`, `:2553`, `:2569`, `:2834`, `:2906` |
| `Entry { id, mtime, is_ignored }`, `is_file` | `worktree.rs:3986`, `:4195` |
| `BufferStoreEvent::BufferAdded` | `crates/project/src/buffer_store.rs:92` |
| `BufferEvent::{Edited { source }, DirtyChanged, Saved, Reloaded}`, `BufferEditSource` | `crates/language/src/buffer.rs:333`-`:341`, `:311` |
| `Buffer::file`, `is_dirty`, `saved_mtime`; `File::path`, `worktree_id` | `buffer.rs:1523`, `:2534`, `:1533`, `:373`, `:395` |
| `text::Buffer::version`, `text` | `crates/text/src/text.rs:2269`, `:2193` |
| `RelPath::as_unix_str`, `fs::MTime` | `crates/path/src/rel_path.rs:268`, `fs.rs:341` |

## Settings (`zed_settings.rs`)

| API | at v1.27.0 |
|---|---|
| `settings::watch_config_file` | `crates/settings/src/settings_file.rs:171` |
| `settings_json::parse_json_with_comments`, `replace_value_in_json_text` (`editing` feature), `infer_json_indent_size` | `crates/settings_json/src/settings_json.rs:782`, `:105`, `:662` |

## Agents (`zed_agents.rs`): the fast-moving part

`agent_ui` and `acp_thread` change often upstream. `agent_ui` is used only in `follow`, `rescan` and `root_thread`, and in `city_item.rs`'s `open`. The rest of the module reads `AcpThread`.

| API | at v1.27.0 |
|---|---|
| `agent_ui::AgentPanel`, `ConversationView` exports | `crates/agent_ui/src/agent_ui.rs:79`, `:88` |
| `AgentPanel::conversation_views` | `crates/agent_ui/src/agent_panel.rs:4145` |
| `ConversationView::root_thread_view`, `ThreadView.thread` | `crates/agent_ui/src/conversation_view.rs:892`, `conversation_view/thread_view.rs:762` |
| `workspace::Event::PanelAdded` | `workspace.rs:1523` |
| `AcpThreadEvent` | `crates/acp_thread/src/acp_thread.rs:3763` |
| `AcpThread::session_id`, `status`, `entries`, `title`, `connection`, `subagent`, `subagents` | `acp_thread.rs:4205`, `:4231`, `:4127`, `:4116`, `:4104`, `:5904`, `:5908` |
| `ThreadStatus`, `AgentThreadEntry`, `ToolCallStatus` | `acp_thread.rs:3845`, `:531`, `:1968` |
| `ToolCall { id, label, locations }`, `kind`, `status`; `ToolCallLocation` | `acp_thread.rs:1147`, `:1455`, `:1461`, `:1111` |
| `AgentConnection::agent_id` | `crates/acp_thread/src/connection.rs:96` |
| `Markdown::source` (the call's label) | `crates/markdown/src/markdown.rs:991` |
| `agent_client_protocol::schema::v2::{ToolKind, StopReason}` | `agent-client-protocol-schema` 1.13.0 |

## Tests only

| API | at v1.27.0 |
|---|---|
| `Project::test`, `RealFs` | `project.rs:2172` |
| `StubAgentConnection` (`acp_thread` `test-support`) | `connection.rs:975` |
| `AcpThread::handle_session_update`, `mark_activity_unconfirmed`, `register_subagent` | `acp_thread.rs:4784`, `:4256`, `:5895` |
| `Buffer::end_transaction_with_source` | `buffer.rs:2629` |
