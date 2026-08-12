pub mod brand_raster;
pub mod menu_bar;
pub mod notifier;

use objc2_foundation::NSBundle;

pub(crate) fn bundle_identifier() -> Option<String> {
    NSBundle::mainBundle()
        .bundleIdentifier()
        .map(|identifier| identifier.to_string())
}
