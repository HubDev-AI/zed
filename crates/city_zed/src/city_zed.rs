#[cfg(target_os = "macos")]
mod city_item;
#[cfg(target_os = "macos")]
mod zed_agents;
#[cfg(target_os = "macos")]
mod zed_feed;
#[cfg(target_os = "macos")]
mod zed_settings;

use gpui::{App, actions};

actions!(
    city,
    [
        /// Opens the project's first folder as a code city in a tab.
        Open
    ]
);

#[cfg(target_os = "macos")]
pub fn init(cx: &mut App) {
    city_item::init(cx);
}

#[cfg(not(target_os = "macos"))]
pub fn init(_: &mut App) {}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use serde_json::{Map, Value, json};

    // eof's own test pins its keymap.json, which fork-apply copies into Zed, to this table.
    #[test]
    fn zed_keymap_holds_eofs_key_table() {
        let keymap: Vec<Value> = settings_json::parse_json_with_comments(include_str!(
            "../../../assets/keymaps/default-macos.json"
        ))
        .unwrap();
        let city_sections: Vec<&Value> = keymap
            .iter()
            .filter(|section| section["context"] == "CityView")
            .collect();
        let bindings: Map<String, Value> = input_keys::BINDINGS
            .iter()
            .map(|&(keys, name, input)| {
                let action = input.map_or(json!(name), |input| json!([name, input]));
                (keys.to_string(), action)
            })
            .collect();
        assert_eq!(
            city_sections,
            [&json!({ "context": "CityView", "bindings": bindings })],
            "run script/fork-apply to copy eof's keymap.json into Zed's keymap"
        );
    }
}
