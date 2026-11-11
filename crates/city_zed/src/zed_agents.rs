//! Agents at work, as Zed's agent panel runs them: every live thread's turns and tool calls,
//! turned into city_host's agent signals. Agent edits of buffers are announced by the file feed,
//! which sees them first and must send them before their text.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use acp_thread::{AcpThread, AcpThreadEvent, AgentThreadEntry, ThreadStatus, ToolCallStatus};
use agent_client_protocol::schema::v2 as acp;
use agent_ui::{AgentPanel, ConversationView};
use city_host::{
    AgentSignal, Origin, ThreadId, ThreadState, ToolCall, ToolKind, ToolLocation, ToolStatus,
};
use city_shell::HostChange;
use futures::channel::mpsc::UnboundedSender;
use gpui::{AppContext as _, Context, Entity, EntityId, Subscription};
use util::ResultExt as _;
use workspace::Workspace;

/// The agent signal for an edit an agent made to the file at `path`.
pub(crate) fn edited(path: String) -> HostChange {
    HostChange::Agent {
        signal: AgentSignal::Edited { path },
        origin: Origin::Host,
    }
}

/// Follows the agent threads of `workspace`'s agent panel, now and as it gains one, sending
/// their signals with paths relative to `root`.
pub(crate) fn follow(
    workspace: &Entity<Workspace>,
    panel: Option<Entity<AgentPanel>>,
    root: PathBuf,
    sender: UnboundedSender<HostChange>,
    cx: &mut gpui::App,
) -> Entity<ZedAgents> {
    cx.new(|cx| {
        let mut agents = ZedAgents::new(root, sender);
        agents._workspace = Some(cx.subscribe(
            workspace,
            |this: &mut ZedAgents, _, event: &workspace::Event, cx| {
                if let workspace::Event::PanelAdded(view) = event
                    && let Ok(panel) = view.clone().downcast::<AgentPanel>()
                {
                    this.attach(panel, cx);
                }
            },
        ));
        if let Some(panel) = panel {
            agents.attach(panel, cx);
        }
        agents
    })
}

pub(crate) struct ZedAgents {
    root: PathBuf,
    sender: UnboundedSender<HostChange>,
    panel: Option<(Entity<AgentPanel>, Subscription)>,
    views: HashMap<EntityId, Subscription>,
    threads: HashMap<EntityId, Followed>,
    _workspace: Option<Subscription>,
}

/// One thread, and what the city has last been told of it.
struct Followed {
    id: ThreadId,
    state: Option<ThreadState>,
    calls: HashMap<String, ToolCall>,
    _subscriptions: [Subscription; 2],
}

impl ZedAgents {
    fn new(root: PathBuf, sender: UnboundedSender<HostChange>) -> Self {
        Self {
            root,
            sender,
            panel: None,
            views: HashMap::default(),
            threads: HashMap::default(),
            _workspace: None,
        }
    }

    fn attach(&mut self, panel: Entity<AgentPanel>, cx: &mut Context<Self>) {
        let observed = cx.observe(&panel, |this, _, cx| this.rescan(cx));
        self.panel = Some((panel, observed));
        self.rescan(cx);
    }

    /// Follows every conversation the panel holds, active or in the background. A conversation's
    /// root thread can appear after the conversation, so each one is watched until it has one.
    fn rescan(&mut self, cx: &mut Context<Self>) {
        let Some((panel, _)) = &self.panel else {
            return;
        };
        let views = panel.read(cx).conversation_views();
        for view in views {
            self.views
                .entry(view.entity_id())
                .or_insert_with(|| cx.observe(&view, |this, _, cx| this.rescan(cx)));
            if let Some(thread) = root_thread(&view, cx) {
                self.track(thread, cx);
            }
        }
    }

