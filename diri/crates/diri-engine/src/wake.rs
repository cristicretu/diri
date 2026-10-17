//! Waking the Mac for a scheduled run.
//!
//! Only root may schedule a wake on macOS, so a tiny privileged helper
//! (`diri-wake-helper`) does it. The user approves it once in System
//! Settings > Login Items; launchd then starts it on demand when the Engine
//! connects to its socket, and it exits when idle. It can do exactly three
//! things for the connecting user: replace that user's diri wake times, report
//! them, and put the Mac back to sleep if the console user has been idle for
//! as long as the Engine says its run took. Nothing else runs as root.
//!
//! Wire: one JSON request line, one JSON response line, per connection.

use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(unix)]
use std::time::Duration;

/// Per-Engine power endpoints. Tests use a private socket and a harmless
/// executable (or `None`) for *both* assertion and dark-wake commands.
#[derive(Clone, Debug)]
pub struct PowerConfig {
    pub socket_path: std::path::PathBuf,
    pub caffeinate: Option<std::path::PathBuf>,
}

impl Default for PowerConfig {
    fn default() -> Self {
        Self {
            socket_path: SOCKET_PATH.into(),
            caffeinate: cfg!(target_os = "macos").then(|| "/usr/bin/caffeinate".into()),
        }
    }
}

impl PowerConfig {
    pub fn call(&self, request: &Request) -> Result<Response, String> {
        call_at(&self.socket_path, request)
    }

    pub(crate) fn command(&self, args: &[&str]) -> Option<std::process::Child> {
        std::process::Command::new(self.caffeinate.as_ref()?)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()
    }
}

/// launchd creates this socket from the helper's plist (`Sockets`).
pub const SOCKET_PATH: &str = "/var/run/com.dirijor.diri.wake.sock";
/// launchd label and plist name the app registers with SMAppService.
pub const HELPER_LABEL: &str = "com.dirijor.diri.wake";
/// Wake this long before a run is due, so the Engine is up to fire it.
pub const WAKE_LEAD_MS: i64 = 2 * 60 * 1000;
/// At most this many wakes per user are kept scheduled.
pub const MAX_WAKES: usize = 64;
/// Wakes further out than this are refused; the Engine only asks for each
/// schedule's next occurrence anyway.
pub const MAX_WAKE_AHEAD_MS: i64 = 35 * 24 * 60 * 60 * 1000;
/// Requests for sleep below this idle time are refused.
pub const MIN_SLEEP_IDLE_SECS: u64 = 60;
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum Request {
    /// Replace every diri wake for the calling user with these times.
    #[serde(rename_all = "camelCase")]
    SetWakes {
        times_ms: Vec<i64>,
    },
    /// Sleep now, but only if the console belongs to the caller and nobody
    /// has touched a keyboard, mouse, or trackpad for `min_idle_secs`.
    #[serde(rename_all = "camelCase")]
    SleepIfIdle {
        min_idle_secs: u64,
        /// Recompute the idle requirement when the request is handled, so time
        /// queued behind another client cannot hide intervening input.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        idle_since_ms: Option<i64>,
    },
    Status,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The caller's diri wake times now scheduled.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wakes_ms: Vec<i64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub slept: bool,
    /// The kernel reports an RTC wake, rather than input/lid/network activity.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub timer_wake: bool,
}

impl Response {
    pub fn failure(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(error.into()),
            ..Self::default()
        }
    }
}

/// The owner tag the helper stamps on a user's wake events, so one user can
/// never cancel another's, or anything macOS or other apps scheduled.
pub fn owner_tag(uid: u32) -> String {
    format!("{HELPER_LABEL}.{uid}")
}

