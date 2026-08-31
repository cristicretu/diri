//! The frontend, compiled into the binary.
//!
//! One file, no build step, no CDN. That is deliberate: the response CSP
//! forbids every external origin, and a phone on a hotel network should not
//! need anything but the tailnet to render this page.

pub const INDEX_HTML: &str = include_str!("ui/index.html");
pub const MANIFEST: &str = include_str!("ui/manifest.webmanifest");
pub const ICON_SVG: &str = include_str!("ui/icon.svg");

#[cfg(test)]
mod tests {
    use super::*;

    /// The page must not reach for anything the CSP will refuse; a blocked
    /// asset on a phone looks like a broken app with no console to check.
    #[test]
    fn the_page_loads_nothing_from_another_origin() {
        assert!(!INDEX_HTML.contains("https://"), "external URL in the page");
        assert!(!INDEX_HTML.contains("http://"), "external URL in the page");
    }

    #[test]
    fn the_manifest_is_valid_json_naming_the_app() {
        let parsed: serde_json::Value = serde_json::from_str(MANIFEST).expect("valid manifest");
        assert_eq!(parsed["name"], "diri");
        assert_eq!(parsed["display"], "standalone");
    }

    #[test]
    fn the_page_declares_the_home_screen_metadata() {
        for required in [
            "apple-mobile-web-app-capable",
            "viewport-fit=cover",
            "/manifest.webmanifest",
        ] {
            assert!(INDEX_HTML.contains(required), "missing {required}");
        }
    }
}
