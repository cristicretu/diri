//! Durable screen checkpoints: `<id>.screen.plist`. Version 2 remains
//! load-compatible with the historical Swift checkpoint; version 3 preserves
//! granular mouse state.
//!
//! A checkpoint pairs an RLE-encoded full grid with the exact raw-log offset
//! it represents, so a restarted daemon can seed the emulator from a few
//! kilobytes and replay only the bytes written since — instead of pushing a
//! 256 KiB raw tail through the emulator per adopted session.
//!
//! Checkpoints are an acceleration cache, never authoritative state: a
//! malformed, stale, or future-version file is ignored and the bounded
//! raw-log replay runs instead. The on-disk format is a property list (either
//! binary or XML is read). Version 2's historical keys still load during an
//! upgrade; a rollback safely treats version 3 as a cache miss and rebuilds
//! from the authoritative raw log.

use std::path::Path;

use diri_proto::grid::{GridCell, GridRowCodec, GridUpdate};
use diri_proto::terminal::{MouseEncoding, MouseModes, MouseTrackingMode};

// Version 1 persisted only the visible grid. Restoring it silently collapsed
// every adopted session to at most one line of history, so it is deliberately
// treated as a cache miss and rebuilt from the authoritative raw log.
const PREVIOUS_VERSION: u64 = 2;
const CURRENT_VERSION: u64 = 3;

/// A decoded checkpoint, grid already validated.
pub struct ScreenCheckpoint {
    pub log_offset: u64,
    /// Rows above the visible grid, oldest first.
    pub history: Vec<Vec<GridCell>>,
    pub grid: GridUpdate,
    /// Partial exit-marker bytes that were pending when the checkpoint was
    /// taken; replaying resumes with them so a marker split across the
    /// checkpoint boundary is still recognized.
    pub marker_buffer: Vec<u8>,
    pub alt_screen: bool,
    pub bracketed_paste: bool,
    pub mouse: MouseModes,
}

impl ScreenCheckpoint {
    /// The checkpoint file that belongs to an output log: `<id>.bin` →
    /// `<id>.screen.plist`, in the same directory.
    pub fn path_for_log(log_path: &Path) -> std::path::PathBuf {
        log_path.with_extension("screen.plist")
    }

    /// Loads and validates; `None` for anything unusable, exactly like the
    /// Swift loader — a bad cache is a cache miss, not an error.
    pub fn load(path: &Path) -> Option<Self> {
        let value = plist::Value::from_file(path).ok()?;
        let dict = value.as_dictionary()?;
        let version = dict.get("version")?.as_unsigned_integer()?;
        if !matches!(version, PREVIOUS_VERSION | CURRENT_VERSION) {
            return None;
        }
        let grid = GridUpdate::decode(as_data(dict.get("gridPayload")?)?).ok()?;
        let history_payload = as_data(dict.get("historyPayload")?)?;
        let history_row_count =
            usize::try_from(dict.get("historyRowCount")?.as_unsigned_integer()?).ok()?;
        // Every encoded row has at least its two-byte run count. Validate
        // before the codec allocates from an untrusted plist integer.
        if history_row_count > history_payload.len() / 2 {
            return None;
        }
        let history = GridRowCodec::decode_rows(history_payload, history_row_count).ok()?;
        if history
            .iter()
            .any(|row| row.len() != usize::from(grid.cols))
        {
            return None;
        }
        let mouse = if version == PREVIOUS_VERSION {
            if dict.get("mouseReporting")?.as_boolean()? {
                // Version 2's restore implementation always synthesized this
                // exact pair. This is checkpoint compatibility, not a claim
                // about the unknowable details of an old live wire peer.
                MouseModes::new(MouseTrackingMode::ButtonEvents, MouseEncoding::Sgr)
            } else {
                MouseModes::OFF
            }
        } else {
            MouseModes::new(
                MouseTrackingMode::from_wire(
                    u8::try_from(dict.get("mouseTrackingMode")?.as_unsigned_integer()?).ok()?,
                )?,
                MouseEncoding::from_wire(
                    u8::try_from(dict.get("mouseEncoding")?.as_unsigned_integer()?).ok()?,
                )?,
            )
        };
        Some(Self {
            log_offset: dict.get("logOffset")?.as_unsigned_integer()?,
            history,
            grid,
            marker_buffer: as_data(dict.get("markerBuffer")?)?.to_vec(),
            alt_screen: dict.get("altScreen")?.as_boolean()?,
            bracketed_paste: dict.get("bracketedPaste")?.as_boolean()?,
            mouse,
        })
    }

