//! Talk with your agent: when a turn finishes, read what matters in the
//! agent's reply aloud, then optionally hand the microphone to Wispr Flow.
//!
//! Everything stays on this Mac. The bundled `diri-voice` helper digests the
//! reply with Apple's on-device model and speaks it with the system voice for
//! its language. Without the helper or the model (macOS before 26, Apple
//! Intelligence off, a language the model does not speak) Diri reads a plain
//! cut of the reply instead: no code, tables or links, the opening sentences
//! and the agent's last question.
//!
//! The store announces turns (`SessionStore::turn_completions`) only while
//! the setting is on. Speech is a queue: a turn that is no longer the
//! session's latest, or the setting turning off, stops it mid-sentence.
// Only macOS speaks; elsewhere the store still detects turns for the tests.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use diri_proto::attention::AttentionKind;
use diri_proto::{SessionId, SessionRecord};

/// A system voice the reader can use, as `diri-voice voices` lists it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceOption {
    pub id: String,
    pub name: String,
    pub language_name: String,
    /// AVSpeechSynthesisVoiceQuality: 1 basic, 2 enhanced, 3 premium; 4 marks
    /// a Kokoro voice.
    pub quality: u8,
}

/// Spoken at the end of a dictated answer to submit it. Each interface
/// language's phrase, plus English, which works in any of them. Dictation
/// often hears "send" as "sent", and people add "the".
const SEND_PHRASES: [&str; 6] = [
    "send to agent",
    "sent to agent",
    "send to the agent",
    "sent to the agent",
    "enviar al agente",
    "发送给智能体",
];

/// The message to submit when dictated `text` ends with a send phrase, the
/// phrase and its trailing punctuation removed; `None` when it does not.
/// Wispr Flow capitalizes and punctuates ("…, then commit. Send to agent.").
pub(crate) fn dictated_submission(text: &str) -> Option<&str> {
    let trimmed = text.trim_end_matches(|c: char| {
        c.is_whitespace() || matches!(c, '.' | '!' | '?' | ',' | '。' | '！' | '，')
    });
    let lower = trimmed.to_lowercase();
    // Lowercasing these phrases never changes their byte length.
    let phrase = SEND_PHRASES.iter().find(|phrase| {
        lower.ends_with(*phrase)
            && lower.len() == trimmed.len()
            && lower[..lower.len() - phrase.len()]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_alphanumeric() || !phrase.is_ascii())
    })?;
    let message = &trimmed[..trimmed.len() - phrase.len()];
    Some(
        message
            .trim_end_matches(|c: char| c.is_whitespace() || matches!(c, ',' | '-' | '—' | '，')),
    )
}

/// A spoken answer, with the request for a closing `Spoken:` paragraph
/// appended for agents that are not already asked (Claude Code through its
/// prompt hook, Codex through its launch). An empty answer, which only
/// submits what is already typed, is left alone.
pub(crate) fn with_spoken_request(message: String, kind: Option<&diri_proto::AgentKind>) -> String {
    let asked = kind.is_some_and(|kind| {
        matches!(
            kind.id(),
            diri_proto::AgentKind::CLAUDE_CODE_ID | diri_proto::AgentKind::CODEX_ID
        )
    });
    if asked || message.trim().is_empty() {
        return message;
    }
    format!("{message}\n\n({})", diri_proto::voice::SPOKEN_REQUEST)
}

/// The `talk_voice` that pipes readings to the user's own speech command.
pub(crate) const COMMAND_VOICE: &str = "command";

/// What `diri-voice transcribe` heard, as the message to send and whether to
/// submit it: the send phrase submits (and is removed), a pause only types
/// the answer in. `None` when nothing was said.
pub(crate) fn transcribed_answer(output: &str) -> Option<(String, bool)> {
    let value: serde_json::Value = serde_json::from_str(output.trim()).ok()?;
    let text = value.get("text")?.as_str()?.trim();
    if text.is_empty() {
        return None;
    }
    if value.get("reason").and_then(serde_json::Value::as_str) == Some("phrase") {
        let message = heard_submission(text).trim().to_owned();
        return (!message.is_empty()).then_some((message, true));
    }
    Some((text.to_owned(), false))
}

/// The session whose next paste is a spoken answer to submit: the listener
/// heard a send phrase there and stopped Wispr Flow, which pastes next.
static ARMED_SUBMIT: std::sync::Mutex<Option<(SessionId, std::time::Instant)>> =
    std::sync::Mutex::new(None);
/// Wispr Flow pastes within a second or two of being stopped.
const ARMED_FOR: std::time::Duration = std::time::Duration::from_secs(10);

pub(crate) fn arm_submit(session: SessionId) {
    *ARMED_SUBMIT
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some((session, std::time::Instant::now()));
}

/// Whether a paste into `session` should be submitted because the user just
/// said a send phrase there. Disarms either way.
pub(crate) fn take_armed_submit(session: &SessionId) -> bool {
    ARMED_SUBMIT
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
        .is_some_and(|(armed, at)| &armed == session && at.elapsed() < ARMED_FOR)
}

/// A spoken answer the listener heard end with a send phrase. Stopping
/// Wispr Flow the moment the phrase is heard can cut its last words, so a
/// trailing "send" or "send to" goes too.
pub(crate) fn heard_submission(text: &str) -> &str {
    if let Some(message) = dictated_submission(text) {
        return message;
    }
    let trimmed = text.trim_end_matches(|c: char| {
        c.is_whitespace() || matches!(c, '.' | '!' | '?' | ',' | '。' | '！' | '，')
    });
    let lower = trimmed.to_lowercase();
    for cut in ["send to the", "sent to the", "send to", "sent to", "send", "sent"] {
        if lower.len() == trimmed.len()
            && lower.ends_with(cut)
            && lower[..lower.len() - cut.len()]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_alphanumeric())
        {
            return trimmed[..trimmed.len() - cut.len()]
                .trim_end_matches(|c: char| c.is_whitespace() || matches!(c, ',' | '-' | '—'));
        }
    }
    text.trim_end()
}

/// A saved voice's name before the list has loaded: identifiers end with
/// it (`com.apple.voice.premium.en-US.Zoe`).
pub(crate) fn voice_name_from_id(id: &str) -> String {
    // Kokoro voices: "kokoro:af_heart" is Heart.
    if let Some(kokoro) = id.strip_prefix("kokoro:") {
        let name = kokoro.split_once('_').map_or(kokoro, |(_, name)| name);
        let mut chars = name.chars();
        return chars
            .next()
            .map(|first| first.to_uppercase().chain(chars).collect())
            .unwrap_or_default();
    }
    id.rsplit('.').next().unwrap_or(id).to_owned()
}

/// A turn the store saw finish, announced only while the setting is on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnCompletion {
    pub session_id: SessionId,
    pub turn: u64,
}

/// The turn a record update just completed, if any. Hydration, reconnects and
/// a restarted attention history (new epoch) are not new completions, so a
/// launch never reads out every session's last reply.
pub(crate) fn completed_turn(
    previous: Option<&SessionRecord>,
    current: &SessionRecord,
) -> Option<u64> {
    let before = previous?.attention_state.as_ref()?;
    let after = current.attention_state.as_ref()?;
    if before.epoch != after.epoch || after.working {
        return None;
    }
    after
        .events
        .iter()
        .filter(|event| event.kind == AttentionKind::Completion && event.sequence > before.sequence)
        .map(|event| event.turn)
        .max()
}

/// Most of what the on-device model can take in one request (4096 tokens,
/// instructions and answer included).
const MAX_DIGEST_INPUT: usize = 6000;
/// Agents open with the outcome and close with questions; keep both ends.
const DIGEST_HEAD: usize = 1500;
const FALLBACK_WORDS: usize = 70;
const TITLE_WORDS: usize = 8;