/// Sorted, de-duplicated, whole-second wake times, or why they are refused.
pub fn validate_wakes(times_ms: &[i64], now_ms: i64) -> Result<Vec<i64>, String> {
    if times_ms.len() > MAX_WAKES {
        return Err(format!("at most {MAX_WAKES} wake times"));
    }
    let mut times: Vec<i64> = Vec::with_capacity(times_ms.len());
    for &time in times_ms {
        if time <= now_ms + 30_000 || time > now_ms + MAX_WAKE_AHEAD_MS {
            return Err("wake times must be between 30 seconds and 35 days ahead".into());
        }
        times.push(time.div_euclid(1000) * 1000);
    }
    times.sort_unstable();
    times.dedup();
    Ok(times)
}

/// What the helper must cancel and add to turn `existing` into `desired`.
/// Both are whole-second epoch milliseconds owned by one user.
pub fn reconcile(existing: &[i64], desired: &[i64]) -> (Vec<i64>, Vec<i64>) {
    let cancel = existing
        .iter()
        .copied()
        .filter(|time| !desired.contains(time))
        .collect();
    let add = desired
        .iter()
        .copied()
        .filter(|time| !existing.contains(time))
        .collect();
    (cancel, add)
}

/// A bounded newline frame with an absolute deadline, including slow senders.
#[cfg(unix)]
pub fn read_frame(stream: &mut UnixStream) -> Result<String, String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .filter(|time| !time.is_zero())
            .ok_or("wake request timed out")?;
        // Poll instead of re-arming SO_RCVTIMEO: Darwin rejects that with
        // EINVAL once the peer has closed, even with its last bytes queued.
        let mut descriptor = libc::pollfd {
            fd: stream.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;
        // SAFETY: one initialized descriptor, live throughout the bounded wait.
        if unsafe { libc::poll(&mut descriptor, 1, timeout) } < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.to_string());
        }
        if descriptor.revents == 0 {
            continue;
        }
        let count = match stream.read(&mut buffer) {
            Ok(count) => count,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error.to_string()),
        };
        if count == 0 {
            return Err("incomplete wake frame".into());
        }
        let end = buffer[..count].iter().position(|byte| *byte == b'\n');
        let take = end.map_or(count, |index| index + 1);
        if bytes.len() + take > MAX_REQUEST_BYTES {
            return Err("wake frame too large".into());
        }
        bytes.extend_from_slice(&buffer[..take]);
        if end.is_some() {
            return String::from_utf8(bytes).map_err(|_| "invalid wake frame".into());
        }
    }
}

/// Kernel wake reasons differ between Intel and Apple Silicon. Unknown
/// reasons fail closed; an alarm coinciding with explicit user activity does too.
pub fn is_timer_wake(reason: &str) -> bool {
    let words: Vec<_> = reason
        .split(|c: char| !c.is_ascii_alphanumeric())
        .map(str::to_ascii_lowercase)
        .collect();
    words.iter().any(|word| word == "rtc")
        && !words.iter().any(|word| {
            word.starts_with("lid")
                || word.starts_with("hid")
                || word.starts_with("user")
                || word == "pwrbtn"
        })
}

pub fn sleep_idle_requirement(minimum: u64, since: Option<i64>, now: i64) -> Result<u64, String> {
    if minimum < MIN_SLEEP_IDLE_SECS {
        return Err(format!(
            "minimum idle time is {MIN_SLEEP_IDLE_SECS} seconds"
        ));
    }
    match since {
        Some(since) if since > now => Err("sleep observation is in the future".into()),
        Some(since) => Ok(minimum.max((now.saturating_sub(since) as u64).div_ceil(1000))),
        None => Ok(minimum),
    }
}

/// Pure authorization decision; unreadable console/HID state fails closed.
pub fn sleep_allowed(uid: u32, console: Option<u32>, idle: Option<u64>, minimum: u64) -> bool {
    minimum >= MIN_SLEEP_IDLE_SECS
        && console == Some(uid)
        && idle.is_some_and(|idle| idle >= minimum)
}