    /// Writes atomically (temp file + rename) as a binary plist.
    pub fn write_atomically(&self, path: &Path) -> std::io::Result<()> {
        let mut dict = plist::Dictionary::new();
        dict.insert(
            "version".into(),
            plist::Value::Integer(CURRENT_VERSION.into()),
        );
        dict.insert(
            "logOffset".into(),
            plist::Value::Integer(self.log_offset.into()),
        );
        dict.insert(
            "gridPayload".into(),
            plist::Value::Data(self.grid.encode().map_err(std::io::Error::other)?),
        );
        dict.insert(
            "historyRowCount".into(),
            plist::Value::Integer(
                u64::try_from(self.history.len())
                    .map_err(std::io::Error::other)?
                    .into(),
            ),
        );
        dict.insert(
            "historyPayload".into(),
            plist::Value::Data(
                GridRowCodec::encode_rows(&self.history).map_err(std::io::Error::other)?,
            ),
        );
        dict.insert(
            "markerBuffer".into(),
            plist::Value::Data(self.marker_buffer.clone()),
        );
        dict.insert("altScreen".into(), plist::Value::Boolean(self.alt_screen));
        dict.insert(
            "bracketedPaste".into(),
            plist::Value::Boolean(self.bracketed_paste),
        );
        dict.insert(
            "mouseReporting".into(),
            plist::Value::Boolean(self.mouse.is_reporting()),
        );
        dict.insert(
            "mouseTrackingMode".into(),
            plist::Value::Integer(u64::from(self.mouse.tracking as u8).into()),
        );
        dict.insert(
            "mouseEncoding".into(),
            plist::Value::Integer(u64::from(self.mouse.encoding as u8).into()),
        );

        let temp = path.with_extension("plist.tmp");
        plist::Value::Dictionary(dict)
            .to_file_binary(&temp)
            .map_err(std::io::Error::other)?;
        std::fs::rename(&temp, path)
    }
}

