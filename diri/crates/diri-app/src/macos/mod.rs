pub mod brand_raster;
pub mod browser;
pub mod menu_bar;
pub mod notifier;
pub(crate) mod terminal_keys;

use objc2_foundation::NSBundle;

pub(crate) fn bundle_identifier() -> Option<String> {
    NSBundle::mainBundle()
        .bundleIdentifier()
        .map(|identifier| identifier.to_string())
}

#[cfg(test)]
pub(crate) mod tab_gesture;