/// An already armed imminent alarm can be retained while other alarms change.
/// Only newly added alarms need the installation lead time.
pub fn validate_replacement(times: &[i64], existing: &[i64], now: i64) -> Result<Vec<i64>, String> {
    if times.len() > MAX_WAKES {
        return Err(format!("at most {MAX_WAKES} wake times"));
    }
    let mut result = Vec::new();
    for &time in times {
        let rounded = time.div_euclid(1000) * 1000;
        if existing.contains(&rounded) && rounded > now && rounded <= now + MAX_WAKE_AHEAD_MS {
            result.push(rounded);
        } else {
            result.extend(validate_wakes(&[time], now)?);
        }
    }
    result.sort_unstable();
    result.dedup();
    Ok(result)
}

/// One request to the helper. Fails fast when it is not installed.
pub fn call(request: &Request) -> Result<Response, String> {
    call_at(SOCKET_PATH, request)
}

#[cfg(not(unix))]
pub fn call_at(_path: impl AsRef<std::path::Path>, _request: &Request) -> Result<Response, String> {
    Err("waking the computer for a scheduled run isn't supported on this platform".to_owned())
}

#[cfg(unix)]
pub fn call_at(path: impl AsRef<std::path::Path>, request: &Request) -> Result<Response, String> {
    // A full launchd backlog must not hang the scheduler indefinitely.
    let connect = || -> std::io::Result<UnixStream> {
        let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
        socket.connect_timeout(
            &socket2::SockAddr::unix(path.as_ref())?,
            Duration::from_secs(5),
        )?;
        Ok(UnixStream::from(std::os::fd::OwnedFd::from(socket)))
    };
    let mut stream =
        connect().map_err(|error| format!("the wake helper isn't available ({error})"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .and_then(|()| stream.set_write_timeout(Some(Duration::from_secs(5))))
        .map_err(|error| error.to_string())?;
    let mut line = serde_json::to_vec(request).map_err(|error| error.to_string())?;
    line.push(b'\n');
    stream.write_all(&line).map_err(|error| error.to_string())?;
    let reply = read_frame(&mut stream)?;
    let response: Response =
        serde_json::from_str(&reply).map_err(|_| "the wake helper sent a bad reply".to_owned())?;
    if response.ok {
        Ok(response)
    } else {
        Err(response
            .error
            .unwrap_or_else(|| "the wake helper refused".to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000_000;

    #[test]
    fn only_recognized_timer_wakes_grant_sleep_provenance() {
        assert!(is_timer_wake("RTC (Alarm)"));
        assert!(is_timer_wake("NUB.SPMI0Sw3IRQ nub-spmi0.0x02 rtc/"));
        for reason in [
            "",
            "LID0",
            "HID Activity",
            "RTC/UserActivity",
            "Network",
            "RTC PWRBTN",
        ] {
            assert!(!is_timer_wake(reason), "{reason}");
        }
    }

    #[test]
    fn queued_sleep_request_does_not_hide_input_since_wake() {
        let minimum = sleep_idle_requirement(300, Some(0), 305_001).unwrap();
        assert_eq!(minimum, 306);
        assert!(!sleep_allowed(501, Some(501), Some(304), minimum));
        assert!(sleep_idle_requirement(60, Some(100), 99).is_err());
        assert!(sleep_idle_requirement(59, None, 0).is_err());
    }

    #[test]
    fn sleep_authorization_fails_closed_for_other_users_and_input() {
        assert!(sleep_allowed(501, Some(501), Some(301), 300));
        assert!(!sleep_allowed(501, Some(502), Some(301), 300));
        assert!(!sleep_allowed(501, None, Some(301), 300));
        assert!(!sleep_allowed(501, Some(501), None, 300));
        assert!(!sleep_allowed(501, Some(501), Some(299), 300));
        assert!(!sleep_allowed(501, Some(501), Some(301), 59));
    }

    #[test]
    fn replacement_retains_imminent_owned_alarms_but_refuses_new_ones() {
        assert_eq!(
            validate_replacement(&[NOW + 10_000], &[NOW + 10_000], NOW).unwrap(),
            vec![NOW + 10_000]
        );
        assert!(validate_replacement(&[NOW + 10_000], &[], NOW).is_err());
        assert!(validate_replacement(&[NOW - 10_000], &[NOW - 10_000], NOW).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn frame_requires_newline_and_rejects_oversized_input() {
        for bytes in [b"{}".to_vec(), vec![b'x'; MAX_REQUEST_BYTES + 1]] {
            let (mut reader, mut writer) = UnixStream::pair().unwrap();
            let sending = std::thread::spawn(move || {
                let _ = writer.write_all(&bytes);
            });
            assert!(read_frame(&mut reader).is_err());
            drop(reader);
            sending.join().unwrap();
        }
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(b"{}\n").unwrap();
        assert_eq!(read_frame(&mut reader).unwrap(), "{}\n");
    }

    #[test]
    fn power_commands_can_be_replaced_without_global_environment() {
        let config = PowerConfig {
            socket_path: "/nonexistent/private.sock".into(),
            caffeinate: Some("/usr/bin/true".into()),
        };
        assert!(
            config
                .command(&["-u", "-t", "5"])
                .unwrap()
                .wait()
                .unwrap()
                .success()
        );
        assert!(
            config
                .command(&["-i", "-w", "1"])
                .unwrap()
                .wait()
                .unwrap()
                .success()
        );
    }

    #[test]
    fn requests_round_trip_in_camel_case() {
        let request = Request::SetWakes {
            times_ms: vec![NOW],
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["op"], "setWakes");
        assert_eq!(json["timesMs"][0], NOW);
        let sleep: Request =
            serde_json::from_str(r#"{"op":"sleepIfIdle","minIdleSecs":90}"#).unwrap();
        assert_eq!(
            sleep,
            Request::SleepIfIdle {
                min_idle_secs: 90,
                idle_since_ms: None
            }
        );
        assert!(serde_json::from_str::<Request>(r#"{"op":"shell","cmd":"id"}"#).is_err());
    }

    #[test]
    fn wake_times_are_bounded_rounded_and_deduplicated() {
        let hour = 3_600_000;
        assert_eq!(
            validate_wakes(&[NOW + 2 * hour + 999, NOW + hour, NOW + hour + 400], NOW).unwrap(),
            vec![NOW + hour, NOW + 2 * hour]
        );
        assert!(validate_wakes(&[NOW + 1_000], NOW).is_err(), "too soon");
        assert!(validate_wakes(&[NOW - hour], NOW).is_err(), "past");
        assert!(
            validate_wakes(&[NOW + 40 * 24 * hour], NOW).is_err(),
            "too far"
        );
        let many: Vec<i64> = (1..=65).map(|i| NOW + i * hour).collect();
        assert!(validate_wakes(&many, NOW).is_err(), "too many");
        assert_eq!(validate_wakes(&[], NOW).unwrap(), Vec::<i64>::new());
    }

    #[test]
    fn reconcile_touches_only_the_difference() {
        let (cancel, add) = reconcile(&[1, 2, 3], &[2, 3, 4]);
        assert_eq!(cancel, vec![1]);
        assert_eq!(add, vec![4]);
        assert_eq!(reconcile(&[5], &[5]), (vec![], vec![]));
    }

    #[test]
    fn owner_tags_are_per_user() {
        assert_eq!(owner_tag(501), "com.dirijor.diri.wake.501");
        assert_ne!(owner_tag(501), owner_tag(502));
    }

    #[test]
    fn a_missing_helper_fails_fast_with_a_readable_error() {
        let error = call_at("/nonexistent/diri-wake.sock", &Request::Status).unwrap_err();
        assert!(error.contains("isn't available"), "{error}");
    }
}