fn as_data(value: &plist::Value) -> Option<&[u8]> {
    match value {
        plist::Value::Data(data) => Some(data.as_slice()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::grid::{ChangedRow, GridCell};

    fn sample() -> ScreenCheckpoint {
        let cells = vec![GridCell::BLANK; 4];
        ScreenCheckpoint {
            log_offset: 12345,
            history: vec![vec![GridCell::BLANK; 4]],
            grid: GridUpdate {
                cols: 4,
                rows: 2,
                cursor_col: 1,
                cursor_row: 1,
                cursor_visible: true,
                is_full_snapshot: true,
                changed_rows: vec![ChangedRow::new(0, cells.clone()), ChangedRow::new(1, cells)],
            },
            marker_buffer: vec![0x1b, b']'],
            alt_screen: true,
            bracketed_paste: false,
            mouse: MouseModes::new(MouseTrackingMode::AnyMotion, MouseEncoding::Sgr),
        }
    }

    #[test]
    fn a_checkpoint_round_trips_through_disk() {
        let dir = tempfile::tempdir().expect("temp");
        let path = dir.path().join("s_x.screen.plist");
        sample().write_atomically(&path).expect("write");

        let loaded = ScreenCheckpoint::load(&path).expect("load");
        assert_eq!(loaded.log_offset, 12345);
        assert_eq!(loaded.history, sample().history);
        assert_eq!(loaded.grid, sample().grid);
        assert_eq!(loaded.marker_buffer, vec![0x1b, b']']);
        assert!(loaded.alt_screen);
        assert!(!loaded.bracketed_paste);
        assert_eq!(loaded.mouse, sample().mouse);
    }

    #[test]
    fn the_on_disk_format_is_a_binary_plist_with_versioned_keys() {
        let dir = tempfile::tempdir().expect("temp");
        let path = dir.path().join("s_x.screen.plist");
        sample().write_atomically(&path).expect("write");

        let bytes = std::fs::read(&path).expect("read");
        assert!(
            bytes.starts_with(b"bplist00"),
            "PropertyListEncoder parity means binary plist"
        );
        let keys = plist::Value::from_file(&path)
            .expect("read binary plist")
            .into_dictionary()
            .expect("dictionary");
        for key in [
            "version",
            "logOffset",
            "gridPayload",
            "historyRowCount",
            "historyPayload",
            "markerBuffer",
            "altScreen",
            "bracketedPaste",
            "mouseReporting",
            "mouseTrackingMode",
            "mouseEncoding",
        ] {
            assert!(keys.contains_key(key), "missing Swift key {key}");
        }

        // Apple's own parser is the strongest rollback check, but `plutil`
        // is a macOS tool. Keep that assertion on Apple runners while the
        // platform-independent header/key assertions still run on Linux.
        #[cfg(target_os = "macos")]
        {
            let lint = std::process::Command::new("plutil")
                .arg("-lint")
                .arg(&path)
                .output()
                .expect("plutil");
            assert!(
                lint.status.success(),
                "plutil rejected our plist: {}",
                String::from_utf8_lossy(&lint.stdout)
            );
        }
    }

    #[test]
    fn a_swift_shaped_plist_written_by_apples_tools_loads() {
        // The forward direction of the mixed-fleet upgrade: a checkpoint
        // with the same keys and value types PropertyListEncoder uses must
        // load here. On macOS, additionally convert it with Apple's plist
        // implementation before loading; Linux has no `plutil`, so it reads
        // the equivalent standard XML representation directly.
        let dir = tempfile::tempdir().expect("temp");
        let path = dir.path().join("s_swift.screen.plist");
        let payload = sample().grid.encode().expect("encode");
        let history_payload = GridRowCodec::encode_rows(&sample().history).expect("history");
        use base64::Engine as _;
        let base64 = base64::engine::general_purpose::STANDARD.encode(&payload);
        let history_base64 = base64::engine::general_purpose::STANDARD.encode(&history_payload);
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>version</key><integer>2</integer>
    <key>logOffset</key><integer>777</integer>
    <key>gridPayload</key><data>{base64}</data>
    <key>historyRowCount</key><integer>1</integer>
    <key>historyPayload</key><data>{history_base64}</data>
    <key>markerBuffer</key><data></data>
    <key>altScreen</key><false/>
    <key>bracketedPaste</key><true/>
    <key>mouseReporting</key><false/>
</dict>
</plist>"#
        );
        std::fs::write(&path, xml).expect("write xml");
        #[cfg(target_os = "macos")]
        {
            let convert = std::process::Command::new("plutil")
                .args(["-convert", "binary1"])
                .arg(&path)
                .output()
                .expect("plutil");
            assert!(convert.status.success());
            assert!(
                std::fs::read(&path).expect("read").starts_with(b"bplist00"),
                "converted to binary"
            );
        }

        let loaded = ScreenCheckpoint::load(&path).expect("load Swift-shaped plist");
        assert_eq!(loaded.log_offset, 777);
        assert_eq!(loaded.history, sample().history);
        assert_eq!(loaded.grid, sample().grid);
        assert!(loaded.bracketed_paste);
        assert_eq!(loaded.mouse, MouseModes::OFF);
    }

    #[test]
    fn version_two_enabled_mouse_state_loads_with_its_historical_semantics() {
        let dir = tempfile::tempdir().expect("temp");
        let path = dir.path().join("s_v2.screen.plist");
        sample().write_atomically(&path).expect("write");
        let mut dict = plist::Value::from_file(&path)
            .expect("read")
            .into_dictionary()
            .expect("dictionary");
        dict.insert(
            "version".into(),
            plist::Value::Integer(PREVIOUS_VERSION.into()),
        );
        dict.remove("mouseTrackingMode");
        dict.remove("mouseEncoding");
        plist::Value::Dictionary(dict)
            .to_file_binary(&path)
            .expect("rewrite v2");

        let loaded = ScreenCheckpoint::load(&path).expect("load v2");
        assert_eq!(
            loaded.mouse,
            MouseModes::new(MouseTrackingMode::ButtonEvents, MouseEncoding::Sgr),
            "v2 restore used to synthesize DECSET 1000 and DECSET 1006"
        );
    }

    #[test]
    fn version_and_shape_mismatches_are_cache_misses() {
        let dir = tempfile::tempdir().expect("temp");
        let path = dir.path().join("s_x.screen.plist");

        assert!(
            ScreenCheckpoint::load(&path).is_none(),
            "a missing file is a miss"
        );

        std::fs::write(&path, b"not a plist at all").expect("write");
        assert!(ScreenCheckpoint::load(&path).is_none(), "garbage is a miss");

        let mut future = sample();
        future.write_atomically(&path).expect("write");
        // Rewrite with a future version.
        let mut dict = plist::Value::from_file(&path)
            .expect("read back")
            .into_dictionary()
            .expect("dict");
        dict.insert(
            "version".into(),
            plist::Value::Integer((CURRENT_VERSION + 1).into()),
        );
        plist::Value::Dictionary(dict)
            .to_file_binary(&path)
            .expect("rewrite");
        assert!(
            ScreenCheckpoint::load(&path).is_none(),
            "a future version is a miss"
        );

        // A truncated grid payload is a miss, not a panic.
        future.grid.changed_rows.clear();
        future.grid.is_full_snapshot = true;
        let mut dict = plist::Dictionary::new();
        dict.insert("version".into(), plist::Value::Integer(2.into()));
        dict.insert("logOffset".into(), plist::Value::Integer(1.into()));
        dict.insert("gridPayload".into(), plist::Value::Data(vec![0xde, 0xad]));
        dict.insert("historyRowCount".into(), plist::Value::Integer(0.into()));
        dict.insert("historyPayload".into(), plist::Value::Data(Vec::new()));
        dict.insert("markerBuffer".into(), plist::Value::Data(Vec::new()));
        dict.insert("altScreen".into(), plist::Value::Boolean(false));
        dict.insert("bracketedPaste".into(), plist::Value::Boolean(false));
        dict.insert("mouseReporting".into(), plist::Value::Boolean(false));
        plist::Value::Dictionary(dict)
            .to_file_binary(&path)
            .expect("write");
        assert!(
            ScreenCheckpoint::load(&path).is_none(),
            "an undecodable grid is a miss"
        );
    }
}
