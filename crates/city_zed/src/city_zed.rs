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
