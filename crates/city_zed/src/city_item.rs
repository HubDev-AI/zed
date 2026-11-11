use std::{sync::Arc, time::Instant};

use city_shell::{CityShell, Source};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, SharedString, Task, Window, div,
    prelude::*,
};
use project::Project;
use util::ResultExt as _;
use workspace::{Item, Workspace};

use crate::{Open, zed_feed};

pub(crate) fn init(cx: &mut App) {
    city_shell::init(cx);
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &Open, window, cx| open(workspace, window, cx));
    })
    .detach();
}

fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let existing = workspace.items_of_type::<CityItem>(cx).next();
    if let Some(city) = existing {
        workspace.activate_item(&city, true, true, window, cx);
        return;
    }
    let project = workspace.project().clone();
    let city = cx.new(|cx| CityItem::new(&project, window, cx));
    workspace.add_item_to_active_pane(Box::new(city), None, true, window, cx);
}

/// Where the city's files come from. Swapping the Zed feed for eof's disk watcher is this one
/// call, `Some(host_standalone::disk_source(root))`, plus a `host_standalone` dependency.
fn city_source(project: &Entity<Project>, cx: &mut App) -> Option<Source> {
    zed_feed::source(project, cx)
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
}

impl CityItem {
    pub(crate) fn new(
        project: &Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut item = Self {
            focus: cx.focus_handle(),
            state: State::Loading,
            _open: Task::ready(()),
        };
        let Some(source) = city_source(project, cx) else {
            item.state = State::Failed("Open a local folder to see it as a city.".into());
            return item;
        };
        let started = Instant::now();
        let root = source.root.clone();
        let text_system = Arc::new(gpui_apple::AppleTextSystem::new());
        let opening = city_shell::open(source, text_system, cx);
        item._open = cx.spawn_in(window, async move |this, cx| {
            let opened = opening.await;
            this.update_in(cx, |this, window, cx| {
                match opened {
                    Ok(shell) => {
                        log::info!(
                            "city: {root:?} ready {} ms after open",
                            started.elapsed().as_millis()
                        );
                        window.on_next_frame(move |_, _| {
                            log::info!(
                                "city: first frame {} ms after open",
                                started.elapsed().as_millis()
                            );
                        });
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
        item
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
        fs,
        path::Path,
        time::{Duration, Instant},
    };

    use gpui::{Entity, TestAppContext, VisualTestContext};
    use project::{Project, RealFs};
    use settings::SettingsStore;

    use super::CityItem;

    const WATCHED_DEADLINE: Duration = Duration::from_secs(30);
    const WATCHED_POLL: Duration = Duration::from_millis(50);
    const OPEN_DEADLINE: Duration = Duration::from_secs(120);

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

    #[gpui::test]
    async fn the_city_tab_follows_files_added_and_deleted_on_disk(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            city_shell::init(cx);
        });
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        put(&root, "Cargo.toml", "[package]\nname = \"demo\"\n");
        put(&root, "src/main.rs", "mod parse;\nfn main() {}\n");
        put(&root, "src/parse.rs", "pub fn parse() {}\n");
        let project = Project::test(RealFs::new(None, cx.executor()), [root.as_path()], cx).await;
        let (item, cx) = cx.add_window_view(|window, cx| CityItem::new(&project, window, cx));

        wait_for(3, &item, cx);
        put(&root, "src/print.rs", "pub fn print() {}\n");
        wait_for(4, &item, cx);
        fs::remove_file(root.join("src/parse.rs")).unwrap();
        wait_for(3, &item, cx);
    }

    // Off-screen open-to-ready time of the tab, on this checkout and on a small folder, for a first
    // open and a reopen. Run with `--ignored --nocapture` in a release profile.
    #[gpui::test]
    #[ignore = "measurement, not a check"]
    async fn open_to_ready_time(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            city_shell::init(cx);
        });
        let checkout = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let small = tempfile::tempdir().unwrap();
        put(small.path(), "Cargo.toml", "[package]\nname = \"demo\"\n");
        put(small.path(), "src/main.rs", "mod parse;\nfn main() {}\n");
        put(small.path(), "src/parse.rs", "pub fn parse() {}\n");
        put(small.path(), "README.md", "A demo.\n");
        for (name, root) in [
            ("small folder", small.path()),
            ("Zed source", checkout.as_path()),
        ] {
            let project = Project::test(RealFs::new(None, cx.executor()), [root], cx).await;
            // The test executor runs the project's own background work on this thread, so it is
            // finished first rather than timed as the city's.
            cx.run_until_parked();
            for attempt in ["first open", "reopen"] {
                let started = Instant::now();
                let (item, cx) =
                    cx.add_window_view(|window, cx| CityItem::new(&project, window, cx));
                let files = loop {
                    cx.run_until_parked();
                    let files = item.read_with(cx, |item, cx| {
                        item.shell().map(|shell| shell.read(cx).file_count())
                    });
                    if let Some(files) = files {
                        break files;
                    }
                    assert!(started.elapsed() < OPEN_DEADLINE, "{name} never opened");
                    std::thread::sleep(Duration::from_millis(1));
                };
                println!(
                    "{name}, {attempt}: {files} files, ready {} ms after open",
                    started.elapsed().as_millis()
                );
                cx.update(|window, _| window.remove_window());
            }
        }
    }
}
