//! Agents at work, as Zed's agent panel runs them: every live thread's turns, names and tool
//! calls, and its subagents', turned into city_host's agent signals. Who wrote a file comes with
//! the file's own change from the file feed, which sees the buffer edit.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};

use acp_thread::{
    AcpThread, AcpThreadEvent, AgentThreadEntry, ForegroundActivity, ThreadStatus, ToolCallStatus,
};
use agent_client_protocol::v2::schema as acp;
use agent_ui::{AgentPanel, ConversationView};
use city_host::{
    AgentSignal, StopReason, ThreadId, ThreadState, ToolCall, ToolKind, ToolLocation, ToolStatus,
    tuning::HEARD_WITHIN,
};
use city_shell::HostChange;
use futures::channel::mpsc::UnboundedSender;
use gpui::{AppContext as _, Context, Entity, EntityId, Subscription, Task};
use util::ResultExt as _;
use workspace::Workspace;

/// How often a live thread's state is sent again, well inside the time the city waits before it
/// stops trusting a live state it has not heard.
const HEARTBEAT: Duration = Duration::from_secs(HEARD_WITHIN.as_secs() / 2);

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
        let mut agents = ZedAgents::new(root, sender, cx);
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
    _heartbeat: Task<()>,
}

/// One thread, and what the city has last been told of it.
struct Followed {
    id: ThreadId,
    parent: Option<ThreadId>,
    state: Option<ThreadState>,
    /// Every call sent, as last sent, across all turns: a call the agent replays unchanged in a
    /// later turn is not news.
    calls: HashMap<String, ToolCall>,
    _subscriptions: [Subscription; 2],
}

