//! The city's view settings kept under a `city` key in Zed's own settings file: read when the city
//! starts, written back in place with Zed's JSON editor (comments and other keys stay as they
//! are), and followed through Zed's config-file watcher while Zed runs.

use std::{
    cell::RefCell,
    io,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};

use city_settings::{Saved, Settings, SettingsStore};
use fs::Fs;
use futures::{StreamExt as _, channel::mpsc::unbounded};
use gpui::App;

/// The key the city's settings sit under in Zed's settings file. Zed reads past keys it does not
/// know and keeps them when it writes the file itself.
const KEY: &str = "city";

/// The city's settings in the Zed settings file at `path`, with edits made to that file while Zed
/// runs sent to the city as they land.
pub(crate) fn from_settings_file(fs: Arc<dyn Fs>, path: PathBuf, cx: &mut App) -> Settings {
    let store = Rc::new(ZedSettingsStore {
        path: path.clone(),
        last: RefCell::new(None),
    });
    let (sender, updates) = unbounded();
    let (mut texts, watching) = settings::watch_config_file(cx.background_executor(), fs, path);
    let followed = store.clone();
    cx.spawn(async move |_| {
        let _watching = watching;
        while let Some(text) = texts.next().await {
            let Ok(Some(saved)) = parse(&text) else {
                continue;
            };
            let known = followed.last.replace(Some(saved.clone()));
            if known.as_ref() != Some(&saved) && sender.unbounded_send(saved).is_err() {
                break;
            }
        }
    })
    .detach();
    Settings { store, updates }
}

struct ZedSettingsStore {
    path: PathBuf,
    /// What the city last loaded, saved or was sent, so a write of the same settings, such as
    /// the city's own save coming back through the watcher, is not sent again.
    last: RefCell<Option<Saved>>,
}

impl SettingsStore for ZedSettingsStore {
    fn load(&self) -> Result<Option<Saved>, String> {
        let saved = read_text(&self.path)
            .map_err(|e| e.to_string())
            .and_then(|text| parse(&text))
            .map_err(|e| format!("{}: {e}", self.path.display()))?;
        if saved.is_some() {
            self.last.replace(saved.clone());
        }
        Ok(saved)
    }

    fn save(&self, saved: &Saved) -> Result<String, String> {
        // A settings file kept elsewhere through a link stays linked.
        let path = std::fs::canonicalize(&self.path).unwrap_or_else(|_| self.path.clone());
        let text = read_text(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let text = with_city(text, saved).map_err(|e| format!("{}: {e}", path.display()))?;
        self.last.replace(Some(saved.clone()));
        write_text(&path, &text).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(format!("\"{KEY}\" in {}", path.display()))
    }
}

/// The settings file's text, empty when there is no file yet.
fn read_text(path: &Path) -> io::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e),
    }
}

/// Writes through a file beside `path` and a rename, so Zed's watcher never reads half a file.
fn write_text(path: &Path, text: &str) -> io::Result<()> {
    if let Some(folder) = path.parent() {
        std::fs::create_dir_all(folder)?;
    }
    let partial = path.with_extension("json.city-part");
    std::fs::write(&partial, text)?;
    std::fs::rename(partial, path)
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
    use std::{
        fs,
        path::Path,
        time::{Duration, Instant},
    };

    use city_settings::Saved;
    use futures::channel::mpsc::UnboundedReceiver;
    use gpui::{TestAppContext, UpdateGlobal as _};
    use project::RealFs;
    use settings::SettingsStore;

    use super::{KEY, from_settings_file};

    const DEADLINE: Duration = Duration::from_secs(10);
    const POLL: Duration = Duration::from_millis(20);

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

    fn put_city(path: &Path, text: &str, saved: &Saved) {
        let text = super::with_city(text.to_owned(), saved).unwrap();
        fs::write(path, text).unwrap();
    }

    fn next_update<T>(updates: &mut UnboundedReceiver<T>, cx: &mut TestAppContext) -> T {
        let started = Instant::now();
        loop {
            cx.executor().advance_clock(Duration::from_millis(200));
            cx.run_until_parked();
            if let Ok(update) = updates.try_recv() {
                return update;
            }
            assert!(started.elapsed() < DEADLINE, "no update arrived");
            std::thread::sleep(POLL);
        }
    }

    fn settle(cx: &mut TestAppContext) {
        for _ in 0..10 {
            cx.executor().advance_clock(Duration::from_millis(200));
            cx.run_until_parked();
            std::thread::sleep(POLL);
        }
    }

    // The city reads its settings from Zed's own file, and a file without them is simply new.
    #[gpui::test]
    async fn the_city_loads_its_settings_from_zeds_settings_file(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let real_fs = RealFs::new(None, cx.executor());

        let store = cx
            .update(|cx| from_settings_file(real_fs, path.clone(), cx))
            .store;
        assert_eq!(store.load(), Ok(None));

        fs::write(&path, USER_SETTINGS).unwrap();
        assert_eq!(store.load(), Ok(None));

        put_city(&path, USER_SETTINGS, &tuned(40.));
        assert_eq!(store.load(), Ok(Some(tuned(40.))));

        fs::write(
            &path,
            "{ \"city\": { \"tuning\": { \"pitch\": \"steep\" } } }",
        )
        .unwrap();
        let error = store.load().unwrap_err();
        assert!(error.contains("\"city\""), "{error}");
    }

    // Saving changes only the city's key: Zed's own settings, comments and all, read as before.
    #[gpui::test]
    async fn the_city_saves_into_zeds_settings_file_and_leaves_the_rest(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, USER_SETTINGS).unwrap();
        let real_fs = RealFs::new(None, cx.executor());
        let store = cx
            .update(|cx| from_settings_file(real_fs, path.clone(), cx))
            .store;

        let saved_to = store.save(&tuned(50.)).unwrap();
        assert!(saved_to.contains("settings.json"), "{saved_to}");
        let text = fs::read_to_string(&path).unwrap();
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
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        put_city(&path, USER_SETTINGS, &tuned(40.));
        let real_fs = RealFs::new(None, cx.executor());
        let mut city = cx.update(|cx| from_settings_file(real_fs, path.clone(), cx));
        assert_eq!(city.store.load(), Ok(Some(tuned(40.))));
        settle(cx);

        city.store.save(&tuned(45.)).unwrap();
        settle(cx);
        assert!(
            city.updates.try_recv().is_err(),
            "the city's own save came back"
        );

        let edited = fs::read_to_string(&path).unwrap();
        put_city(&path, &edited, &tuned(70.));
        assert_eq!(next_update(&mut city.updates, cx), tuned(70.));
    }
}