/// Markdown reduced to prose a voice can read: no code blocks, tables, rules,
/// links or emphasis markers. Inline code keeps its text.
pub(crate) fn speakable(reply: &str) -> String {
    let mut paragraphs: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut fence: Option<&str> = None;
    let flush = |current: &mut String, paragraphs: &mut Vec<String>| {
        let text = current.trim();
        if !text.is_empty() {
            paragraphs.push(text.to_owned());
        }
        current.clear();
    };
    for line in reply.lines() {
        let trimmed = line.trim();
        if let Some(marker) = fence {
            if trimmed.starts_with(marker) {
                fence = None;
            }
            continue;
        }
        if let Some(marker) = ["```", "~~~"]
            .into_iter()
            .find(|marker| trimmed.starts_with(marker))
        {
            flush(&mut current, &mut paragraphs);
            fence = Some(marker);
            continue;
        }
        if trimmed.is_empty()
            || trimmed.starts_with('|')
            || trimmed
                .chars()
                .all(|c| matches!(c, '-' | '*' | '_' | '=' | ' '))
        {
            flush(&mut current, &mut paragraphs);
            continue;
        }
        let item = strip_line_marker(trimmed);
        let is_item = item.len() != trimmed.len();
        if is_item || trimmed.starts_with('#') {
            flush(&mut current, &mut paragraphs);
        }
        let text = inline_prose(item);
        if text.is_empty() {
            continue;
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(&text);
        if is_item || trimmed.starts_with('#') {
            flush(&mut current, &mut paragraphs);
        }
    }
    flush(&mut current, &mut paragraphs);
    paragraphs.join("\n")
}

fn strip_line_marker(line: &str) -> &str {
    let line = line
        .trim_start_matches('#')
        .trim_start_matches('>')
        .trim_start();
    for bullet in ["- [ ] ", "- [x] ", "- ", "* ", "+ "] {
        if let Some(rest) = line.strip_prefix(bullet) {
            return rest;
        }
    }
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0
        && let Some(rest) = line[digits..]
            .strip_prefix(". ")
            .or_else(|| line[digits..].strip_prefix(") "))
    {
        return rest;
    }
    line
}

fn inline_prose(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(start) = rest.find(['[', '`', '*', '_', 'h']) {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        // [text](url) reads as its text.
        if tail.starts_with('[')
            && let Some(close) = tail.find("](")
            && let Some(end) = tail[close..].find(')')
        {
            out.push_str(&tail[1..close]);
            rest = &tail[close + end + 1..];
            continue;
        }
        // Bare links are noise when spoken.
        if tail.starts_with("http://") || tail.starts_with("https://") {
            let end = tail.find(char::is_whitespace).unwrap_or(tail.len());
            rest = &tail[end..];
            continue;
        }
        let mut chars = tail.chars();
        let first = chars.next().expect("found a char");
        match first {
            '`' | '*' => {}
            // Underscores inside identifiers stay; emphasis markers go.
            '_' if out.chars().last().is_none_or(|c| !c.is_alphanumeric()) => {}
            _ => out.push(first),
        }
        rest = chars.as_str();
    }
    out.push_str(rest);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A `speak` summary as one line of prose: agents sometimes add Markdown
/// or line breaks despite being asked not to.
pub(crate) fn inline_prose_lines(text: &str) -> String {
    text.lines()
        .map(|line| inline_prose(strip_line_marker(line.trim())))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The closing paragraph the Agent wrote to be read aloud, as prose, when
/// the reply has one. The last one wins: an Agent quoting the instruction
/// earlier in its reply should not be read instead of its own.
pub(crate) fn spoken_paragraph(reply: &str) -> Option<String> {
    let marker = diri_proto::voice::SPOKEN_MARKER;
    let lines = reply.lines().collect::<Vec<_>>();
    let start = lines.iter().rposition(|line| {
        inline_prose(strip_line_marker(line.trim()))
            .get(..marker.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(marker))
    })?;
    let paragraph = lines[start..]
        .iter()
        .map(|line| line.trim())
        .take_while(|line| !line.is_empty())
        .map(|line| inline_prose(strip_line_marker(line)))
        .collect::<Vec<_>>()
        .join(" ");
    let text = paragraph[marker.len()..].trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// Glyphs agents' terminal interfaces draw around text: message bullets,
/// prompts, box sides and tree branches.
const TERMINAL_DECORATION: &[char] = &[
    '⏺', '●', '•', '◆', '◇', '✦', '✧', '◉', '○', '❯', '›', '»', '>', '│', '┃', '║', '|', '⎿',
    '└', '├', '╭', '╰', '╮', '╯', '▌', '▐', '*', '-',
];

/// A terminal line without the interface drawn around its text.
fn terminal_text(line: &str) -> &str {
    line.trim()
        .trim_start_matches(|c: char| TERMINAL_DECORATION.contains(&c) || c.is_whitespace())
        .trim_end_matches(|c: char| matches!(c, '│' | '┃' | '║') || c.is_whitespace())
}

/// Status bars, rules and key hints: interface, not reply.
fn is_terminal_chrome(line: &str) -> bool {
    let text = terminal_text(line);
    let lower = text.to_lowercase();
    // Agents' spinners and turn timers: "✻ Worked for 10s · done 12:50".
    text.is_empty()
        || text.starts_with(['✻', '✳', '✶', '✢', '✽', '⠋', '⠙', '⠹', '⠸'])
        || text
            .chars()
            .all(|c| "─━═-╌┄·╭╮╰╯┌┐└┘ ".contains(c))
        || [
            "esc to",
            "shift+tab",
            "? for shortcuts",
            "ctrl+",
            "tokens",
            "auto mode",
            "accept edits",
            "worked for",
            "show details",
        ]
            .iter()
            .any(|hint| lower.contains(hint))
}

/// The last `Spoken:` paragraph on an agent's terminal: the marker line and
/// the wrapped lines under it, up to a blank line or interface chrome.
/// Works for any agent, since it reads what the agent printed.
pub(crate) fn terminal_spoken_paragraph(lines: &[String]) -> Option<String> {
    let marker = diri_proto::voice::SPOKEN_MARKER;
    let starts_with_marker = |line: &str| {
        terminal_text(line)
            .get(..marker.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(marker))
    };
    let start = lines.iter().rposition(|line| starts_with_marker(line))?;
    let paragraph = lines[start..]
        .iter()
        .enumerate()
        .take_while(|(index, line)| *index == 0 || !is_terminal_chrome(line))
        .take_while(|(index, line)| *index == 0 || !starts_with_marker(line))
        .map(|(_, line)| inline_prose(terminal_text(line)))
        .collect::<Vec<_>>()
        .join(" ");
    let text = paragraph.get(marker.len()..)?.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// What an agent printed since the user's last prompt, without interface
/// chrome: for the on-device digest when there is no transcript to read.
/// The last prompt line on screen is the input box (empty, or showing a
/// placeholder like Codex's "Ask Codex to do anything"); the reply is what
/// sits between the user's message, the prompt line before it, and that box.
pub(crate) fn terminal_reply(lines: &[String]) -> Option<String> {
    let prompt = |line: &str| {
        let inner = line
            .trim_start()
            .trim_start_matches(|c: char| matches!(c, '│' | '┃' | '║') || c.is_whitespace());
        ['❯', '›', '>'].iter().any(|glyph| inner.starts_with(*glyph))
    };
    let prompts: Vec<usize> = (0..lines.len()).filter(|&index| prompt(&lines[index])).collect();
    let (start, end) = match prompts.as_slice() {
        [.., message, input] => (message + 1, *input),
        [message] => (message + 1, lines.len()),
        [] => (0, lines.len()),
    };
    let text = lines[start..end]
        .iter()
        .filter(|line| !is_terminal_chrome(line))
        .map(|line| terminal_text(line))
        .collect::<Vec<_>>()
        .join("\n");
    (!text.trim().is_empty()).then_some(text)
}

/// What the on-device model reads: the speakable reply, its middle elided
/// when it would not fit in one request.
pub(crate) fn digest_input(speakable: &str) -> String {
    if speakable.len() <= MAX_DIGEST_INPUT {
        return speakable.to_owned();
    }
    let head = floor_char_boundary(speakable, DIGEST_HEAD);
    let tail = ceil_char_boundary(
        speakable,
        speakable.len() - (MAX_DIGEST_INPUT - DIGEST_HEAD),
    );
    format!("{}\n…\n{}", &speakable[..head], &speakable[tail..])
}

/// The reading when the model cannot help: the opening sentences up to about
/// [`FALLBACK_WORDS`] words, then the agent's last question if it was cut.
pub(crate) fn fallback_cut(speakable: &str) -> String {
    let sentences = sentences(speakable);
    let mut spoken: Vec<&str> = Vec::new();
    let mut words = 0;
    for sentence in &sentences {
        let count = sentence.split_whitespace().count();
        if !spoken.is_empty() && words + count > FALLBACK_WORDS {
            break;
        }
        spoken.push(sentence);
        words += count;
    }
    if let Some(question) = sentences
        .iter()
        .rev()
        .find(|sentence| sentence.ends_with('?'))
        && !spoken.contains(question)
    {
        spoken.push(question);
    }
    let text = spoken.join(" ");
    let cut = text
        .split_whitespace()
        .take(FALLBACK_WORDS * 2)
        .collect::<Vec<_>>();
    cut.join(" ")
}

fn sentences(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((index, c)) = chars.next() {
        let ends = match c {
            '\n' => true,
            '.' | '!' | '?' | '…' | '。' | '！' | '？' => {
                chars.peek().is_none_or(|(_, next)| next.is_whitespace())
            }
            _ => false,
        };
        if ends {
            let end = index + c.len_utf8();
            let sentence = text[start..end].trim();
            if !sentence.is_empty() {
                out.push(sentence);
            }
            start = end;
        }
    }
    let last = text[start..].trim();
    if !last.is_empty() {
        out.push(last);
    }
    out
}

/// A session title short enough to introduce a reading.
pub(crate) fn spoken_title(title: &str) -> String {
    title
        .split_whitespace()
        .take(TITLE_WORDS)
        .collect::<Vec<_>>()
        .join(" ")
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(text: &str, mut index: usize) -> usize {
    while !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[cfg(target_os = "macos")]
pub(crate) use macos::{
    KokoroState, ensure_kokoro, install, installed_voices, kokoro_state, open_voice_downloads,
    preview_voice, wispr_flow_installed,
};

#[cfg(target_os = "macos")]
mod macos {
    use std::io::{Read as _, Write as _};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::sync::{Arc, OnceLock, RwLock, mpsc};
    use std::thread;
    use std::time::{Duration, Instant};

    use diri_proto::{AgentKind, SessionId, SessionRecord};
    use tokio::sync::broadcast::error::RecvError;

    use super::{
        TurnCompletion, digest_input, fallback_cut, inline_prose_lines, speakable, spoken_title,
    };
    use crate::i18n::tf;
    use crate::store::{DictationShortcut, SessionStore, StoreRuntime, TalkAnswer};

    const DIGEST_TIMEOUT: Duration = Duration::from_secs(20);
    const TERMINAL_READ_TIMEOUT: Duration = Duration::from_secs(3);
    /// A finished reply and its closing paragraph fit well inside this.
    const TERMINAL_READ_LINES: usize = 300;
    /// Claude Code reports a finished turn a few milliseconds before its
    /// final message reaches the transcript. Read once the file has been
    /// quiet this long, or after `SETTLE_LIMIT` whatever its state.
    const SETTLE_QUIET: Duration = Duration::from_millis(400);
    const SETTLE_LIMIT: Duration = Duration::from_secs(3);
    const SPEECH_POLL: Duration = Duration::from_millis(100);
    /// The helper's "on-device model unavailable" exit.
    const EXIT_UNAVAILABLE: i32 = 3;
    const WISPR_SCHEME: &str = "wispr-flow://";
    const WISPR_START: &str = "wispr-flow://start-hands-free";
    /// Ends hands-free dictation; Wispr Flow then pastes what it heard.
    const WISPR_STOP: &str = "wispr-flow://stop-hands-free";
    /// A pause this long after speaking also ends the answer, unsent.
    const ANSWER_SILENCE: &str = "8";
    /// The listener gives up a little before the answer wait does.
    const LISTEN_LIMIT: &str = "110";
    /// How long a dictation app gets to type its text after being stopped.
    const TYPED_TEXT_SETTLE: Duration = Duration::from_millis(1500);
    const LISTENING_SOUND: &str = "/System/Library/Sounds/Tink.aiff";
    const SENT_SOUND: &str = "/System/Library/Sounds/Pop.aiff";
    /// Lets the window show the agent that just spoke before dictation
    /// starts, so Wispr Flow pastes the answer into its terminal.
    const FOCUS_SETTLE: Duration = Duration::from_millis(300);
    /// How long the next agent waits for the user to answer this one.
    const ANSWER_LIMIT: Duration = Duration::from_secs(120);
    const ANSWER_POLL: Duration = Duration::from_millis(250);

    struct Utterance {
        session_id: SessionId,
        turn: u64,
        text: String,
    }

    /// Starts the reader and speaker threads. Both sleep on channels; nothing
    /// runs while no turn finishes.
    pub(crate) fn install(runtime: Arc<StoreRuntime>, tokio: tokio::runtime::Handle) {
        let store = Arc::clone(&runtime.store);
        ensure_kokoro(&store.read().expect("store").preferences().talk_voice);
        let mut completions = store.read().expect("store").turn_completions();
        let (speech_tx, speech_rx) = mpsc::channel::<Utterance>();
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let reader_store = Arc::clone(&store);
        let reader_runtime = Arc::clone(&runtime);
        let speaker_tokio = tokio.clone();
        let spawned = thread::Builder::new()
            .name("diri-voice-reader".into())
            .spawn(move || {
                loop {
                    let completion = match completions.blocking_recv() {
                        Ok(completion) => completion,
                        Err(RecvError::Lagged(_)) => continue,
                        Err(RecvError::Closed) => break,
                    };
                    let Some(record) = current_record(&reader_store, &completion) else {
                        continue;
                    };
                    let spoken =
                        || spoken_summary(&reader_runtime, &tokio, &record.id, completion.turn);
                    let terminal = || terminal_lines(&reader_runtime, &tokio, &record.id);
                    if let Some(text) = reading(&home, &record, spoken, terminal)
                        && speech_tx
                            .send(Utterance {
                                session_id: completion.session_id,
                                turn: completion.turn,
                                text,
                            })
                            .is_err()
                    {
                        break;
                    }
                }
            });
        if spawned.is_err() {
            return;
        }
        let _ = thread::Builder::new()
            .name("diri-voice-speaker".into())
            .spawn(move || {
                while let Ok(utterance) = speech_rx.recv() {
                    speak(&runtime, &speaker_tokio, &utterance);
                }
            });
    }

    /// The session's record while the setting is on and the turn is still its
    /// latest. A reply the user already answered is not worth reading.
    fn current_record(
        store: &RwLock<SessionStore>,
        completion: &TurnCompletion,
    ) -> Option<Arc<SessionRecord>> {
        let store = store.read().expect("store");
        if !speaks(&store, &completion.session_id) {
            return None;
        }
        let record = store.sessions().get(&completion.session_id)?;
        is_latest_turn(record, completion.turn).then(|| Arc::clone(record))
    }

    fn speaks(store: &SessionStore, session: &SessionId) -> bool {
        store.reads_aloud(session)
    }

    fn is_latest_turn(record: &SessionRecord, turn: u64) -> bool {
        record
            .attention_state
            .as_ref()
            .is_some_and(|state| state.turn == turn && !state.working)
    }

    /// What the agent gave Diri's `speak` tool during `turn`, if anything.
    fn spoken_summary(
        runtime: &StoreRuntime,
        tokio: &tokio::runtime::Handle,
        session: &SessionId,
        turn: u64,
    ) -> Option<String> {
        let params = diri_proto::voice::SpokenParams {
            session_id: session.0.clone(),
            turn,
        };
        let value = tokio
            .block_on(runtime.client().request(
                diri_proto::Method::VOICE_SPOKEN,
                Some(&params),
                Some(TERMINAL_READ_TIMEOUT),
            ))
            .ok()?;
        serde_json::from_value::<diri_proto::voice::SpokenResult>(value)
            .ok()?
            .text
    }

    /// The last lines of a session's terminal, local or remote, from the
    /// Engine. Any agent's reply is there, whatever its transcript format.
    fn terminal_lines(
        runtime: &StoreRuntime,
        tokio: &tokio::runtime::Handle,
        session: &SessionId,
    ) -> Option<Vec<String>> {
        let params = serde_json::json!({ "sessionID": session });
        let value = tokio
            .block_on(runtime.client().request(
                diri_proto::Method::SESSION_READ_SCROLLBACK,
                Some(&params),
                Some(TERMINAL_READ_TIMEOUT),
            ))
            .ok()?;
        let mut lines = serde_json::from_value::<diri_proto::ReadScrollbackResult>(value)
            .ok()?
            .lines;
        let keep = lines.len().saturating_sub(TERMINAL_READ_LINES);
        lines.drain(..keep);
        Some(lines)
    }

    /// What to say for one finished turn, or `None` to stay quiet. In
    /// order: what the agent gave Diri's `speak` tool; its `Spoken:`
    /// paragraph, from its transcript (Claude Code, Codex) or else its
    /// terminal (any agent, remote ones too); the on-device digest of what
    /// it wrote.
    fn reading(
        home: &Path,
        record: &SessionRecord,
        spoken: impl FnOnce() -> Option<String>,
        terminal: impl FnOnce() -> Option<Vec<String>>,
    ) -> Option<String> {
        let title = spoken_title(&record.title);
        if let Some(text) = spoken().map(|text| inline_prose_lines(&text))
            && !text.is_empty()
        {
            return Some(tf("voice.reading", &[("title", &title), ("text", &text)]));
        }
        settle(record);
        let messages = turn_messages(home, record);
        let spoken_reading =
            |spoken: String| Some(tf("voice.reading", &[("title", &title), ("text", &spoken)]));
        if let Some(spoken) = messages
            .as_ref()
            .and_then(|messages| messages.last())
            .and_then(|last| super::spoken_paragraph(last))
        {
            return spoken_reading(spoken);
        }
        let screen = if messages.is_none() { terminal() } else { None };
        if let Some(spoken) = screen.as_deref().and_then(super::terminal_spoken_paragraph) {
            return spoken_reading(spoken);
        }
        let reply = messages
            .map(|messages| messages.join("\n\n"))
            .or_else(|| screen.as_deref().and_then(super::terminal_reply));
        let Some(reply) = reply else {
            // Nothing readable at all: still say which agent finished.
            return Some(tf("voice.finished", &[("title", &title)]));
        };
        let prose = speakable(&reply);
        if prose.is_empty() {
            return None;
        }
        let body = match digest(&digest_input(&prose)) {
            Digest::Spoken(text) => text,
            Digest::Quiet => return None,
            Digest::Unavailable => fallback_cut(&prose),
        };
        (!body.is_empty()).then(|| tf("voice.reading", &[("title", &title), ("text", &body)]))
    }

    /// Waits until the transcript stops growing, so the turn's last message
    /// is on disk before it is read.
    fn settle(record: &SessionRecord) {
        let Some(path) = record.transcript_path.as_deref() else {
            return;
        };
        let version = || {
            std::fs::metadata(path)
                .ok()
                .map(|meta| (meta.len(), meta.modified().ok()))
        };
        let deadline = Instant::now() + SETTLE_LIMIT;
        let mut seen = version();
        while Instant::now() < deadline {
            thread::sleep(SETTLE_QUIET);
            let now = version();
            if now == seen {
                return;
            }
            seen = now;
        }
    }

    /// Everything the agent wrote since the user's last message: its
    /// narration along the way and the final reply, in order.
    fn turn_messages(home: &Path, record: &SessionRecord) -> Option<Vec<String>> {
        if record.host.is_some() {
            return None;
        }
        let kind = record.effective_kind();
        if !matches!(kind.id(), AgentKind::CLAUDE_CODE_ID | AgentKind::CODEX_ID) {
            return None;
        }
        let snapshot = crate::transcript::load(
            home,
            Path::new(record.transcript_path.as_deref()?),
            kind,
            record.agent_session_id.as_deref()?,
            &record.cwd,
            None,
        )
        .ok()??;
        let turns = snapshot.document.turns;
        let start = turns
            .iter()
            .rposition(|turn| turn.role == "You")
            .map_or(0, |index| index + 1);
        let messages = turns
            .into_iter()
            .skip(start)
            .map(|turn| turn.text)
            .collect::<Vec<_>>();
        (!messages.is_empty()).then_some(messages)
    }

    enum Digest {
        Spoken(String),
        Quiet,
        Unavailable,
    }

    fn digest(input: &str) -> Digest {
        let Some(helper) = helper_path() else {
            return Digest::Unavailable;
        };
        let Ok(mut child) = Command::new(helper)
            .arg("digest")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        else {
            return Digest::Unavailable;
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(input.as_bytes());
        }
        let deadline = Instant::now() + DIGEST_TIMEOUT;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Digest::Unavailable;
                }
            }
        };
        if status.code() == Some(EXIT_UNAVAILABLE) || !status.success() {
            return Digest::Unavailable;
        }
        let mut text = String::new();
        if let Some(mut stdout) = child.stdout.take() {
            let _ = stdout.read_to_string(&mut text);
        }
        match text.trim() {
            "" => Digest::Quiet,
            text => Digest::Spoken(text.to_owned()),
        }
    }

    /// Speaks one utterance, stopping as soon as it stops being wanted. When
    /// it ran to the end while the user is in Diri, brings that agent's
    /// session forward and hands the microphone to Wispr Flow, then waits
    /// for the answer before the next agent speaks: a conversation with each
    /// agent in turn.
    fn speak(runtime: &StoreRuntime, tokio: &tokio::runtime::Handle, utterance: &Utterance) {
        let store = &runtime.store;
        let wanted = || {
            let store = store.read().expect("store");
            speaks(&store, &utterance.session_id)
                && store
                    .sessions()
                    .get(&utterance.session_id)
                    .is_some_and(|record| is_latest_turn(record, utterance.turn))
        };
        if !wanted() {
            return;
        }
        let (voice, command) = {
            let store = store.read().expect("store");
            let prefs = store.preferences();
            (prefs.talk_voice.clone(), prefs.talk_voice_command.clone())
        };
        let Ok(mut child) = speech_command(&voice, &command)
            .stdin(Stdio::piped())
            .spawn()
        else {
            return;
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(utterance.text.as_bytes());
        }
        if !wait_while(&mut child, wanted) {
            return;
        }
        answer(runtime, tokio, utterance);
    }

    /// After a reading, takes the user's spoken answer for that agent with
    /// whichever listener they chose.
    fn answer(runtime: &StoreRuntime, tokio: &tokio::runtime::Handle, utterance: &Utterance) {
        let store = &runtime.store;
        let (on, mut with, shortcut, active, selected, language) = {
            let store = store.read().expect("store");
            let prefs = store.preferences();
            (
                prefs.talk_dictation,
                prefs.talk_answer,
                prefs.talk_answer_shortcut.clone(),
                store.app_is_active(),
                store.selected_session_id() == Some(&utterance.session_id),
                crate::i18n::resolve(prefs.ui_language).tag(),
            )
        };
        if !on {
            return;
        }
        if with == TalkAnswer::WisprFlow && !wispr_flow_installed() {
            with = TalkAnswer::Diri;
        }
        if with == TalkAnswer::Diri {
            answer_in_diri(runtime, tokio, utterance, language);
            return;
        }
        // Wispr Flow and dictation apps type into the focused window: only
        // while the user is in Diri, with that agent's session in front.
        let start_stop: Box<dyn Fn()> = match (with, shortcut) {
            (TalkAnswer::Shortcut, Some(shortcut)) => Box::new(move || press(&shortcut)),
            (TalkAnswer::Shortcut, None) => return,
            _ => Box::new(|| {
                let _ = Command::new("/usr/bin/open")
                    .arg("-g")
                    .arg(WISPR_START)
                    .status();
            }),
        };
        if !active {
            return;
        }
        if !selected {
            store
                .write()
                .expect("store")
                .select(utterance.session_id.clone());
            runtime.publish_local_change();
            thread::sleep(FOCUS_SETTLE);
        }
        start_stop();
        let stop: Box<dyn Fn()> = if with == TalkAnswer::Shortcut {
            start_stop
        } else {
            Box::new(|| {
                let _ = Command::new("/usr/bin/open")
                    .arg("-g")
                    .arg(WISPR_STOP)
                    .status();
            })
        };
        let mut listener = listen_for_answer_end("listen", language);
        let heard_phrase = await_answer(store, utterance, &mut listener, &*stop);
        if let Some(mut child) = listener {
            stop_process(&mut child);
        }
        // A dictation app that types its text (macOS Dictation) never pastes,
        // so the armed submit is still waiting: press Enter for it.
        if heard_phrase && with == TalkAnswer::Shortcut {
            thread::sleep(TYPED_TEXT_SETTLE);
            if super::take_armed_submit(&utterance.session_id) {
                let _ = tokio.block_on(runtime.client().send_text(
                    &utterance.session_id,
                    String::new(),
                    true,
                ));
            }
        }
    }

    /// Diri's own listener: transcribes the answer on this Mac and sends it
    /// to the agent itself, so it works with Diri in the background. A sound
    /// says it is listening; "send to agent" submits, a pause types it in.
    fn answer_in_diri(
        runtime: &StoreRuntime,
        tokio: &tokio::runtime::Handle,
        utterance: &Utterance,
        language: &str,
    ) {
        let Some(mut child) = listen_for_answer_end("transcribe", language) else {
            return;
        };
        cue(LISTENING_SOUND);
        let deadline = Instant::now() + ANSWER_LIMIT;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(_) => return,
            }
            let still_wanted = {
                let store = runtime.store.read().expect("store");
                speaks(&store, &utterance.session_id)
                    && store
                        .sessions()
                        .get(&utterance.session_id)
                        .is_some_and(|record| is_latest_turn(record, utterance.turn))
            };
            if !still_wanted || Instant::now() >= deadline {
                stop_process(&mut child);
                return;
            }
            thread::sleep(ANSWER_POLL);
        };
        if !status.success() {
            return;
        }
        let mut output = String::new();
        if let Some(mut stdout) = child.stdout.take() {
            let _ = stdout.read_to_string(&mut output);
        }
        let Some((message, submit)) = super::transcribed_answer(&output) else {
            return;
        };
        if tokio
            .block_on(runtime.client().send_text(&utterance.session_id, message, submit))
            .is_ok()
            && submit
        {
            cue(SENT_SOUND);
        }
    }

    /// Listens for the user saying a send phrase or going quiet: `listen`
    /// alongside a dictation app, `transcribe` to also take the words.
    fn listen_for_answer_end(mode: &str, language: &str) -> Option<Child> {
        let mut command = Command::new(helper_path()?);
        command.args([mode, "--language", language]);
        for phrase in super::SEND_PHRASES {
            command.args(["--phrase", phrase]);
        }
        command
            .args(["--silence", ANSWER_SILENCE, "--limit", LISTEN_LIMIT])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()
    }

    /// Presses the user's dictation shortcut (`diri-voice press`).
    fn press(shortcut: &DictationShortcut) {
        let Some(helper) = helper_path() else {
            return;
        };
        let _ = Command::new(helper)
            .args(["press", "--code", &shortcut.key_code.to_string()])
            .args(["--modifiers", &shortcut.modifiers().join(",")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    /// A short system sound, for listening that has no window of its own.
    fn cue(sound: &'static str) {
        thread::spawn(move || {
            let _ = Command::new("/usr/bin/afplay")
                .arg(sound)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        });
    }

    /// When the listener has finished, why: stops the dictation app so it
    /// pastes the answer, and arms the submit when the phrase was heard. A
    /// failed listener (no microphone access) leaves dictation to the user.
    fn answer_ended(listener: &mut Option<Child>, session: &SessionId, stop: &dyn Fn()) -> bool {
        let Some(child) = listener.as_mut() else {
            return false;
        };
        let Ok(Some(status)) = child.try_wait() else {
            return false;
        };
        let mut reason = String::new();
        if let Some(mut stdout) = child.stdout.take() {
            let _ = stdout.read_to_string(&mut reason);
        }
        *listener = None;
        let phrase = reason.trim() == "phrase";
        if phrase {
            super::arm_submit(session.clone());
        }
        if status.success() {
            stop();
        }
        phrase
    }

    /// Holds the next reading until the user sends this agent something,
    /// moves to another session or app, or lets it go unanswered. Whether
    /// the send phrase was heard.
    fn await_answer(
        store: &RwLock<SessionStore>,
        utterance: &Utterance,
        listener: &mut Option<Child>,
        stop: &dyn Fn(),
    ) -> bool {
        let deadline = Instant::now() + ANSWER_LIMIT;
        let mut heard_phrase = false;
        while Instant::now() < deadline {
            thread::sleep(ANSWER_POLL);
            heard_phrase |= answer_ended(listener, &utterance.session_id, stop);
            let store = store.read().expect("store");
            let waiting = speaks(&store, &utterance.session_id)
                && store.app_is_active()
                && store.selected_session_id() == Some(&utterance.session_id)
                && store
                    .sessions()
                    .get(&utterance.session_id)
                    .is_some_and(|record| is_latest_turn(record, utterance.turn));
            if !waiting || (heard_phrase && listener.is_none()) {
                return heard_phrase;
            }
        }
        heard_phrase
    }

    /// Waits for `child` to finish; stops it and returns false once `wanted`
    /// turns false.
    fn wait_while(child: &mut Child, wanted: impl Fn() -> bool) -> bool {
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return status.success(),
                Ok(None) if wanted() => thread::sleep(SPEECH_POLL),
                _ => {
                    stop_process(child);
                    return false;
                }
            }
        }
    }

    /// Ends a child and everything it started: a speech command such as
    /// `curl … | afplay` runs as its own process group.
    fn stop_process(child: &mut Child) {
        if let Ok(group) = i32::try_from(child.id()) {
            // SAFETY: signalling a process group we created.
            unsafe {
                libc::killpg(group, libc::SIGTERM);
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    }

    /// Speech for `text` in `voice`: a system voice identifier, a
    /// "kokoro:<name>" voice (the helper uses a system voice until Kokoro's
    /// files are downloaded, and for replies that are not English), or empty
    /// for the best installed voice for the text's language.
    fn speech_command(voice: &str, shell_command: &str) -> Command {
        use std::os::unix::process::CommandExt as _;
        if voice == super::COMMAND_VOICE && !shell_command.trim().is_empty() {
            let mut command = Command::new("/bin/sh");
            command
                .arg("-c")
                .arg(shell_command)
                .process_group(0)
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            return command;
        }
        let voice = if voice == super::COMMAND_VOICE { "" } else { voice };
        let mut command = match helper_path() {
            Some(helper) => {
                let mut command = Command::new(helper);
                command.arg("say").arg("--voice").arg(voice);
                if let Some(dir) = kokoro_dir() {
                    command.arg("--kokoro-dir").arg(dir);
                }
                command
            }
            // `say` reads stdin when given no text.
            None => Command::new("/usr/bin/say"),
        };
        command
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    /// `diri-voice` next to the Engine in the bundle, beside a loose
    /// executable, or where `scripts/build-voice-helper.sh` puts dev builds.
    fn helper_path() -> Option<&'static Path> {
        static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
        PATH.get_or_init(|| {
            let mut candidates = Vec::new();
            if let Some(path) = std::env::var_os("DIRI_VOICE_PATH") {
                candidates.push(PathBuf::from(path));
            }
            if let Ok(exe) = std::env::current_exe()
                && let Some(macos_dir) = exe.parent()
            {
                if let Some(contents) = macos_dir.parent() {
                    candidates.push(contents.join("Resources/bin/diri-voice"));
                }
                candidates.push(macos_dir.join("diri-voice"));
            }
            if let Ok(cwd) = std::env::current_dir() {
                candidates.push(cwd.join("target/diri-voice/diri-voice"));
            }
            candidates.into_iter().find(|path| path.is_file())
        })
        .as_deref()
    }

    /// Starts the user's speech command and stops it again, for tests.
    #[cfg(test)]
    pub(super) fn stop_speech_command_for_test(shell_command: &str) -> i32 {
        let mut child = speech_command(super::COMMAND_VOICE, shell_command)
            .stdin(Stdio::null())
            .spawn()
            .expect("spawn");
        let group = i32::try_from(child.id()).expect("pid");
        thread::sleep(Duration::from_millis(200));
        stop_process(&mut child);
        group
    }

    #[cfg(test)]
    pub(super) fn download_percent_for_test(line: &str) -> Option<u8> {
        download_percent(line)
    }

    #[cfg(test)]
    pub(super) fn turn_reply_for_test(home: &Path, record: &SessionRecord) -> Option<String> {
        turn_messages(home, record).map(|messages| messages.join("\n\n"))
    }

    /// Kokoro-82M, the open-weights voice (Apache-2.0), run by `diri-voice`
    /// with ONNX Runtime on Apple silicon. Its files are downloaded the first
    /// time a Kokoro voice is picked.
    const KOKORO_PREFIX: &str = "kokoro:";
    const KOKORO_VOICES: [(&str, &str, &str); 11] = [
        ("af_heart", "Heart", "English (United States)"),
        ("af_bella", "Bella", "English (United States)"),
        ("af_nicole", "Nicole", "English (United States)"),
        ("af_sarah", "Sarah", "English (United States)"),
        ("am_michael", "Michael", "English (United States)"),
        ("am_fenrir", "Fenrir", "English (United States)"),
        ("am_puck", "Puck", "English (United States)"),
        ("bf_emma", "Emma", "English (United Kingdom)"),
        ("bf_isabella", "Isabella", "English (United Kingdom)"),
        ("bm_george", "George", "English (United Kingdom)"),
        ("bm_fable", "Fable", "English (United Kingdom)"),
    ];

    /// Where Kokoro's model, voices and lexicons live.
    fn kokoro_dir() -> Option<PathBuf> {
        let home = std::env::var_os("HOME")?;
        Some(diri_proto::paths::DirijorPaths::app_support(home).join("voices/kokoro"))
    }

    /// Whether every file `voice` needs is on disk; `KokoroFiles` in the
    /// helper names the same files and verifies them as it downloads.
    fn kokoro_ready(voice: &str) -> bool {
        let Some(dir) = kokoro_dir() else {
            return false;
        };
        let accent = if voice.starts_with('b') { "gb" } else { "us" };
        [
            "model_quantized.onnx".to_owned(),
            format!("voices/{voice}.bin"),
            format!("lexicon/{accent}_gold.json"),
            format!("lexicon/{accent}_silver.json"),
        ]
        .iter()
        .all(|file| dir.join(file).is_file())
    }

    /// A Kokoro voice's files, for Settings.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum KokoroState {
        Ready,
        Missing,
        Downloading(u8),
        Failed,
    }

    static KOKORO_DOWNLOAD: std::sync::Mutex<Option<(String, KokoroState)>> =
        std::sync::Mutex::new(None);

    fn set_download(voice: &str, state: KokoroState) {
        *KOKORO_DOWNLOAD
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some((voice.to_owned(), state));
    }

    /// The state of a "kokoro:<name>" voice; `None` for system voices.
    pub(crate) fn kokoro_state(id: &str) -> Option<KokoroState> {
        let voice = id.strip_prefix(KOKORO_PREFIX)?;
        if kokoro_ready(voice) {
            return Some(KokoroState::Ready);
        }
        let download = KOKORO_DOWNLOAD
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        Some(match download.as_ref() {
            Some((current, state)) if current == voice => *state,
            _ => KokoroState::Missing,
        })
    }

    /// Starts downloading what a "kokoro:<name>" voice needs, unless it is
    /// there or already on its way.
    pub(crate) fn ensure_kokoro(id: &str) {
        let Some(voice) = id.strip_prefix(KOKORO_PREFIX) else {
            return;
        };
        if matches!(
            kokoro_state(id),
            Some(KokoroState::Ready | KokoroState::Downloading(_)) | None
        ) {
            return;
        }
        let (Some(helper), Some(dir)) = (helper_path(), kokoro_dir()) else {
            set_download(voice, KokoroState::Failed);
            return;
        };
        set_download(voice, KokoroState::Downloading(0));
        let voice = voice.to_owned();
        thread::spawn(move || {
            let child = Command::new(helper)
                .args(["kokoro-download", "--voice", &voice, "--dir"])
                .arg(&dir)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn();
            let Ok(mut child) = child else {
                set_download(&voice, KokoroState::Failed);
                return;
            };
            if let Some(stdout) = child.stdout.take() {
                use std::io::BufRead as _;
                for line in std::io::BufReader::new(stdout)
                    .lines()
                    .map_while(Result::ok)
                {
                    if let Some(percent) = download_percent(&line) {
                        set_download(&voice, KokoroState::Downloading(percent));
                    }
                }
            }
            let ok = child.wait().is_ok_and(|status| status.success());
            set_download(
                &voice,
                if ok && kokoro_ready(&voice) {
                    KokoroState::Ready
                } else {
                    KokoroState::Failed
                },
            );
        });
    }

    /// "progress <done> <total>" from `diri-voice kokoro-download`.
    fn download_percent(line: &str) -> Option<u8> {
        let mut parts = line.strip_prefix("progress ")?.split(' ');
        let done: u64 = parts.next()?.parse().ok()?;
        let total: u64 = parts.next()?.parse().ok()?;
        (total > 0).then(|| (done.min(total) * 100 / total) as u8)
    }

    /// The voices worth offering in Settings, best first. Runs the helper, so
    /// call it off the main thread. Empty without the helper.
    pub(crate) fn installed_voices(interface_language: &str) -> Vec<super::VoiceOption> {
        let mut voices: Vec<super::VoiceOption> = Vec::new();
        // ONNX Runtime ships for Apple silicon only.
        if cfg!(target_arch = "aarch64") {
            voices.extend(
                KOKORO_VOICES
                    .iter()
                    .map(|(id, name, language)| super::VoiceOption {
                        id: format!("{KOKORO_PREFIX}{id}"),
                        name: (*name).to_owned(),
                        language_name: (*language).to_owned(),
                        quality: 4,
                    }),
            );
        }
        let Some(helper) = helper_path() else {
            return voices;
        };
        let system: Vec<super::VoiceOption> = Command::new(helper)
            .args(["voices", "--language", interface_language])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| serde_json::from_slice(&output.stdout).ok())
            .unwrap_or_default();
        voices.extend(system);
        voices
    }

    /// Reads `text` in `voice` so a choice can be heard, cutting off the
    /// previous sample.
    pub(crate) fn preview_voice(voice: &str, shell_command: &str, text: &str) {
        static SAMPLE: std::sync::Mutex<Option<Child>> = std::sync::Mutex::new(None);
        let mut sample = SAMPLE.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(mut previous) = sample.take() {
            stop_process(&mut previous);
        }
        let Ok(mut child) = speech_command(voice, shell_command)
            .stdin(Stdio::piped())
            .spawn()
        else {
            return;
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        *sample = Some(child);
    }

    /// System Settings' Accessibility pane, where Spoken Content's Manage
    /// Voices downloads enhanced and premium voices.
    pub(crate) fn open_voice_downloads() {
        thread::spawn(|| {
            let _ = Command::new("/usr/bin/open")
                .arg("x-apple.systempreferences:com.apple.Accessibility-Settings.extension?SpokenContent")
                .status();
        });
    }

    /// Whether some app handles `wispr-flow://`. Checked once per launch.
    pub(crate) fn wispr_flow_installed() -> bool {
        static INSTALLED: OnceLock<bool> = OnceLock::new();
        *INSTALLED.get_or_init(|| {
            use objc2_app_kit::NSWorkspace;
            use objc2_foundation::{NSString, NSURL};
            let Some(url) = NSURL::URLWithString(&NSString::from_str(WISPR_SCHEME)) else {
                return false;
            };
            NSWorkspace::sharedWorkspace()
                .URLForApplicationToOpenURL(&url)
                .is_some()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::attention::{AttentionEvent, AttentionState};

    fn state(
        sequence: u64,
        turn: u64,
        working: bool,
        events: Vec<AttentionEvent>,
    ) -> AttentionState {
        AttentionState {
            version: 1,
            epoch: "e".into(),
            sequence,
            turn,
            working,
            observed_at: None,
            last_native_completion: None,
            active_tools: Default::default(),
            events,
            native_requests: Default::default(),
            native_completions: Default::default(),
        }
    }

    fn event(sequence: u64, turn: u64, kind: AttentionKind) -> AttentionEvent {
        AttentionEvent {
            sequence,
            turn,
            kind,
            occurred_at: diri_proto::DateMillis(0.0),
            resolved: false,
            blocking: true,
            detail: None,
        }
    }

    fn record(attention: Option<AttentionState>) -> SessionRecord {
        let mut record =
            crate::sidebar::SidebarPreviewFixture::make(crate::sidebar::PreviewScenario::Typical)
                .list
                .sessions
                .remove(0);
        record.attention_state = attention;
        record
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_reading_covers_the_agents_whole_turn() {
        let home = tempfile::tempdir().expect("home");
        let id = "0f3c1d2e-1111-2222-3333-444455556666";
        let dir = home.path().join(".claude/projects/-tmp");
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join(format!("{id}.jsonl"));
        let line = |kind: &str, text: &str| {
            serde_json::json!({"type": kind, "message": {"content": [{"type": "text", "text": text}]}})
                .to_string()
        };
        std::fs::write(
            &path,
            [
                line("user", "fix it"),
                line("assistant", "Looking."),
                line("assistant", "Fixed it."),
            ]
            .join("\n"),
        )
        .expect("transcript");
        let mut session = record(None);
        session.kind = diri_proto::AgentKind::CLAUDE_CODE;
        session.host = None;
        session.agent_session_id = Some(id.into());
        session.transcript_path = Some(path.to_string_lossy().into_owned());
        assert_eq!(
            macos::turn_reply_for_test(home.path(), &session).as_deref(),
            Some("Looking.\n\nFixed it.")
        );
        std::fs::write(
            &path,
            [line("assistant", "Fixed it."), line("user", "thanks")].join("\n"),
        )
        .expect("transcript");
        assert_eq!(macos::turn_reply_for_test(home.path(), &session), None);
        session.host = Some("box".into());
        assert_eq!(macos::turn_reply_for_test(home.path(), &session), None);
    }

    #[test]
    fn a_new_completion_is_announced_once() {
        let working = record(Some(state(4, 3, true, vec![])));
        let done = record(Some(state(
            5,
            3,
            false,
            vec![event(5, 3, AttentionKind::Completion)],
        )));
        assert_eq!(completed_turn(Some(&working), &done), Some(3));
        assert_eq!(completed_turn(Some(&done), &done), None);
    }

    #[test]
    fn hydration_requests_and_new_epochs_stay_quiet() {
        let done = record(Some(state(
            5,
            3,
            false,
            vec![event(5, 3, AttentionKind::Completion)],
        )));
        assert_eq!(completed_turn(None, &done), None);
        let before = record(Some(state(4, 3, true, vec![])));
        let asked = record(Some(state(
            5,
            3,
            true,
            vec![event(5, 3, AttentionKind::Request)],
        )));
        assert_eq!(completed_turn(Some(&before), &asked), None);
        let mut restarted = state(1, 1, false, vec![event(1, 1, AttentionKind::Completion)]);
        restarted.epoch = "other".into();
        assert_eq!(
            completed_turn(Some(&before), &record(Some(restarted))),
            None
        );
    }

    #[test]
    fn speakable_drops_code_tables_links_and_markers() {
        let reply = "## Summary\nI fixed the **login** loop in `guard.ts`.\n\n```ts\nlet a = 1;\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n- Changed [the guard](https://x.y/z)\n- See https://example.com now\n\n---\nShould I ship_it?";
        assert_eq!(
            speakable(reply),
            "Summary\nI fixed the login loop in guard.ts.\nChanged the guard\nSee now\nShould I ship_it?"
        );
    }

    #[test]
    fn the_agents_spoken_paragraph_is_read_as_written() {
        let reply = "Fixed the race in `voice.rs`.\n\n```rust\nlet a = 1;\n```\n\n**Spoken:** I fixed the race.\nThe reading now waits for the transcript.\n\nShould not be read.";
        assert_eq!(
            spoken_paragraph(reply).as_deref(),
            Some("I fixed the race. The reading now waits for the transcript.")
        );
        assert_eq!(
            spoken_paragraph("Add a paragraph that starts with \"Spoken:\".\n\nSpoken: Done.")
                .as_deref(),
            Some("Done.")
        );
        assert_eq!(spoken_paragraph("No marker here."), None);
        assert_eq!(spoken_paragraph("Spoken:"), None);
    }

    #[test]
    fn saying_send_to_agent_submits_the_rest() {
        assert_eq!(
            dictated_submission("Fix the tests, then commit. Send to agent."),
            Some("Fix the tests, then commit.")
        );
        assert_eq!(
            dictated_submission("ship it, send to agent"),
            Some("ship it")
        );
        assert_eq!(dictated_submission("Send to agent."), Some(""));
        assert_eq!(
            dictated_submission("Revisa los tests. Enviar al agente."),
            Some("Revisa los tests.")
        );
        assert_eq!(
            dictated_submission("运行测试。发送给智能体。"),
            Some("运行测试。")
        );
        assert_eq!(dictated_submission("Explain how send to agent works"), None);
        assert_eq!(dictated_submission("resend to agent"), None);
        // Dictation hears "sent", and people say "the agent".
        assert_eq!(
            dictated_submission("I quit it, sent to agent."),
            Some("I quit it")
        );
        assert_eq!(dictated_submission("Ship it. Send to the agent."), Some("Ship it."));
        assert_eq!(dictated_submission("It was sent to agents"), None);
    }

    #[test]
    fn a_heard_send_phrase_is_removed_even_when_cut_off() {
        assert_eq!(
            heard_submission("Run the tests. Send to agent."),
            "Run the tests."
        );
        assert_eq!(heard_submission("Run the tests, send to"), "Run the tests");
        assert_eq!(heard_submission("Run the tests. Send."), "Run the tests.");
        assert_eq!(heard_submission("Run the tests, sent to"), "Run the tests");
        assert_eq!(heard_submission("Run the tests."), "Run the tests.");
        assert_eq!(heard_submission("Run the resend"), "Run the resend");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn kokoro_download_progress_reads_as_a_percentage() {
        assert_eq!(
            macos::download_percent_for_test("progress 46180558 92361116"),
            Some(50)
        );
        assert_eq!(macos::download_percent_for_test("progress 5 0"), None);
        assert_eq!(macos::download_percent_for_test("ready"), None);
    }

    fn screen(text: &str) -> Vec<String> {
        text.lines().map(str::to_owned).collect()
    }

    #[test]
    fn any_agents_spoken_paragraph_is_read_from_its_terminal() {
        // Claude Code: bullets, two-space continuation, a status bar below.
        let claude = screen(
            "⏺ I fixed the race.\n\n  Spoken: I fixed the race in the voice reader and the\n  tests pass. Tell me if I should commit.\n\n✳ Thinking… (esc to interrupt)\n────────\n❯",
        );
        assert_eq!(
            terminal_spoken_paragraph(&claude).as_deref(),
            Some("I fixed the race in the voice reader and the tests pass. Tell me if I should commit.")
        );
        // A boxed interface (Gemini-style) and Markdown emphasis.
        let boxed = screen(
            "╭──────────────╮\n│ ✦ Done.      │\n│ **Spoken:** The build is green. │\n│ Nothing else to do. │\n╰──────────────╯",
        );
        assert_eq!(
            terminal_spoken_paragraph(&boxed).as_deref(),
            Some("The build is green. Nothing else to do.")
        );
        assert_eq!(terminal_spoken_paragraph(&screen("⏺ No marker here.")), None);
    }

    #[test]
    fn a_terminal_reply_starts_after_the_users_prompt() {
        let lines = screen(
            "❯ old question\n⏺ old answer\n› fix the login loop\n• Fixed the guard in two files.\n  Tests pass.\n\n  ⏵⏵ auto mode on (shift+tab to cycle)\n›",
        );
        assert_eq!(
            terminal_reply(&lines).as_deref(),
            Some("Fixed the guard in two files.\nTests pass.")
        );
    }

    #[test]
    fn agents_without_a_hook_are_asked_in_the_answer() {
        let claude = diri_proto::AgentKind::CLAUDE_CODE;
        let gemini = diri_proto::AgentKind::new("gemini");
        assert_eq!(with_spoken_request("Run it.".into(), Some(&claude)), "Run it.");
        let asked = with_spoken_request("Run it.".into(), Some(&gemini));
        assert!(asked.starts_with("Run it.\n\n("), "{asked}");
        assert!(asked.contains("Spoken:"), "{asked}");
        assert_eq!(with_spoken_request(String::new(), Some(&gemini)), "");
    }

    #[test]
    fn a_placeholder_in_the_input_box_is_not_the_users_message() {
        // Codex, as captured from a real session.
        let lines = screen(
            "› What is 23 times 4? Answer in one short sentence.\n\n• Called dirijor.speak\n  └ {\"spoken\":true}\n    + Show details\n\n• 23 times 4 is 92.\n\n  Worked for 15s • 13:29\n\n› Ask Codex to do anything\n\n  GPT-6-Astra high · /private/tmp/voice-test\n  ? for shortcuts",
        );
        let reply = terminal_reply(&lines).expect("reply");
        assert!(reply.ends_with("23 times 4 is 92."), "{reply}");
        assert!(!reply.contains("GPT-6"), "{reply}");
    }

    #[test]
    fn a_transcribed_answer_is_sent_on_the_phrase_and_typed_on_a_pause() {
        assert_eq!(
            transcribed_answer(r#"{"reason":"phrase","text":"Run the tests, send to agent"}"#),
            Some(("Run the tests".to_owned(), true))
        );
        assert_eq!(
            transcribed_answer(r#"{"reason":"silence","text":"Run the tests"}"#),
            Some(("Run the tests".to_owned(), false))
        );
        assert_eq!(transcribed_answer(r#"{"reason":"limit","text":""}"#), None);
        assert_eq!(
            transcribed_answer(r#"{"reason":"phrase","text":"Send to agent"}"#),
            None
        );
        assert_eq!(transcribed_answer("not json"), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn stopping_a_speech_command_stops_its_whole_pipeline() {
        let group = macos::stop_speech_command_for_test("sleep 30 | sleep 30");
        std::thread::sleep(std::time::Duration::from_millis(200));
        // SAFETY: signal 0 only asks whether the group still has members.
        let alive = unsafe { libc::killpg(group, 0) } == 0;
        assert!(!alive, "a process of the speech command outlived the stop");
    }

    #[test]
    fn a_shortcut_names_its_modifiers() {
        let shortcut = crate::store::DictationShortcut {
            label: "cmd-alt-d".into(),
            key_code: 2,
        };
        assert_eq!(shortcut.modifiers(), vec!["cmd", "alt"]);
        let bare = crate::store::DictationShortcut {
            label: "f5".into(),
            key_code: 96,
        };
        assert!(bare.modifiers().is_empty());
    }

    #[test]
    fn a_saved_voice_is_named_before_the_list_loads() {
        assert_eq!(
            voice_name_from_id("com.apple.voice.premium.en-US.Zoe"),
            "Zoe"
        );
        assert_eq!(voice_name_from_id("Zoe"), "Zoe");
        assert_eq!(voice_name_from_id("kokoro:af_heart"), "Heart");
    }

    #[test]
    fn fallback_keeps_the_opening_and_the_last_question() {
        let filler =
            "This sentence only adds detail that nobody needs to hear right now. ".repeat(6);
        let text = format!("I fixed the loop. {filler}Should I also redirect public pages?");
        let cut = fallback_cut(&text);
        assert!(cut.starts_with("I fixed the loop."), "{cut}");
        assert!(
            cut.ends_with("Should I also redirect public pages?"),
            "{cut}"
        );
        assert!(
            cut.split_whitespace().count() <= FALLBACK_WORDS + 12,
            "{cut}"
        );
    }

    #[test]
    fn long_replies_keep_both_ends_on_char_boundaries() {
        let text = format!("Început {} sfârșit?", "ă".repeat(MAX_DIGEST_INPUT));
        let input = digest_input(&text);
        assert!(input.starts_with("Început"));
        assert!(input.ends_with("sfârșit?"));
        assert!(input.len() <= MAX_DIGEST_INPUT + 8);
    }
}


