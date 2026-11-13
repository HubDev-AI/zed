use std::{any::Any, sync::Arc};

use agent_ui::AgentPanel;
use city_shell::{CityShell, PowerPreference, Source};
use futures::channel::mpsc::unbounded;
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, SharedString, Task, Window, div,
    prelude::*,
};
use project::Project;
use util::ResultExt as _;
use workspace::{Item, Workspace};

use crate::{Open, zed_agents, zed_feed, zed_settings};

pub(crate) fn init(cx: &mut App) {
    city_shell::init(settings_store(cx), cx);
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &Open, window, cx| open(workspace, window, cx));
    })
    .detach();
}

fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let project = workspace.project().clone();
    let agents = (cx.entity(), workspace.panel::<AgentPanel>(cx));
    let existing = workspace.items_of_type::<CityItem>(cx).next();
    if let Some(city) = existing {
        city.update(cx, |city, cx| {
            if matches!(city.state, State::Failed(_)) {
                city.start(&project, Some(agents), window, cx);
                cx.notify();
            }
        });
        workspace.activate_item(&city, true, true, window, cx);
        return;
    }
    let city = cx.new(|cx| CityItem::new(&project, Some(agents), window, cx));
    workspace.add_item_to_active_pane(Box::new(city), None, true, window, cx);
}

/// Where the city's files and agent signals come from: adapters that send into one stream, so an
/// agent edit reaches the city before its text. Swapping the Zed feed for eof's disk watcher
/// replaces `zed_feed::source` with `host_standalone::disk_source(root)` (plus a `host_standalone`
/// dependency), whose stream the agent adapter would then need forwarding into.
fn city_source(
    project: &Entity<Project>,
    agents: Option<AgentHost>,
    cx: &mut App,
) -> Option<Source> {
    let (sender, changes) = unbounded();
    let mut source = zed_feed::source(project, sender.clone(), changes, cx)?;
    if let Some((workspace, panel)) = agents {
        let agents = zed_agents::follow(&workspace, panel, source.root.clone(), sender, cx);
        source.keep_alive = Box::new((source.keep_alive, agents));
    }
    Some(source)
}

/// The GPU the city draws on: a tab in an editor must not switch a Mac to its discrete GPU.
const GPU: PowerPreference = PowerPreference::LowPower;

/// The workspace whose agent panel the city follows, and the panel when it has one yet.
type AgentHost = (Entity<Workspace>, Option<Entity<AgentPanel>>);

/// Where the city keeps its view settings: under a `city` key in Zed's settings file. Another
/// `city_settings::SettingsStore` takes its place here.
fn settings_store(cx: &mut App) -> city_settings::Settings {
    zed_settings::from_settings_file(<dyn fs::Fs>::global(cx), paths::settings_file().clone(), cx)
}

enum State {
    Loading,
    Ready(Entity<CityShell>),
    Failed(SharedString),
}

pub struct CityItem {
    focus: FocusHandle,
    state: State,
    _open: Task<()>,
    /// The feed and agent follower, held here rather than by eof's change loop, which would keep
    /// them until a change arrived after the tab closed.
    _keep_alive: Box<dyn Any>,
}

impl CityItem {
    pub(crate) fn new(
        project: &Entity<Project>,
        agents: Option<AgentHost>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut item = Self {
            focus: cx.focus_handle(),
            state: State::Loading,
            _open: Task::ready(()),
            _keep_alive: Box::new(()),
        };
        item.start(project, agents, window, cx);
        item
    }