    /// Follows `thread` from now on. A thread already in a turn is reported as working, with the
    /// tool calls still open.
    pub(crate) fn track(&mut self, thread: Entity<AcpThread>, cx: &mut Context<Self>) {
        let key = thread.entity_id();
        if self.threads.contains_key(&key) {
            return;
        }
        let id = ThreadId(thread.read(cx).session_id().to_string());
        let events = cx.subscribe(&thread, move |this, thread, event, cx| {
            this.thread_event(key, &thread, event, cx)
        });
        let released = cx.observe_release(&thread, move |this, _, _| this.released(key));
        self.threads.insert(
            key,
            Followed {
                id,
                state: None,
                calls: HashMap::default(),
                _subscriptions: [events, released],
            },
        );
        if thread.read(cx).status() == ThreadStatus::Generating {
            self.set_state(key, ThreadState::Working);
            let open: Vec<usize> = (0..thread.read(cx).entries().len()).collect();
            for index in open {
                self.entry_changed(key, &thread, index, true, cx);
            }
        }
    }

    fn thread_event(
        &mut self,
        key: EntityId,
        thread: &Entity<AcpThread>,
        event: &AcpThreadEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            AcpThreadEvent::StatusChanged => match thread.read(cx).status() {
                ThreadStatus::Generating => self.set_state(key, ThreadState::Working),
                ThreadStatus::Idle => self.set_state(key, ThreadState::Idle),
                ThreadStatus::Unconfirmed => {}
            },
            AcpThreadEvent::NewEntry => {
                if let Some(index) = thread.read(cx).entries().len().checked_sub(1) {
                    self.entry_changed(key, thread, index, false, cx);
                }
            }
            AcpThreadEvent::EntryUpdated(index) => {
                self.entry_changed(key, thread, *index, false, cx)
            }
            AcpThreadEvent::ToolAuthorizationRequested(_) => {
                self.set_state(key, ThreadState::WaitingForPermission);
                self.calls_changed(key, thread, cx);
            }
            AcpThreadEvent::ToolAuthorizationReceived(_) => {
                self.set_state(key, ThreadState::Working);
                self.calls_changed(key, thread, cx);
            }
            AcpThreadEvent::Stopped { stop_reason, .. } => {
                self.calls_changed(key, thread, cx);
                let state = match stop_reason {
                    Some(acp::StopReason::EndTurn) | None => ThreadState::Idle,
                    Some(_) => ThreadState::Stopped,
                };
                self.set_state(key, state);
            }
            AcpThreadEvent::Error | AcpThreadEvent::ConnectionLost | AcpThreadEvent::Refusal => {
                self.set_state(key, ThreadState::Error)
            }
            _ => {}
        }
    }

    /// Sends every tool call of the thread whose state the city has not seen yet.
    fn calls_changed(&mut self, key: EntityId, thread: &Entity<AcpThread>, cx: &Context<Self>) {
        for index in 0..thread.read(cx).entries().len() {
            self.entry_changed(key, thread, index, false, cx);
        }
    }

    fn entry_changed(
        &mut self,
        key: EntityId,
        thread: &Entity<AcpThread>,
        index: usize,
        open_only: bool,
        cx: &Context<Self>,
    ) {
        let Some(AgentThreadEntry::ToolCall(call)) = thread.read(cx).entries().get(index) else {
            return;
        };
        let call = ToolCall {
            id: call.id.to_string(),
            kind: tool_kind(call.kind()),
            status: tool_status(call.status()),
            locations: call
                .locations
                .iter()
                .filter_map(|location| {
                    Some(ToolLocation {
                        path: relative(&self.root, &location.path)?,
                        line: location.line,
                    })
                })
                .collect(),
        };
        if open_only && !call.status.open() {
            return;
        }
        let Some(followed) = self.threads.get(&key) else {
            return;
        };
        let in_turn = followed.state.is_some_and(ThreadState::in_turn);
        // Calls seen again after their turn ended, such as the replay when a turn stops, are old
        // news; only a thread that is generating opens a turn for a call.
        let generating = thread.read(cx).status() == ThreadStatus::Generating;
        if followed.calls.get(&call.id) == Some(&call) || (!in_turn && !generating) {
            return;
        }
        if !in_turn {
            self.set_state(key, ThreadState::Working);
        }
        let Some(followed) = self.threads.get_mut(&key) else {
            return;
        };
        followed.calls.insert(call.id.clone(), call.clone());
        let signal = AgentSignal::ToolCall {
            thread: followed.id.clone(),
            call,
        };
        self.send(signal);
    }

    fn set_state(&mut self, key: EntityId, state: ThreadState) {
        let Some(followed) = self.threads.get_mut(&key) else {
            return;
        };
        if followed.state == Some(state) {
            return;
        }
        followed.state = Some(state);
        if !state.in_turn() {
            followed.calls.clear();
        }
        let signal = AgentSignal::Thread {
            thread: followed.id.clone(),
            state,
        };
        self.send(signal);
    }

    /// A thread that goes away mid-turn ends its turn.
    fn released(&mut self, key: EntityId) {
        if self
            .threads
            .get(&key)
            .is_some_and(|followed| followed.state.is_some_and(ThreadState::in_turn))
        {
            self.set_state(key, ThreadState::Stopped);
        }
        self.threads.remove(&key);
    }

    fn send(&self, signal: AgentSignal) {
        self.sender
            .unbounded_send(HostChange::Agent {
                signal,
                origin: Origin::Host,
            })
            .log_err();
    }
}

