//! Per-tab Session memory: which Session feedback from a page went to last.
//!
//! A bounded LRU map from page key to Session. Recording writes the page's
//! whole lookup chain (the page and its parents), so a new thread in a
//! remembered Slack channel still suggests that channel's Session, while a
//! remembered thread always beats its channel. Pure; the store persists it.
//! `formal/ContextRing.lean` proves lookup-after-record and the bound.

use serde::{Deserialize, Serialize};

use super::tab_key::lookup_chain;

/// The most page keys remembered.
pub const MAX_ENTRIES: usize = 512;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub key: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    pub at_ms: u64,
}

/// Most recently used first.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct Affinity {
    pub entries: Vec<Entry>,
}

impl Affinity {
    /// Remember `session_id` for `key` and every page `key` belongs to.
    pub fn record(&mut self, key: &str, session_id: &str, at_ms: u64) {
        // Parents first, so the exact key ends up most recent.
        for key in lookup_chain(key).into_iter().rev() {
            self.entries.retain(|entry| entry.key != key);
            self.entries.insert(
                0,
                Entry {
                    key,
                    session_id: session_id.to_owned(),
                    at_ms,
                },
            );
        }
        self.entries.truncate(MAX_ENTRIES);
    }

    /// The Session remembered for `key`, most specific page first, skipping
    /// Sessions `alive` rejects (closed, archived).
    pub fn lookup(&self, key: &str, alive: impl Fn(&str) -> bool) -> Option<&str> {
        lookup_chain(key).iter().find_map(|key| {
            self.entries
                .iter()
                .find(|entry| &entry.key == key)
                .map(|entry| entry.session_id.as_str())
                .filter(|session| alive(session))
        })
    }

    /// Forget a Session everywhere, e.g. once it is removed.
    pub fn forget_session(&mut self, session_id: &str) {
        self.entries.retain(|entry| entry.session_id != session_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALIVE: fn(&str) -> bool = |_| true;

    #[test]
    fn remembers_the_last_session_for_a_page() {
        let mut affinity = Affinity::default();
        affinity.record("https://example.com/a", "s1", 1);
        affinity.record("https://example.com/b", "s2", 2);
        affinity.record("https://example.com/a", "s3", 3);
        assert_eq!(affinity.lookup("https://example.com/a", ALIVE), Some("s3"));
        assert_eq!(affinity.lookup("https://example.com/b", ALIVE), Some("s2"));
        assert_eq!(affinity.lookup("https://example.com/c", ALIVE), None);
        assert_eq!(affinity.entries[0].key, "https://example.com/a");
    }

    #[test]
    fn a_thread_falls_back_to_its_channel_but_wins_over_it() {
        let thread_a = "https://app.slack.com/client/T/C/thread/C-1.1";
        let thread_b = "https://app.slack.com/client/T/C/thread/C-2.2";
        let mut affinity = Affinity::default();
        affinity.record(thread_a, "s1", 1);
        assert_eq!(affinity.lookup(thread_b, ALIVE), Some("s1"));
        affinity.record(thread_b, "s2", 2);
        assert_eq!(affinity.lookup(thread_a, ALIVE), Some("s1"));
        assert_eq!(affinity.lookup(thread_b, ALIVE), Some("s2"));
        assert_eq!(
            affinity.lookup("https://app.slack.com/client/T/C", ALIVE),
            Some("s2")
        );
    }

    #[test]
    fn dev_server_paths_fall_back_to_the_origin() {
        let mut affinity = Affinity::default();
        affinity.record("http://localhost:3000/settings", "s1", 1);
        assert_eq!(
            affinity.lookup("http://localhost:3000/billing", ALIVE),
            Some("s1")
        );
        assert_eq!(
            affinity.lookup("http://localhost:4000/settings", ALIVE),
            None
        );
    }

    #[test]
    fn closed_sessions_are_skipped() {
        let mut affinity = Affinity::default();
        affinity.record("http://localhost:3000", "old", 1);
        affinity.record("http://localhost:3000/settings", "gone", 2);
        assert_eq!(
            affinity.lookup("http://localhost:3000/settings", |s| s != "gone"),
            None,
            "the exact page's session is gone and the origin was overwritten"
        );
        affinity.record("http://localhost:3000/billing", "live", 3);
        assert_eq!(
            affinity.lookup("http://localhost:3000/settings", |s| s != "gone"),
            Some("live")
        );
        affinity.forget_session("live");
        assert!(affinity.entries.iter().all(|e| e.session_id != "live"));
    }

    #[test]
    fn stays_bounded_and_evicts_least_recent() {
        let mut affinity = Affinity::default();
        for i in 0..(MAX_ENTRIES + 10) {
            affinity.record(&format!("https://example.com/{i}"), "s", i as u64);
        }
        assert_eq!(affinity.entries.len(), MAX_ENTRIES);
        assert_eq!(affinity.lookup("https://example.com/0", ALIVE), None);
        assert_eq!(
            affinity.lookup(&format!("https://example.com/{}", MAX_ENTRIES + 9), ALIVE),
            Some("s")
        );
    }
}