    /// Opens the city of the project's folder in this tab, or says why it cannot.
    fn start(
        &mut self,
        project: &Entity<Project>,
        agents: Option<AgentHost>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.state = State::Loading;
        let Some(mut source) = city_source(project, agents, cx) else {
            self.state = State::Failed("Open a local folder to see it as a city.".into());
            return;
        };
        self._keep_alive = std::mem::replace(&mut source.keep_alive, Box::new(()));
        let root = source.root.clone();
        let text_system = Arc::new(gpui_apple::AppleTextSystem::new());
        let opening = city_shell::open(source, text_system, GPU, cx);
        self._open = cx.spawn_in(window, async move |this, cx| {
            let opened = opening.await;
            this.update_in(cx, |this, window, cx| {
                match opened {
                    Ok(shell) => {
                        if this.focus.is_focused(window) {
                            shell.focus_handle(cx).focus(window, cx);
                        }
                        this.state = State::Ready(shell);
                    }
                    Err(error) => {
                        log::error!("city: {root:?}: {error:#}");
                        this.state =
                            State::Failed(format!("The city could not open: {error:#}").into());
                    }
                }
                cx.notify();
            })
            .log_err();
        });
    }

    #[cfg(test)]
    fn shell(&self) -> Option<&Entity<CityShell>> {
        match &self.state {
            State::Ready(shell) => Some(shell),
            State::Loading | State::Failed(_) => None,
        }
    }

    fn message(&self, text: SharedString) -> gpui::Div {
        div()
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child(text)
    }
}

impl EventEmitter<()> for CityItem {}

impl Focusable for CityItem {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.state {
            State::Ready(shell) => shell.focus_handle(cx),
            State::Loading | State::Failed(_) => self.focus.clone(),
        }
    }
}

impl Item for CityItem {
    type Event = ();

    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        "City".into()
    }
}

impl Render for CityItem {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        match &self.state {
            State::Ready(shell) => div().size_full().child(shell.clone()),
            State::Loading => self.message("Building the city…".into()),
            State::Failed(message) => self.message(message.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        any::Any,
        fs,
        path::Path,
        time::{Duration, Instant},
    };

    use gpui::{AppContext as _, Entity, Focusable as _, TestAppContext, VisualTestContext};
    use project::{Project, RealFs};
    use settings::SettingsStore;
    use workspace::MultiWorkspace;

    use super::{CityItem, GPU, PowerPreference, State, open};
    use crate::zed_agents::ZedAgents;

    const WATCHED_DEADLINE: Duration = Duration::from_secs(30);
    const WATCHED_POLL: Duration = Duration::from_millis(50);

    fn test_settings() -> city_settings::Settings {
        city_settings::Settings {
            store: city_settings::MemoryStore::new(None),
            updates: futures::channel::mpsc::unbounded().1,
        }
    }

    fn put(root: &Path, path: &str, text: &str) {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn wait_for(expected: usize, item: &Entity<CityItem>, cx: &mut VisualTestContext) {
        let started = Instant::now();
        loop {
            cx.executor().advance_clock(project::FS_WATCH_LATENCY);
            cx.run_until_parked();
            let count = item.read_with(cx, |item, cx| {
                item.shell().map(|shell| shell.read(cx).file_count())
            });
            if count == Some(expected) {
                return;
            }
            assert!(
                started.elapsed() < WATCHED_DEADLINE,
                "expected {expected} files, the city shows {count:?}"
            );
            std::thread::sleep(WATCHED_POLL);
        }
    }

    // `city_shell::open` takes the preference as a required argument and eof's tests cover its
    // way to the adapter request, so the tab only has to pick low power.
    #[test]
    fn the_city_tab_asks_for_the_low_power_gpu() {
        assert_eq!(GPU, PowerPreference::LowPower);
    }

    #[gpui::test]
    async fn the_city_tab_follows_files_added_and_deleted_on_disk(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            city_shell::init(test_settings(), cx);
        });
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        put(&root, "Cargo.toml", "[package]\nname = \"demo\"\n");
        put(&root, "src/main.rs", "mod parse;\nfn main() {}\n");
        put(&root, "src/parse.rs", "pub fn parse() {}\n");
        let project = Project::test(RealFs::new(None, cx.executor()), [root.as_path()], cx).await;
        let (item, cx) = cx.add_window_view(|window, cx| CityItem::new(&project, None, window, cx));