fn root_thread(view: &Entity<ConversationView>, cx: &gpui::App) -> Option<Entity<AcpThread>> {
    let thread_view = view.read(cx).root_thread_view()?;
    Some(thread_view.read(cx).thread.clone())
}

/// `path` relative to `root`, `/`-separated, or `None` when it lies outside.
fn relative(root: &Path, path: &Path) -> Option<String> {
    let inside = path.strip_prefix(root).ok()?;
    let parts: Vec<String> = inside
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect();
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn tool_kind(kind: &acp::ToolKind) -> ToolKind {
    match kind {
        acp::ToolKind::Read => ToolKind::Read,
        acp::ToolKind::Edit => ToolKind::Edit,
        acp::ToolKind::Delete => ToolKind::Delete,
        acp::ToolKind::Move => ToolKind::Move,
        acp::ToolKind::Search => ToolKind::Search,
        acp::ToolKind::Execute => ToolKind::Execute,
        _ => ToolKind::Other,
    }
}

fn tool_status(status: ToolCallStatus) -> ToolStatus {
    match status {
        ToolCallStatus::Pending => ToolStatus::Pending,
        ToolCallStatus::WaitingForConfirmation => ToolStatus::WaitingForPermission,
        ToolCallStatus::InProgress => ToolStatus::InProgress,
        ToolCallStatus::Completed => ToolStatus::Completed,
        ToolCallStatus::Failed => ToolStatus::Failed,
        ToolCallStatus::Rejected => ToolStatus::Rejected,
        ToolCallStatus::Canceled => ToolStatus::Canceled,
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, rc::Rc};

    use acp_thread::{AcpThread, AgentConnection as _, StubAgentConnection};
    use agent_client_protocol::schema::v1 as acp_v1;
    use city_host::{
        AgentSignal, ThreadId, ThreadState, ToolCall, ToolKind, ToolLocation, ToolStatus,
    };
    use city_shell::{HostChange, Source};
    use futures::channel::mpsc::{UnboundedReceiver, unbounded};
    use gpui::{AppContext as _, Entity, TestAppContext};
    use language::BufferEditSource;
    use project::{Project, RealFs};
    use settings::SettingsStore;
    use util::path_list::PathList;

    use super::ZedAgents;
    use crate::zed_feed;

    fn init(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
        });
    }

    async fn session(
        connection: &Rc<StubAgentConnection>,
        project: &Entity<Project>,
        root: &Path,
        cx: &mut TestAppContext,
    ) -> Entity<AcpThread> {
        cx.update(|cx| {
            connection
                .clone()
                .new_session(project.clone(), PathList::new(&[root]), cx)
        })
        .await
        .unwrap()
    }

    fn update(thread: &Entity<AcpThread>, update: acp_v1::SessionUpdate, cx: &mut TestAppContext) {
        thread
            .update(cx, |thread, cx| thread.handle_session_update(update, cx))
            .unwrap();
        cx.run_until_parked();
    }

    fn call_started(id: &str, path: &Path, line: u32) -> acp_v1::SessionUpdate {
        acp_v1::SessionUpdate::ToolCall(
            acp_v1::ToolCall::new(id.to_string(), "Edit")
                .kind(acp_v1::ToolKind::Edit)
                .status(acp_v1::ToolCallStatus::Pending)
                .locations(vec![acp_v1::ToolCallLocation::new(path).line(line)]),
        )
    }

    fn call_status(id: &str, status: acp_v1::ToolCallStatus) -> acp_v1::SessionUpdate {
        acp_v1::SessionUpdate::ToolCallUpdate(acp_v1::ToolCallUpdate::new(
            id.to_string(),
            acp_v1::ToolCallUpdateFields::new().status(status),
        ))
    }

    fn agent_signals(changes: &mut UnboundedReceiver<HostChange>) -> Vec<AgentSignal> {
        let mut signals = Vec::new();
        while let Ok(change) = changes.try_recv() {
            if let HostChange::Agent { signal, .. } = change {
                signals.push(signal);
            }
        }
        signals
    }

    fn thread_id(thread: &Entity<AcpThread>, cx: &mut TestAppContext) -> ThreadId {
        thread.read_with(cx, |thread, _| ThreadId(thread.session_id().to_string()))
    }

    fn edit_call(status: ToolStatus) -> ToolCall {
        ToolCall {
            id: "edit-1".into(),
            kind: ToolKind::Edit,
            status,
            locations: vec![ToolLocation {
                path: "src/main.rs".into(),
                line: Some(2),
            }],
        }
    }

    // One turn of a real agent thread, in the order the city needs: the thread is working before
    // its call, the call carries its file and line through each status, the agent's edit is named
    // before its text reaches the city, and the turn ends idle.
    #[gpui::test]
    async fn a_thread_turn_reaches_the_city_in_order(cx: &mut TestAppContext) {
        init(cx);
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {\n}\n").unwrap();
        let project = Project::test(RealFs::new(None, cx.executor()), [root.as_path()], cx).await;
        let (sender, changes) = unbounded();
        let Source {
            mut changes,
            keep_alive,
            ..
        } = cx
            .update(|cx| zed_feed::source(&project, sender.clone(), changes, cx))
            .unwrap();
        let agents = cx.new(|_| ZedAgents::new(root.clone(), sender));
        let connection = Rc::new(StubAgentConnection::new());
        let thread = session(&connection, &project, &root, cx).await;
        agents.update(cx, |agents, cx| agents.track(thread.clone(), cx));
        let id = thread_id(&thread, cx);
        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(root.join("src/main.rs"), cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();
        agent_signals(&mut changes);

        let respond = connection.defer_next_prompt_response();
        let turn = thread.update(cx, |thread, cx| thread.send(vec!["change main".into()], cx));
        cx.run_until_parked();
        update(
            &thread,
            call_started("edit-1", &root.join("src/main.rs"), 2),
            cx,
        );
        update(
            &thread,
            call_status("edit-1", acp_v1::ToolCallStatus::InProgress),
            cx,
        );
        buffer.update(cx, |buffer, cx| {
            buffer.start_transaction();
            buffer.edit([(0..0, "// agent\n")], None, cx);
            buffer.end_transaction_with_source(BufferEditSource::Agent, cx);
        });
        cx.run_until_parked();
        let mut sent = Vec::new();
        while let Ok(change) = changes.try_recv() {
            sent.push(change);
        }
        let edited = sent.iter().position(|change| {
            matches!(change, HostChange::Agent { signal: AgentSignal::Edited { path }, .. } if path == "src/main.rs")
        });
        let text = sent.iter().position(|change| {
            matches!(change, HostChange::Text { path, text: Some(_) } if path == "src/main.rs")
        });
        assert!(edited.is_some() && edited < text, "{sent:?}");
        update(
            &thread,
            call_status("edit-1", acp_v1::ToolCallStatus::Completed),
            cx,
        );
        respond
            .send(Ok(acp_v1::PromptResponse::new(acp_v1::StopReason::EndTurn)))
            .unwrap();
        turn.await.unwrap();
        cx.run_until_parked();

        let mut signals: Vec<AgentSignal> = sent
            .into_iter()
            .filter_map(|change| match change {
                HostChange::Agent { signal, .. } => Some(signal),
                _ => None,
            })
            .collect();
        signals.extend(agent_signals(&mut changes));
        let call = |status| AgentSignal::ToolCall {
            thread: id.clone(),
            call: edit_call(status),
        };
        assert_eq!(
            signals,
            [
                AgentSignal::Thread {
                    thread: id.clone(),
                    state: ThreadState::Working,
                },
                call(ToolStatus::Pending),
                call(ToolStatus::InProgress),
                AgentSignal::Edited {
                    path: "src/main.rs".into(),
                },
                call(ToolStatus::Completed),
                AgentSignal::Thread {
                    thread: id.clone(),
                    state: ThreadState::Idle,
                },
            ]
        );
        drop(keep_alive);
    }

    // Two threads working at once stay two threads: each keeps its own id, state and calls.
    #[gpui::test]
    async fn two_threads_at_once_stay_apart(cx: &mut TestAppContext) {
        init(cx);
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn lib() {}\n").unwrap();
        let project = Project::test(RealFs::new(None, cx.executor()), [root.as_path()], cx).await;
        let (sender, mut changes) = unbounded();
        let agents = cx.new(|_| ZedAgents::new(root.clone(), sender));
        let connection = Rc::new(StubAgentConnection::new());
        let first = session(&connection, &project, &root, cx).await;
        let second = session(&connection, &project, &root, cx).await;
        for thread in [&first, &second] {
            agents.update(cx, |agents, cx| agents.track(thread.clone(), cx));
        }
        let (first_id, second_id) = (thread_id(&first, cx), thread_id(&second, cx));
        assert_ne!(first_id, second_id);

        let first_respond = connection.defer_next_prompt_response();
        let first_turn = first.update(cx, |thread, cx| thread.send(vec!["one".into()], cx));
        cx.run_until_parked();
        let second_respond = connection.defer_next_prompt_response();
        let second_turn = second.update(cx, |thread, cx| thread.send(vec!["two".into()], cx));
        cx.run_until_parked();
        update(&first, call_started("a", &root.join("src/main.rs"), 1), cx);
        update(&second, call_started("b", &root.join("src/lib.rs"), 1), cx);
        first_respond
            .send(Ok(acp_v1::PromptResponse::new(acp_v1::StopReason::EndTurn)))
            .unwrap();
        first_turn.await.unwrap();
        cx.run_until_parked();

        let signals = agent_signals(&mut changes);
        let of = |id: &ThreadId| -> Vec<String> {
            signals
                .iter()
                .filter_map(|signal| match signal {
                    AgentSignal::Thread { thread, state } if thread == id => {
                        Some(format!("{state:?}"))
                    }
                    AgentSignal::ToolCall { thread, call } if thread == id => Some(format!(
                        "{} {:?} {}",
                        call.id, call.status, call.locations[0].path
                    )),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(of(&first_id), ["Working", "a Pending src/main.rs", "Idle"]);
        assert_eq!(of(&second_id), ["Working", "b Pending src/lib.rs"]);

        second_respond
            .send(Ok(acp_v1::PromptResponse::new(
                acp_v1::StopReason::Cancelled,
            )))
            .unwrap();
        second_turn.await.unwrap();
        cx.run_until_parked();
        let ended = agent_signals(&mut changes);
        assert_eq!(
            ended.last(),
            Some(&AgentSignal::Thread {
                thread: second_id,
                state: ThreadState::Stopped,
            }),
            "{ended:?}"
        );
    }
}
