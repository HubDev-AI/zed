//! The city's view settings kept under a `city` key in Zed's own settings file: read through Zed's
//! config-file watcher, first as the city starts and then on each edit while Zed runs, and written
//! back in place with Zed's JSON editor (comments and other keys stay as they are).

use std::{
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use city_settings::{Saved, Settings, SettingsStore};
use fs::Fs;
use futures::{
    FutureExt as _, StreamExt as _, channel::mpsc::unbounded, future::BoxFuture, lock::Mutex,
};
use gpui::App;

/// The key the city's settings sit under in Zed's settings file. Zed reads past keys it does not
/// know and keeps them when it writes the file itself.
const KEY: &str = "city";

/// The city's settings in the Zed settings file at `path`, with edits made to that file while Zed
/// runs sent to the city as they land.
pub(crate) fn from_settings_file(fs: Arc<dyn Fs>, path: PathBuf, cx: &mut App) -> Settings {
    let store = Rc::new(ZedSettingsStore {
        fs: fs.clone(),
        path: path.clone(),
        last: Last::default(),
        loaded: Arc::default(),
    });
    let (sender, updates) = unbounded();
    let (mut changes, watching) =
        settings::watch_config_file(cx.background_executor(), fs.clone(), path.clone());
    let (last, loaded) = (store.last.clone(), store.loaded.clone());
    cx.spawn(async move |_| {
        let _watching = watching;
        // The watcher's text was read before this lock was taken, maybe while a save was writing,
        // so only a read under the lock is compared with `last`.
        while changes.next().await.is_some() {
            let mut last = last.lock().await;
            let text = read(fs.as_ref(), &path).await;
            loaded.store(true, Ordering::Release);
            let saved = match text
                .and_then(|text| parse(&text).map_err(|e| format!("{}: {e}", path.display())))
            {
                Ok(Some(saved)) => saved,
                Ok(None) => continue,
                Err(reason) => {
                    log::error!("{reason}");
                    continue;
                }
            };
            if last.as_ref() != Some(&saved) {
                *last = Some(saved.clone());
                if sender.unbounded_send(saved).is_err() {
                    break;
                }
            }
        }
    })
    .detach();
    Settings { store, updates }
}

/// What the file last held as far as the city knows, whether the city loaded it, saved it or was
/// sent it, so a write of the same settings, such as the city's own save coming back through the
/// watcher, is not sent again over what the city changed since.
type Last = Arc<Mutex<Option<Saved>>>;

struct ZedSettingsStore {
    fs: Arc<dyn Fs>,
    path: PathBuf,
    last: Last,
    /// Whether the watcher has read the file yet. Until it has, the city holds its defaults.
    loaded: Arc<AtomicBool>,
}

impl SettingsStore for ZedSettingsStore {
    fn load(&self) -> Result<Option<Saved>, String> {
        // The port's load runs on the main thread as the city starts, so it does not touch the
        // disk: the watcher's first read of the file reaches the city through `updates`.
        Ok(self.last.try_lock().and_then(|last| last.clone()))
    }

    fn save(&self, saved: &Saved) -> BoxFuture<'static, Result<String, String>> {
        let (fs, path, last, saved) = (
            self.fs.clone(),
            self.path.clone(),
            self.last.clone(),
            saved.clone(),
        );
        let loaded = self.loaded.load(Ordering::Acquire);
        async move {
            // The watcher takes this lock before it reads the file, so it never compares the file
            // from before this write with `last` from after it and sends the old settings back.
            let mut last = last.lock().await;
            // A settings file kept elsewhere through a link stays linked.
            let path = fs.canonicalize(&path).await.unwrap_or(path);
            let text = read(fs.as_ref(), &path).await?;
            if !loaded && matches!(parse(&text), Ok(Some(_))) {
                return Err(format!(
                    "{}: the city's settings are still loading",
                    path.display()
                ));
            }
            let text = with_city(text, &saved).map_err(|e| format!("{}: {e}", path.display()))?;
            fs.atomic_write(path.clone(), text)
                .await
                .map_err(|e| format!("{}: {e:#}", path.display()))?;
            *last = Some(saved);
            Ok(format!("\"{KEY}\" in {}", path.display()))
        }
        .boxed()
    }
}

/// The settings file's text, empty when there is no file yet.
async fn read(fs: &dyn Fs, path: &Path) -> Result<String, String> {
    match fs.metadata(path).await {
        Ok(None) => Ok(String::new()),
        Ok(Some(_)) => fs.load(path).await,
        Err(e) => Err(e),
    }
    .map_err(|e| format!("{}: {e:#}", path.display()))
}