        wait_for(3, &item, cx);
        put(&root, "src/print.rs", "pub fn print() {}\n");
        wait_for(4, &item, cx);
        fs::remove_file(root.join("src/parse.rs")).unwrap();
        wait_for(3, &item, cx);
    }

    // A folder can start empty and be emptied again; the city keeps following it, and keeps the
    // keyboard focus the tab had while it was loading, so the city's keys work without a click.
    #[gpui::test]
    async fn the_city_tab_opens_on_an_empty_folder_and_follows_it(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            city_shell::init(test_settings(), cx);
        });
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let project = Project::test(RealFs::new(None, cx.executor()), [root.as_path()], cx).await;
        let (item, cx) = cx.add_window_view(|window, cx| {
            let item = CityItem::new(&project, None, window, cx);
            item.focus.focus(window, cx);
            item
        });

        wait_for(0, &item, cx);
        let shell_focused = cx.update(|window, cx| {
            let item = item.read(cx);
            item.shell()
                .is_some_and(|shell| shell.focus_handle(cx).is_focused(window))
        });
        assert!(shell_focused);
        put(&root, "notes.md", "# Notes\n");
        wait_for(1, &item, cx);
        fs::remove_file(root.join("notes.md")).unwrap();
        wait_for(0, &item, cx);
    }

    // Eof's change loop would hold the feed and the agent follower, with its heartbeat, until a
    // change arrived after the tab closed; in a quiet folder that is never, so the tab holds them.
    #[gpui::test]
    async fn closing_the_tab_ends_its_agent_follower(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        agent_ui::test_support::init_test(cx);
        cx.update(|cx| city_shell::init(test_settings(), cx));
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        put(&root, "notes.md", "# Notes\n");
        let project = Project::test(RealFs::new(None, cx.executor()), [root.as_path()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        let item = workspace.update_in(cx, |_, window, cx| {
            let agents = (cx.entity(), None);
            cx.new(|cx| CityItem::new(&project, Some(agents), window, cx))
        });
        wait_for(1, &item, cx);
        let follower = item
            .read_with(cx, |item, _| {
                item._keep_alive
                    .downcast_ref::<(Box<dyn Any>, Entity<ZedAgents>)>()
                    .map(|(_, agents)| agents.downgrade())
            })
            .expect("the tab holds its agent follower");
        drop(item);
        // Releasing the item drops its last handle to the follower, which GPUI releases on the
        // next flush of effects.
        cx.update(|_, _| {});
        cx.run_until_parked();
        assert!(follower.upgrade().is_none());
    }

    // A tab that failed, such as one opened before the window had a folder, tries again when the
    // city is opened again, rather than staying failed or stacking a second tab.
    #[gpui::test]
    async fn opening_the_city_again_retries_a_tab_that_failed(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        agent_ui::test_support::init_test(cx);
        cx.update(|cx| city_shell::init(test_settings(), cx));
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        put(&root, "notes.md", "# Notes\n");
        let project = Project::test(RealFs::new(None, cx.executor()), [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        let cities = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, cx| {
                workspace
                    .items_of_type::<CityItem>(cx)
                    .map(|city| matches!(city.read(cx).state, State::Failed(_)))
                    .collect::<Vec<_>>()
            })
        };

        workspace.update_in(cx, |workspace, window, cx| open(workspace, window, cx));
        assert_eq!(cities(cx), [true]);
        workspace.update_in(cx, |workspace, window, cx| open(workspace, window, cx));
        assert_eq!(cities(cx), [true]);
        project
            .update(cx, |project, cx| {
                project.find_or_create_worktree(&root, true, cx)
            })
            .await
            .unwrap();
        workspace.update_in(cx, |workspace, window, cx| open(workspace, window, cx));
        assert_eq!(cities(cx), [false]);
    }
}