impl ZedAgents {
    pub(crate) fn new(
        root: PathBuf,
        sender: UnboundedSender<HostChange>,
        cx: &mut Context<Self>,
    ) -> Self {
        let heartbeat = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(HEARTBEAT).await;
                if this.update(cx, |this, _| this.heartbeat()).is_err() {
                    break;
                }
            }
        });
        Self {
            root,
            sender,
            panel: None,
            views: HashMap::default(),
            threads: HashMap::default(),
            _workspace: None,
            _heartbeat: heartbeat,
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

    /// Follows `thread` and its subagents from now on.
    pub(crate) fn track(&mut self, thread: Entity<AcpThread>, cx: &mut Context<Self>) {
        self.track_with_parent(thread, None, cx);
    }

    /// A thread already in a turn is reported as working, with the tool calls still open.
    fn track_with_parent(
        &mut self,
        thread: Entity<AcpThread>,
        parent: Option<ThreadId>,
        cx: &mut Context<Self>,
    ) {
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
                id: id.clone(),
                parent,
                state: None,
                calls: HashMap::default(),
                _subscriptions: [events, released],
            },
        );
        if let Some(state) = current_state(thread.read(cx)) {
            self.set_state(key, &thread, state, cx);
            if state.live() {
                for index in 0..thread.read(cx).entries().len() {
                    self.entry_changed(key, &thread, index, true, cx);
                }
            }
        }
        let subagents: Vec<Entity<AcpThread>> = thread.read(cx).subagents().cloned().collect();
        for subagent in subagents {
            self.track_with_parent(subagent, Some(id.clone()), cx);
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
            AcpThreadEvent::StatusChanged => self.state_changed(key, thread, cx),
            AcpThreadEvent::NewEntry => {
                if let Some(index) = thread.read(cx).entries().len().checked_sub(1) {
                    self.entry_changed(key, thread, index, false, cx);
                }
            }
            AcpThreadEvent::EntryUpdated(index) => {
                self.entry_changed(key, thread, *index, false, cx)
            }
            AcpThreadEvent::ToolAuthorizationRequested(_)
            | AcpThreadEvent::ToolAuthorizationReceived(_) => {
                self.state_changed(key, thread, cx);
                self.calls_changed(key, thread, cx);
            }
            AcpThreadEvent::Stopped { stop_reason, .. } => {
                self.calls_changed(key, thread, cx);
                self.set_state(key, thread, ended(stop_reason.as_ref()), cx);
            }
            AcpThreadEvent::Refusal => self.set_state(
                key,
                thread,
                ThreadState::Stopped {
                    reason: StopReason::Refusal,
                },
                cx,
            ),
            AcpThreadEvent::Error | AcpThreadEvent::ConnectionLost => {
                self.set_state(key, thread, ThreadState::Error, cx)
            }
            AcpThreadEvent::TitleUpdated => self.send_name(key, thread, cx),
            AcpThreadEvent::SubagentSpawned(session) => {
                let child = thread.read(cx).subagent(session).cloned();
                let parent = self.threads.get(&key).map(|followed| followed.id.clone());
                if let Some(child) = child {
                    self.track_with_parent(child, parent, cx);
                }
            }
            _ => {}
        }
    }

    fn state_changed(&mut self, key: EntityId, thread: &Entity<AcpThread>, cx: &Context<Self>) {
        let state = match current_state(thread.read(cx)) {
            Some(state) => state,
            // Some agents report why a turn ended before Zed reports it idle; the reason stays.
            None if self
                .threads
                .get(&key)
                .and_then(|followed| followed.state)
                .is_some_and(ThreadState::ended) =>
            {
                return;
            }
            None => ThreadState::Idle,
        };
        self.set_state(key, thread, state, cx);
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
        let label = call.label.read(cx).source().trim().to_string();
        let call = ToolCall {
            id: call.id.to_string(),
            kind: tool_kind(call.kind()),
            status: tool_status(call.status()),
            title: (!label.is_empty()).then_some(label),
            locations: call
                .locations
                .iter()
                .map(|location| ToolLocation {
                    path: city_path(&self.root, &location.path),
                    line: location.line,
                })
                .collect(),
        };
        if open_only && !call.status.open() {
            return;
        }
        let Some(followed) = self.threads.get(&key) else {
            return;
        };
        let live = followed.state.is_some_and(ThreadState::live);
        // Calls seen again after their turn ended, such as the replay when a turn stops, are old
        // news; only a thread that is generating opens a turn for a call.
        let generating = thread.read(cx).status() == ThreadStatus::Generating;
        if followed.calls.get(&call.id) == Some(&call) || (!live && !generating) {
            return;
        }
        if !live {
            self.set_state(key, thread, ThreadState::Working, cx);
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

    /// Sends the thread's state when it changes, with its name just before it.
    fn set_state(
        &mut self,
        key: EntityId,
        thread: &Entity<AcpThread>,
        state: ThreadState,
        cx: &gpui::App,
    ) {
        if self
            .threads
            .get(&key)
            .is_none_or(|followed| followed.state == Some(state))
        {
            return;
        }
        self.send_name(key, thread, cx);
        let Some(followed) = self.threads.get_mut(&key) else {
            return;
        };
        followed.state = Some(state);
        let signal = AgentSignal::Thread {
            thread: followed.id.clone(),
            state,
        };
        self.send(signal);
    }

    fn send_name(&self, key: EntityId, thread: &Entity<AcpThread>, cx: &gpui::App) {
        let Some(followed) = self.threads.get(&key) else {
            return;
        };
        let thread = thread.read(cx);
        self.send(AgentSignal::Named {
            thread: followed.id.clone(),
            title: thread.title().map(|title| title.to_string()),
            agent: Some(thread.connection().agent_id().to_string()),
            parent: followed.parent.clone(),
        });
    }

    /// Confirms each live thread's state again, so the city keeps trusting it.
    fn heartbeat(&self) {
        for followed in self.threads.values() {
            if let Some(state) = followed.state.filter(|state| state.live()) {
                self.send(AgentSignal::Thread {
                    thread: followed.id.clone(),
                    state,
                });
            }
        }
    }

    fn released(&mut self, key: EntityId) {
        if let Some(followed) = self.threads.remove(&key) {
            self.send(AgentSignal::Thread {
                thread: followed.id,
                state: ThreadState::Gone,
            });
        }
    }

    fn send(&self, signal: AgentSignal) {
        self.sender
            .unbounded_send(HostChange::Agent { signal })
            .log_err();
    }
}

fn root_thread(view: &Entity<ConversationView>, cx: &gpui::App) -> Option<Entity<AcpThread>> {
    let thread_view = view.read(cx).root_thread_view()?;
    Some(thread_view.read(cx).thread.clone())
}

/// The state Zed itself shows for the thread's turn, or `None` when it has none.
fn current_state(thread: &AcpThread) -> Option<ThreadState> {
    match thread.foreground_activity() {
        ForegroundActivity::Running => Some(ThreadState::Working),
        ForegroundActivity::RequiresAction => Some(ThreadState::WaitingForPermission),
        ForegroundActivity::Unconfirmed | ForegroundActivity::Unknown => Some(ThreadState::Unknown),
        ForegroundActivity::Idle => None,
    }
}

/// The state a turn ends in for the reason the agent gave.
fn ended(reason: Option<&acp::StopReason>) -> ThreadState {
    let reason = match reason {
        None | Some(acp::StopReason::EndTurn) => return ThreadState::Idle,
        Some(acp::StopReason::Error(_)) => return ThreadState::Error,
        Some(acp::StopReason::Cancelled) => StopReason::Cancelled,
        Some(acp::StopReason::MaxTokens) => StopReason::MaxTokens,
        Some(acp::StopReason::MaxTurnRequests) => StopReason::MaxTurnRequests,
        Some(acp::StopReason::Refusal) => StopReason::Refusal,
        Some(_) => StopReason::Other,
    };
    ThreadState::Stopped { reason }
}

/// `path` as the city names it: relative to `root` and `/`-separated inside it, absolute
/// outside.
fn city_path(root: &Path, path: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(inside) if inside.components().next().is_some() => inside
            .components()
            .map(|part| part.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/"),
        _ => path.to_string_lossy().into_owned(),
    }
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
        ToolCallStatus::InProgress | ToolCallStatus::Paused => ToolStatus::InProgress,
        ToolCallStatus::Completed => ToolStatus::Completed,
        ToolCallStatus::Failed => ToolStatus::Failed,
        ToolCallStatus::Rejected => ToolStatus::Rejected,
        ToolCallStatus::Canceled => ToolStatus::Canceled,
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, rc::Rc};

    use acp_thread::{AcpThread, AgentConnection as _, StubAgentConnection, ToolCallStatus};
    use agent_client_protocol::v1::schema as acp_v1;
    use city_host::{
        AgentSignal, StopReason, ThreadId, ThreadState, ToolCall, ToolKind, ToolLocation,
        ToolStatus,
    };
    use city_shell::HostChange;
    use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
    use gpui::{AppContext as _, Entity, TestAppContext};
    use project::{Project, RealFs};
    use settings::SettingsStore;
    use util::path_list::PathList;

    use super::{HEARTBEAT, ZedAgents, tool_status};

    struct Rig {
        root: std::path::PathBuf,
        project: Entity<Project>,
        connection: Rc<StubAgentConnection>,
        agents: Entity<ZedAgents>,
        changes: UnboundedReceiver<HostChange>,
        _dir: tempfile::TempDir,
    }

    async fn rig(connection: StubAgentConnection, cx: &mut TestAppContext) -> Rig {
        cx.executor().allow_parking();
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
        });
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {\n}\n").unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn lib() {}\n").unwrap();
        let project = Project::test(RealFs::new(None, cx.executor()), [root.as_path()], cx).await;
        let (sender, changes): (UnboundedSender<HostChange>, _) = unbounded();
        let agents = cx.new(|cx| ZedAgents::new(root.clone(), sender, cx));
        Rig {
            root,
            project,
            connection: Rc::new(connection),
            agents,
            changes,
            _dir: dir,
        }
    }

    impl Rig {
        async fn thread(&self, cx: &mut TestAppContext) -> Entity<AcpThread> {
            let thread = cx
                .update(|cx| {
                    self.connection.clone().new_session(
                        self.project.clone(),
                        PathList::new(&[self.root.as_path()]),
                        cx,
                    )
                })
                .await
                .unwrap();
            self.agents
                .update(cx, |agents, cx| agents.track(thread.clone(), cx));
            thread
        }

        fn signals(&mut self, cx: &mut TestAppContext) -> Vec<AgentSignal> {
            cx.run_until_parked();
            let mut signals = Vec::new();
            while let Ok(change) = self.changes.try_recv() {
                if let HostChange::Agent { signal } = change {
                    signals.push(signal);
                }
            }
            signals
        }
    }

    fn update(thread: &Entity<AcpThread>, update: acp_v1::SessionUpdate, cx: &mut TestAppContext) {
        thread
            .update(cx, |thread, cx| thread.handle_session_update(update, cx))
            .unwrap();
        cx.run_until_parked();
    }

    fn call_started(id: &str, path: &Path, line: u32) -> acp_v1::SessionUpdate {
        acp_v1::SessionUpdate::ToolCall(
            acp_v1::ToolCall::new(id.to_string(), "Change main")
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

    fn end(stop: acp_v1::StopReason) -> anyhow::Result<acp_v1::PromptResponse> {
        Ok(acp_v1::PromptResponse::new(stop))
    }

    fn id_of(thread: &Entity<AcpThread>, cx: &mut TestAppContext) -> ThreadId {
        thread.read_with(cx, |thread, _| ThreadId(thread.session_id().to_string()))
    }

    /// The signals without names, which each state carries along.
    fn unnamed(signals: Vec<AgentSignal>) -> Vec<AgentSignal> {
        signals
            .into_iter()
            .filter(|signal| !matches!(signal, AgentSignal::Named { .. }))
            .collect()
    }

    fn state(thread: &ThreadId, state: ThreadState) -> AgentSignal {
        AgentSignal::Thread {
            thread: thread.clone(),
            state,
        }
    }

    fn edit_call(thread: &ThreadId, status: ToolStatus, path: &str) -> AgentSignal {
        AgentSignal::ToolCall {
            thread: thread.clone(),
            call: ToolCall {
                id: "edit-1".into(),
                kind: ToolKind::Edit,
                status,
                title: Some("Change main".into()),
                locations: vec![ToolLocation {
                    path: path.into(),
                    line: Some(2),
                }],
            },
        }
    }

    // One turn, in the order the city needs: named and working before its call, the call with its
    // title, file and line through each status, then idle.
    #[gpui::test]
    async fn a_turn_is_named_working_then_each_call_status_then_idle(cx: &mut TestAppContext) {
        let mut rig = rig(StubAgentConnection::new(), cx).await;
        let thread = rig.thread(cx).await;
        let id = id_of(&thread, cx);
        let respond = rig.connection.defer_next_prompt_response();
        let turn = thread.update(cx, |thread, cx| thread.send(vec!["change".into()], cx));
        cx.run_until_parked();
        let main = rig.root.join("src/main.rs");
        update(&thread, call_started("edit-1", &main, 2), cx);
        update(
            &thread,
            call_status("edit-1", acp_v1::ToolCallStatus::InProgress),
            cx,
        );
        update(
            &thread,
            call_status("edit-1", acp_v1::ToolCallStatus::Completed),
            cx,
        );
        respond.send(end(acp_v1::StopReason::EndTurn)).unwrap();
        turn.await.unwrap();
        let signals = rig.signals(cx);

        assert!(
            matches!(
                signals.first(),
                Some(AgentSignal::Named { thread, title: _, agent: Some(_), parent: None })
                    if *thread == id
            ),
            "{signals:?}"
        );
        assert_eq!(
            unnamed(signals),
            [
                state(&id, ThreadState::Working),
                edit_call(&id, ToolStatus::Pending, "src/main.rs"),
                edit_call(&id, ToolStatus::InProgress, "src/main.rs"),
                edit_call(&id, ToolStatus::Completed, "src/main.rs"),
                state(&id, ThreadState::Idle),
            ]
        );
    }

    // A live thread is confirmed again before the city would stop trusting it; an ended one is not.
    #[gpui::test]
    async fn a_live_thread_is_confirmed_on_a_heartbeat(cx: &mut TestAppContext) {
        let mut rig = rig(StubAgentConnection::new(), cx).await;
        let thread = rig.thread(cx).await;
        let id = id_of(&thread, cx);
        let respond = rig.connection.defer_next_prompt_response();
        let turn = thread.update(cx, |thread, cx| thread.send(vec!["wait".into()], cx));
        rig.signals(cx);

        cx.executor().advance_clock(HEARTBEAT);
        assert_eq!(rig.signals(cx), [state(&id, ThreadState::Working)]);

        respond.send(end(acp_v1::StopReason::EndTurn)).unwrap();
        turn.await.unwrap();
        rig.signals(cx);
        cx.executor().advance_clock(HEARTBEAT);
        assert_eq!(rig.signals(cx), []);
    }

    // A cancelled turn stops with its reason, a failed prompt is an error, and a thread that is
    // closed is gone.
    #[gpui::test]
    async fn turns_end_stopped_or_in_error_and_closed_threads_are_gone(cx: &mut TestAppContext) {
        let mut rig = rig(StubAgentConnection::new(), cx).await;
        let thread = rig.thread(cx).await;
        let id = id_of(&thread, cx);

        let respond = rig.connection.defer_next_prompt_response();
        let turn = thread.update(cx, |thread, cx| thread.send(vec!["one".into()], cx));
        cx.run_until_parked();
        respond.send(end(acp_v1::StopReason::Cancelled)).unwrap();
        turn.await.ok();
        let ended = unnamed(rig.signals(cx));
        assert_eq!(
            ended.last(),
            Some(&state(
                &id,
                ThreadState::Stopped {
                    reason: StopReason::Cancelled
                }
            )),
            "{ended:?}"
        );

        let respond = rig.connection.defer_next_prompt_response();
        let turn = thread.update(cx, |thread, cx| thread.send(vec!["two".into()], cx));
        cx.run_until_parked();
        respond
            .send(Err(anyhow::anyhow!("the agent failed")))
            .unwrap();
        turn.await.ok();
        let failed = unnamed(rig.signals(cx));
        assert!(
            failed.contains(&state(&id, ThreadState::Error)),
            "{failed:?}"
        );

        let closed = rig.thread(cx).await;
        let closed_id = id_of(&closed, cx);
        rig.signals(cx);
        drop(closed);
        // Entities are released when the app next flushes its effects.
        cx.update(|_| {});
        assert_eq!(rig.signals(cx), [state(&closed_id, ThreadState::Gone)]);
    }

    // A thread whose own work Zed has not confirmed yet is unknown, not idle.
    #[gpui::test]
    async fn an_unconfirmed_thread_is_unknown(cx: &mut TestAppContext) {
        let mut rig = rig(
            StubAgentConnection::new().with_receipt_submissions(true),
            cx,
        )
        .await;
        let thread = rig.thread(cx).await;
        let id = id_of(&thread, cx);
        rig.signals(cx);
        thread.update(cx, |thread, cx| thread.mark_activity_unconfirmed(cx));
        assert_eq!(unnamed(rig.signals(cx)), [state(&id, ThreadState::Unknown)]);
    }

    // A call on a file outside the folder keeps its absolute path, so the city can say where.
    #[gpui::test]
    async fn a_call_outside_the_folder_keeps_its_absolute_path(cx: &mut TestAppContext) {
        let mut rig = rig(StubAgentConnection::new(), cx).await;
        let thread = rig.thread(cx).await;
        let id = id_of(&thread, cx);
        let outside = tempfile::tempdir().unwrap();
        let elsewhere = outside.path().canonicalize().unwrap().join("notes.md");
        let respond = rig.connection.defer_next_prompt_response();
        let turn = thread.update(cx, |thread, cx| thread.send(vec!["look".into()], cx));
        cx.run_until_parked();
        update(&thread, call_started("edit-1", &elsewhere, 2), cx);
        let elsewhere = elsewhere.to_string_lossy().into_owned();
        assert!(
            unnamed(rig.signals(cx)).contains(&edit_call(&id, ToolStatus::Pending, &elsewhere)),
            "{elsewhere}"
        );
        respond.send(end(acp_v1::StopReason::EndTurn)).unwrap();
        turn.await.unwrap();
    }

    // A subagent is followed as its own thread, named with the thread that started it.
    #[gpui::test]
    async fn a_subagent_is_followed_under_its_parent(cx: &mut TestAppContext) {
        let mut rig = rig(StubAgentConnection::new(), cx).await;
        let parent = rig.thread(cx).await;
        let parent_id = id_of(&parent, cx);
        let child = cx
            .update(|cx| {
                rig.connection.clone().new_session(
                    rig.project.clone(),
                    PathList::new(&[rig.root.as_path()]),
                    cx,
                )
            })
            .await
            .unwrap();
        let child_id = id_of(&child, cx);
        rig.signals(cx);
        parent.update(cx, |thread, cx| thread.register_subagent(child.clone(), cx));
        let respond = rig.connection.defer_next_prompt_response();
        let turn = child.update(cx, |thread, cx| thread.send(vec!["help".into()], cx));
        let signals = rig.signals(cx);
        assert!(
            signals.contains(&AgentSignal::Named {
                thread: child_id.clone(),
                title: child.read_with(cx, |thread, _| thread.title().map(|t| t.to_string())),
                agent: signals.iter().find_map(|signal| match signal {
                    AgentSignal::Named { agent, .. } => agent.clone(),
                    _ => None,
                }),
                parent: Some(parent_id),
            }),
            "{signals:?}"
        );
        assert!(
            unnamed(signals).contains(&state(&child_id, ThreadState::Working)),
            "the subagent's turn is followed"
        );
        respond.send(end(acp_v1::StopReason::EndTurn)).unwrap();
        turn.await.unwrap();
    }

    // A later turn that shows an earlier turn's call again, unchanged, sends nothing new for it.
    #[gpui::test]
    async fn an_earlier_turns_call_is_not_sent_again(cx: &mut TestAppContext) {
        let mut rig = rig(StubAgentConnection::new(), cx).await;
        let thread = rig.thread(cx).await;
        let id = id_of(&thread, cx);
        let main = rig.root.join("src/main.rs");

        let respond = rig.connection.defer_next_prompt_response();
        let turn = thread.update(cx, |thread, cx| thread.send(vec!["one".into()], cx));
        cx.run_until_parked();
        update(&thread, call_started("edit-1", &main, 2), cx);
        update(
            &thread,
            call_status("edit-1", acp_v1::ToolCallStatus::Completed),
            cx,
        );
        respond.send(end(acp_v1::StopReason::EndTurn)).unwrap();
        turn.await.unwrap();
        rig.signals(cx);

        let respond = rig.connection.defer_next_prompt_response();
        let turn = thread.update(cx, |thread, cx| thread.send(vec!["two".into()], cx));
        cx.run_until_parked();
        update(
            &thread,
            call_status("edit-1", acp_v1::ToolCallStatus::Completed),
            cx,
        );
        respond.send(end(acp_v1::StopReason::EndTurn)).unwrap();
        turn.await.unwrap();
        assert_eq!(
            unnamed(rig.signals(cx)),
            [
                state(&id, ThreadState::Working),
                state(&id, ThreadState::Idle),
            ]
        );
    }

    // Two threads working at once stay two threads: each keeps its own id, state and calls.
    #[gpui::test]
    async fn two_threads_at_once_stay_apart(cx: &mut TestAppContext) {
        let mut rig = rig(StubAgentConnection::new(), cx).await;
        let first = rig.thread(cx).await;
        let second = rig.thread(cx).await;
        let (first_id, second_id) = (id_of(&first, cx), id_of(&second, cx));
        assert_ne!(first_id, second_id);

        let first_respond = rig.connection.defer_next_prompt_response();
        let first_turn = first.update(cx, |thread, cx| thread.send(vec!["one".into()], cx));
        cx.run_until_parked();
        let second_respond = rig.connection.defer_next_prompt_response();
        let second_turn = second.update(cx, |thread, cx| thread.send(vec!["two".into()], cx));
        cx.run_until_parked();
        update(
            &first,
            call_started("a", &rig.root.join("src/main.rs"), 1),
            cx,
        );
        update(
            &second,
            call_started("b", &rig.root.join("src/lib.rs"), 1),
            cx,
        );
        first_respond
            .send(end(acp_v1::StopReason::EndTurn))
            .unwrap();
        first_turn.await.unwrap();

        let signals = unnamed(rig.signals(cx));
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
            .send(end(acp_v1::StopReason::EndTurn))
            .unwrap();
        second_turn.await.unwrap();
    }

    #[test]
    fn a_paused_call_still_claims_its_files() {
        assert!(tool_status(ToolCallStatus::Paused).open());
    }

    fn request_permission(
        thread: &Entity<AcpThread>,
        cx: &mut TestAppContext,
    ) -> (
        acp_thread::PermissionRequestId,
        gpui::Task<super::acp::RequestPermissionOutcome>,
    ) {
        thread.update(cx, |thread, cx| {
            let request = super::acp::RequestPermissionRequest::new(
                thread.session_id().clone(),
                "Approve operation",
                vec![super::acp::PermissionOption::new(
                    "allow",
                    "Allow",
                    super::acp::PermissionOptionKind::AllowOnce,
                )],
            );
            thread.request_permission(request, cx).unwrap()
        })
    }

    fn last_state(signals: &[AgentSignal]) -> Option<ThreadState> {
        signals.iter().rev().find_map(|signal| match signal {
            AgentSignal::Thread { state, .. } => Some(*state),
            _ => None,
        })
    }

    #[gpui::test]
    async fn a_permission_request_shows_waiting_until_answered(cx: &mut TestAppContext) {
        let mut rig = rig(StubAgentConnection::new(), cx).await;
        let thread = rig.thread(cx).await;
        let respond = rig.connection.defer_next_prompt_response();
        let turn = thread.update(cx, |thread, cx| thread.send(vec!["wait".into()], cx));
        cx.run_until_parked();
        rig.signals(cx);
        let (permission, response) = request_permission(&thread, cx);
        let signals = unnamed(rig.signals(cx));
        assert!(thread.read_with(cx, |thread, _| thread.is_waiting_for_confirmation()));
        thread.update(cx, |thread, cx| {
            thread.cancel_permission_request(permission, cx)
        });
        response.await;
        let after = unnamed(rig.signals(cx));
        respond.send(end(acp_v1::StopReason::EndTurn)).unwrap();
        turn.await.unwrap();
        let ended = unnamed(rig.signals(cx));
        assert_eq!(last_state(&ended), Some(ThreadState::Idle), "{ended:?}");
        assert_eq!(last_state(&after), Some(ThreadState::Working), "{after:?}");
        assert_eq!(
            last_state(&signals),
            Some(ThreadState::WaitingForPermission),
            "{signals:?}"
        );
    }

    #[gpui::test]
    async fn answering_one_of_two_permission_requests_keeps_waiting(cx: &mut TestAppContext) {
        let mut rig = rig(StubAgentConnection::new(), cx).await;
        let thread = rig.thread(cx).await;
        let respond = rig.connection.defer_next_prompt_response();
        let turn = thread.update(cx, |thread, cx| thread.send(vec!["wait".into()], cx));
        cx.run_until_parked();
        let (first, first_response) = request_permission(&thread, cx);
        let (second, second_response) = request_permission(&thread, cx);
        rig.signals(cx);
        thread.update(cx, |thread, cx| thread.cancel_permission_request(first, cx));
        first_response.await;
        let signals = unnamed(rig.signals(cx));
        thread.update(cx, |thread, cx| {
            thread.cancel_permission_request(second, cx)
        });
        second_response.await;
        respond.send(end(acp_v1::StopReason::EndTurn)).unwrap();
        turn.await.unwrap();
        assert!(
            !signals.contains(&state(
                &thread.read_with(cx, |thread, _| ThreadId(thread.session_id().to_string())),
                ThreadState::Working
            )),
            "{signals:?}"
        );
    }

    #[gpui::test]
    async fn a_thread_followed_while_waiting_for_permission_shows_waiting(cx: &mut TestAppContext) {
        let mut rig = rig(StubAgentConnection::new(), cx).await;
        let thread = cx
            .update(|cx| {
                rig.connection.clone().new_session(
                    rig.project.clone(),
                    PathList::new(&[rig.root.as_path()]),
                    cx,
                )
            })
            .await
            .unwrap();
        let respond = rig.connection.defer_next_prompt_response();
        let turn = thread.update(cx, |thread, cx| thread.send(vec!["wait".into()], cx));
        cx.run_until_parked();
        let (permission, response) = request_permission(&thread, cx);
        cx.run_until_parked();
        rig.agents
            .update(cx, |agents, cx| agents.track(thread.clone(), cx));
        let signals = unnamed(rig.signals(cx));
        thread.update(cx, |thread, cx| {
            thread.cancel_permission_request(permission, cx)
        });
        response.await;
        respond.send(end(acp_v1::StopReason::EndTurn)).unwrap();
        turn.await.unwrap();
        assert_eq!(
            last_state(&signals),
            Some(ThreadState::WaitingForPermission),
            "{signals:?}"
        );
    }

    fn reported(
        thread: &Entity<AcpThread>,
        update: super::acp::StateUpdate,
        cx: &mut TestAppContext,
    ) {
        thread
            .update(cx, |thread, cx| thread.update_session_state(update, cx))
            .unwrap();
        cx.run_until_parked();
    }

    fn running() -> super::acp::StateUpdate {
        super::acp::StateUpdate::Running(super::acp::RunningStateUpdate::new())
    }

    fn requires_action() -> super::acp::StateUpdate {
        super::acp::StateUpdate::RequiresAction(super::acp::RequiresActionStateUpdate::new())
    }

    fn idle(reason: super::acp::StopReason) -> super::acp::StateUpdate {
        super::acp::StateUpdate::Idle(super::acp::IdleStateUpdate::new().stop_reason(reason))
    }

    #[gpui::test]
    async fn an_agent_reporting_it_needs_action_shows_waiting(cx: &mut TestAppContext) {
        let mut rig = rig(
            StubAgentConnection::new().with_receipt_submissions(true),
            cx,
        )
        .await;
        let thread = rig.thread(cx).await;
        reported(&thread, running(), cx);
        rig.signals(cx);
        reported(&thread, requires_action(), cx);
        let signals = unnamed(rig.signals(cx));
        assert_eq!(
            last_state(&signals),
            Some(ThreadState::WaitingForPermission),
            "{signals:?}"
        );
    }

    #[gpui::test]
    async fn a_thread_followed_after_it_reported_needing_action_shows_waiting(
        cx: &mut TestAppContext,
    ) {
        let mut rig = rig(
            StubAgentConnection::new().with_receipt_submissions(true),
            cx,
        )
        .await;
        let thread = cx
            .update(|cx| {
                rig.connection.clone().new_session(
                    rig.project.clone(),
                    PathList::new(&[rig.root.as_path()]),
                    cx,
                )
            })
            .await
            .unwrap();
        reported(&thread, running(), cx);
        reported(&thread, requires_action(), cx);
        rig.agents
            .update(cx, |agents, cx| agents.track(thread.clone(), cx));
        let signals = unnamed(rig.signals(cx));
        assert_eq!(
            last_state(&signals),
            Some(ThreadState::WaitingForPermission),
            "{signals:?}"
        );
    }

    #[gpui::test]
    async fn a_reported_turn_end_keeps_its_reason_until_the_next_turn(cx: &mut TestAppContext) {
        let mut rig = rig(
            StubAgentConnection::new().with_receipt_submissions(true),
            cx,
        )
        .await;
        let thread = rig.thread(cx).await;
        let mut ends = Vec::new();
        for (reason, expected) in [
            (super::acp::StopReason::Cancelled, StopReason::Cancelled),
            (super::acp::StopReason::MaxTokens, StopReason::MaxTokens),
            (super::acp::StopReason::Refusal, StopReason::Refusal),
        ] {
            reported(&thread, running(), cx);
            let started = unnamed(rig.signals(cx));
            reported(&thread, idle(reason), cx);
            let ended = unnamed(rig.signals(cx));
            ends.push((last_state(&started), last_state(&ended), expected));
        }
        reported(&thread, running(), cx);
        let started = unnamed(rig.signals(cx));
        reported(&thread, idle(super::acp::StopReason::EndTurn), cx);
        let ended = unnamed(rig.signals(cx));
        assert_eq!(last_state(&started), Some(ThreadState::Working));
        assert_eq!(last_state(&ended), Some(ThreadState::Idle));
        for (started, ended, expected) in ends {
            assert_eq!(started, Some(ThreadState::Working));
            assert_eq!(ended, Some(ThreadState::Stopped { reason: expected }));
        }
    }
}