/// The city's settings in a settings file's text, `None` when it has none.
fn parse(text: &str) -> Result<Option<Saved>, String> {
    if text.trim().is_empty() {
        return Ok(None);
    }
    let mut root: serde_json::Value =
        settings_json::parse_json_with_comments(text).map_err(|e| e.to_string())?;
    match root.get_mut(KEY).map(serde_json::Value::take) {
        None => Ok(None),
        Some(value) => serde_json::from_value(value)
            .map(Some)
            .map_err(|e| format!("\"{KEY}\": {e}")),
    }
}

/// `text` with the city's settings set to `saved`, and everything else as it was.
fn with_city(text: String, saved: &Saved) -> Result<String, String> {
    let mut text = if text.trim().is_empty() {
        "{\n}\n".to_owned()
    } else {
        text
    };
    let value = serde_json::to_value(saved).map_err(|e| e.to_string())?;
    let tab_size = settings_json::infer_json_indent_size(&text);
    let (range, replacement) =
        settings_json::replace_value_in_json_text(&text, &[KEY], tab_size, Some(&value), None);
    text.replace_range(range, &replacement);
    Ok(text)
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc};

    use city_settings::{Placement, Saved, Settings, Tuning, clamped};
    use gpui::{TestAppContext, UpdateGlobal as _};
    use hud_view_settings::Saving;
    use project::{FakeFs, Fs as _};
    use settings::SettingsStore;

    use super::{KEY, from_settings_file, parse};

    const PATH: &str = "/zed/settings.json";

    const USER_SETTINGS: &str = r#"// Zed settings
{
  // a comment that must survive
  "buffer_font_size": 15,
  "theme": "One Dark"
}
"#;

    fn tuned(pitch: f32) -> Saved {
        let mut saved = Saved::default();
        saved.tuning.pitch = pitch;
        saved
    }

    async fn put_city(fs: &FakeFs, text: &str, saved: &Saved) {
        let text = super::with_city(text.to_owned(), saved).unwrap();
        fs.insert_file(PATH, text.into_bytes()).await;
    }

    async fn city_with(saved: &Saved, cx: &mut TestAppContext) -> (Arc<FakeFs>, Settings) {
        let fs = FakeFs::new(cx.executor());
        fs.create_dir(Path::new("/zed")).await.unwrap();
        put_city(&fs, USER_SETTINGS, saved).await;
        let mut city = cx.update(|cx| from_settings_file(fs.clone(), PATH.into(), cx));
        cx.run_until_parked();
        assert_eq!(city.updates.try_recv().ok(), Some(saved.clone()));
        (fs, city)
    }

    async fn file_with_a_panel_place(cx: &mut TestAppContext) -> (Arc<FakeFs>, Saved) {
        let fs = FakeFs::new(cx.executor());
        fs.create_dir(Path::new("/zed")).await.unwrap();
        let mut saved = tuned(40.);
        saved.panels.insert("map".into(), Placement::in_column(2));
        put_city(&fs, USER_SETTINGS, &saved).await;
        (fs, saved)
    }

    // Zed's start does not wait on its settings file: the city starts on defaults, and the file's
    // settings, panel places included, take effect when the read lands.
    #[gpui::test]
    async fn the_city_starts_before_zeds_settings_file_is_read(cx: &mut TestAppContext) {
        let (fs, saved) = file_with_a_panel_place(cx).await;

        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            let city = from_settings_file(fs.clone(), PATH.into(), cx);
            assert_eq!(city.store.load(), Ok(None));
            city_shell::init(city, cx);
            assert_eq!(*cx.global::<Tuning>(), clamped(Tuning::default()));
        });
        cx.run_until_parked();
        cx.read(|cx| assert_eq!(*cx.global::<Tuning>(), clamped(saved.tuning)));

        let saving = cx.read(|cx| cx.global::<Saving>().0.clone());
        cx.update(|cx| saving(Placement::in_column(1), cx))
            .await
            .unwrap();
        let text = fs.load(Path::new(PATH)).await.unwrap();
        let panels = parse(&text).unwrap().unwrap().panels;
        assert_eq!(panels.get("map"), Some(&Placement::in_column(2)), "{text}");
    }

    // A save asked for before the first read lands would write the city's defaults over the
    // settings the file holds, so it is refused and the file's settings still take effect.
    #[gpui::test(iterations = 20)]
    async fn a_save_before_the_first_read_keeps_the_files_settings(cx: &mut TestAppContext) {
        let (fs, saved) = file_with_a_panel_place(cx).await;
        let saving = cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            city_shell::init(from_settings_file(fs.clone(), PATH.into(), cx), cx);
            let saving = cx.global::<Saving>().0.clone();
            saving(Placement::in_column(1), cx)
        });
        let error = saving.await.unwrap_err();
        assert!(error.contains("still loading"), "{error}");
        cx.run_until_parked();
        let text = fs.load(Path::new(PATH)).await.unwrap();
        assert_eq!(parse(&text), Ok(Some(saved.clone())), "{text}");
        cx.read(|cx| assert_eq!(*cx.global::<Tuning>(), clamped(saved.tuning)));
    }

    // Edits reach the city once they hold its settings; a file without them is simply new, and
    // settings that cannot be read are left out.
    #[gpui::test]
    async fn the_city_follows_its_settings_in_zeds_settings_file(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.create_dir(Path::new("/zed")).await.unwrap();
        let mut city = cx.update(|cx| from_settings_file(fs.clone(), PATH.into(), cx));
        cx.run_until_parked();
        assert!(city.updates.try_recv().is_err());

        fs.insert_file(PATH, USER_SETTINGS.into()).await;
        let mut city = cx.update(|cx| from_settings_file(fs.clone(), PATH.into(), cx));
        cx.run_until_parked();
        assert!(city.updates.try_recv().is_err());

        put_city(&fs, USER_SETTINGS, &tuned(40.)).await;
        cx.run_until_parked();
        assert_eq!(city.updates.try_recv().ok(), Some(tuned(40.)));
        assert_eq!(city.store.load(), Ok(Some(tuned(40.))));

        let unreadable = "{ \"city\": { \"tuning\": { \"pitch\": \"steep\" } } }";
        fs.insert_file(PATH, unreadable.into()).await;
        cx.run_until_parked();
        assert!(city.updates.try_recv().is_err());
        let error = parse(unreadable).unwrap_err();
        assert!(error.contains("\"city\""), "{error}");
    }

    // Saving changes only the city's key: Zed's own settings, comments and all, read as before.
    #[gpui::test]
    async fn the_city_saves_into_zeds_settings_file_and_leaves_the_rest(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.create_dir(Path::new("/zed")).await.unwrap();
        fs.insert_file(PATH, USER_SETTINGS.into()).await;
        let store = cx
            .update(|cx| from_settings_file(fs.clone(), PATH.into(), cx))
            .store;

        let saved_to = store.save(&tuned(50.)).await.unwrap();
        assert!(saved_to.contains("settings.json"), "{saved_to}");
        let text = fs.load(Path::new(PATH)).await.unwrap();
        assert!(text.contains("// a comment that must survive"), "{text}");
        assert!(text.contains(&format!("\"{KEY}\"")), "{text}");
        assert_eq!(store.load(), Ok(Some(tuned(50.))));

        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            SettingsStore::update_global(cx, |store, cx| store.set_user_settings(&text, cx))
                .result()
                .unwrap();
            let user = cx.global::<SettingsStore>().raw_user_settings().unwrap();
            assert_eq!(
                user.content.theme.buffer_font_size.map(|size| size.0),
                Some(15.)
            );
        });
    }

    // An edit of the settings file while Zed runs reaches the city; the city's own save does not
    // come back to it.
    #[gpui::test]
    async fn an_outside_edit_of_zeds_settings_file_reaches_the_city(cx: &mut TestAppContext) {
        let (fs, mut city) = city_with(&tuned(40.), cx).await;

        city.store.save(&tuned(45.)).await.unwrap();
        cx.run_until_parked();
        assert!(
            city.updates.try_recv().is_err(),
            "the city's own save came back"
        );

        let edited = fs.load(Path::new(PATH)).await.unwrap();
        put_city(&fs, &edited, &tuned(70.)).await;
        cx.run_until_parked();
        assert_eq!(city.updates.try_recv().ok(), Some(tuned(70.)));
    }

    // A save queued behind another can write while the watcher still handles the first one's
    // write; that first write must not come back as an outside edit over the second.
    #[gpui::test(iterations = 100)]
    async fn a_save_queued_behind_another_does_not_send_the_first_back(cx: &mut TestAppContext) {
        let (fs, mut city) = city_with(&tuned(40.), cx).await;

        fs.pause_events();
        city.store.save(&tuned(45.)).await.unwrap();
        let second = cx.executor().spawn(city.store.save(&tuned(50.)));
        fs.unpause_events_and_flush();
        cx.run_until_parked();
        second.await.unwrap();
        cx.run_until_parked();
        assert!(city.updates.try_recv().is_err(), "the first save came back");

        let edited = fs.load(Path::new(PATH)).await.unwrap();
        put_city(&fs, &edited, &tuned(70.)).await;
        cx.run_until_parked();
        assert_eq!(city.updates.try_recv().ok(), Some(tuned(70.)));
        assert!(city.updates.try_recv().is_err());
    }
}
