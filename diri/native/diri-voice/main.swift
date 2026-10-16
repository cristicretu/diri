// diri-voice: the on-device half of "Talk with your agent".
//
//   diri-voice digest   reads an agent's final reply on stdin and prints what is
//                       worth hearing, or nothing when it should stay quiet.
//                       Exit 3 means the on-device model is unavailable; the
//                       caller then falls back to its own plain-text cut.
//   diri-voice say [--voice ID] [--kokoro-dir DIR]
//                       a "kokoro:<name>" voice speaks with Kokoro from DIR when
//                       its files are there and the text is English.
//   diri-voice kokoro-download --dir DIR --voice NAME
//                       fetches Kokoro's model, voice and lexicons (Kokoro.swift).
//   diri-voice phonemes [--british] --lexicon DIR
//                       prints Kokoro phonemes for stdin (EnglishG2P.swift).
//   diri-voice say [--voice ID]
//                       speaks stdin with the chosen voice when it speaks the
//                       text's language, otherwise the best installed voice for
//                       that language. The caller stops speech by terminating
//                       the process.
//   diri-voice listen [--phrase TEXT]... [--language CODE] [--silence SECONDS] [--limit SECONDS]
//                       listens alongside Wispr Flow's dictation and prints why
//                       the answer is over: "phrase" when the speech ends with a
//                       phrase, "silence" after that long a pause following
//                       speech, "limit" when time runs out. Recognition runs on
//                       this Mac only; without it, only the pause counts. Exit 4
//                       means no microphone access.
//   diri-voice language prints the dominant language of stdin ("en", "es").
//   diri-voice transcribe [the listen options]
//                       listens the same way and prints {"reason", "text"}: the
//                       answer itself, for Diri to send without another app.
//                       Exit 3 means on-device recognition is unavailable.
//   diri-voice press --code KEYCODE [--modifiers cmd,ctrl,alt,shift,fn]
//                       presses a shortcut, as a dictation app's start/stop
//                       key. Exit 5 means Accessibility permission is missing.
//   diri-voice voices [--language CODE]...
//                       prints the voices worth offering as JSON, best first:
//                       every enhanced or premium voice, and the basic voices
//                       of this Mac's main language and the given languages.
//
// Everything runs on this Mac: Apple's Foundation Models for the digest and
// AVSpeechSynthesizer for the voice. Nothing is sent anywhere.

import AVFoundation
import ApplicationServices
import Foundation
import NaturalLanguage
import Speech

#if canImport(FoundationModels)
    import FoundationModels
#endif

let exitUnavailable: Int32 = 3
let maxInputBytes = 64 * 1024

let instructions = """
    A developer's coding agent just finished a turn. The developer is away from \
    the screen and will hear a short summary read aloud instead of the agent's \
    messages. The messages are in order; the last one is the agent's final \
    reply. Say enough for the developer to understand what the agent did, what \
    it found or changed, and what is left, in plain spoken words, in the \
    language of the messages. Use only facts the messages state. Never read \
    out code, commands, file names, paths, numbers of lines or links.
    """

func readInput() -> String {
    let data = FileHandle.standardInput.readDataToEndOfFile().prefix(maxInputBytes)
    return String(decoding: data, as: UTF8.self)
}

func fail(_ message: String, code: Int32 = 1) -> Never {
    FileHandle.standardError.write(Data("diri-voice: \(message)\n".utf8))
    exit(code)
}

#if canImport(FoundationModels)
    @available(macOS 26.0, *)
    @Generable
    struct Spoken {
        @Guide(description: "What the agent did and what it found or changed, in its own words, as two to four short first-person sentences of at most 60 words in total. Lead with the result. Keep its hedges: if it did not check something, say so")
        var outcome: String
        @Guide(description: "What the agent says it could not do or what failed, in its own words, as one short first-person sentence; an empty string when nothing failed")
        var problem: String
        @Guide(description: "The question the agent asks the developer, as one short question; an empty string when it asks nothing")
        var question: String
        @Guide(description: "True only when the message is small talk with nothing the developer needs to hear")
        var nothingToSay: Bool
    }

    @available(macOS 26.0, *)
    func digest(_ reply: String) async -> String {
        let model = SystemLanguageModel.default
        guard case .available = model.availability else {
            fail("on-device model unavailable", code: exitUnavailable)
        }
        let session = LanguageModelSession(model: model, instructions: instructions)
        let options = GenerationOptions(sampling: .greedy, maximumResponseTokens: 300)
        do {
            let spoken = try await session.respond(
                to: "The agent's messages:\n\(reply)",
                generating: Spoken.self,
                options: options
            ).content
            if spoken.nothingToSay { return "" }
            // The small model sometimes says one thing in two fields; the
            // question wins over a problem that only restates it.
            // A question the agent never asked is the model's, not the agent's.
            let asked = reply.contains("?") || reply.contains("？")
            let question = asked ? spoken.question.trimmingCharacters(in: .whitespacesAndNewlines) : ""
            var parts: [String] = []
            for part in [spoken.outcome, spoken.problem] {
                let text = part.trimmingCharacters(in: .whitespacesAndNewlines)
                if !text.isEmpty && !(parts + [question]).contains(where: { overlaps($0, text) }) {
                    parts.append(text)
                }
            }
            if !question.isEmpty { parts.append(question) }
            return parts.joined(separator: " ")
        } catch {
            // Unsupported language, guardrails, an oversized reply: the caller's
            // plain cut is a better answer than silence.
            fail("digest failed: \(error)", code: exitUnavailable)
        }
    }
#endif

/// Whether two sentences share most of their words.
func overlaps(_ a: String, _ b: String) -> Bool {
    func words(_ text: String) -> Set<String> {
        Set(text.lowercased().split(whereSeparator: { !$0.isLetter && !$0.isNumber }).map(String.init))
    }
    let left = words(a)
    let right = words(b)
    guard !left.isEmpty, !right.isEmpty else { return false }
    return Double(left.intersection(right).count) / Double(min(left.count, right.count)) >= 0.6
}

func runDigest() async {
    let reply = readInput().trimmingCharacters(in: .whitespacesAndNewlines)
    guard !reply.isEmpty else { return }
    #if canImport(FoundationModels)
        if #available(macOS 26.0, *) {
            let text = await digest(reply)
            if !text.isEmpty { print(text) }
            return
        }
    #endif
    fail("on-device model needs macOS 26", code: exitUnavailable)
}

final class Speaker: NSObject, AVSpeechSynthesizerDelegate, @unchecked Sendable {
    private let synthesizer = AVSpeechSynthesizer()
    private var done: CheckedContinuation<Void, Never>?

    func speak(_ text: String, voice chosen: String?) async {
        let utterance = AVSpeechUtterance(string: text)
        let recognizer = NLLanguageRecognizer()
        recognizer.processString(text)
        let language = recognizer.dominantLanguage?.rawValue
        if let chosen, let voice = AVSpeechSynthesisVoice(identifier: chosen),
            language.map({ speaks(voice, $0) }) ?? true
        {
            utterance.voice = voice
        } else if let language, let voice = bestVoice(for: language) {
            utterance.voice = voice
        }
        synthesizer.delegate = self
        await withCheckedContinuation { continuation in
            done = continuation
            synthesizer.speak(utterance)
        }
    }

    func speechSynthesizer(_: AVSpeechSynthesizer, didFinish _: AVSpeechUtterance) {
        finish()
    }

    func speechSynthesizer(_: AVSpeechSynthesizer, didCancel _: AVSpeechUtterance) {
        finish()
    }

    private func finish() {
        done?.resume()
        done = nil
    }
}

/// Whether `voice` speaks `language`, a bare code like "en" or "zh-Hans".
func speaks(_ voice: AVSpeechSynthesisVoice, _ language: String) -> Bool {
    baseLanguage(voice.language) == baseLanguage(language)
}

func baseLanguage(_ code: String) -> String {
    String(code.split(separator: "-").first ?? Substring(code)).lowercased()
}

/// Voices a person would pick to hear replies: no novelty voices (Bells,
/// Bubbles) and no Personal Voice, which needs its own permission.
func offeredVoices() -> [AVSpeechSynthesisVoice] {
    AVSpeechSynthesisVoice.speechVoices().filter { voice in
        if #available(macOS 14.0, *) {
            return !voice.voiceTraits.contains(.isNoveltyVoice)
                && !voice.voiceTraits.contains(.isPersonalVoice)
        }
        return true
    }
}

/// The highest-quality installed voice for `language`. Among equals, the
/// system's own choice for the language, then this Mac's region.
func bestVoice(for language: String) -> AVSpeechSynthesisVoice? {
    let preferred = AVSpeechSynthesisVoice(language: language)?.identifier
    let region = Locale.current.region?.identifier ?? ""
    return offeredVoices()
        .filter { speaks($0, language) }
        .max { a, b in
            func rank(_ voice: AVSpeechSynthesisVoice) -> (Int, Int, Int) {
                (
                    voice.quality.rawValue,
                    voice.identifier == preferred ? 1 : 0,
                    voice.language.hasSuffix("-\(region)") ? 1 : 0
                )
            }
            return rank(a) < rank(b)
        }
}

func listVoices(extra: [String]) {
    let languages = Set((Locale.preferredLanguages.prefix(1) + extra).map(baseLanguage))
    // Language names in the first given language: Diri's interface language.
    let names = extra.first.map { Locale(identifier: $0) } ?? Locale.current
    let voices = offeredVoices()
        .filter { $0.quality != .default || languages.contains(baseLanguage($0.language)) }
        .sorted {
            ($0.quality.rawValue, $1.language, $1.name) > ($1.quality.rawValue, $0.language, $0.name)
        }
        .map { voice -> [String: Any] in
            [
                "id": voice.identifier,
                "name": voice.name,
                "language": voice.language,
                "languageName": names.localizedString(forIdentifier: voice.language) ?? voice.language,
                "quality": voice.quality.rawValue,
            ]
        }
    guard let data = try? JSONSerialization.data(withJSONObject: voices) else {
        fail("could not list voices")
    }
    FileHandle.standardOutput.write(data)
}

/// The values of every `--name VALUE` pair in `arguments`.
func values(_ name: String, in arguments: [String]) -> [String] {
    arguments.indices
        .filter { arguments[$0] == name && arguments.indices.contains($0 + 1) }
        .map { arguments[$0 + 1] }
}

/// Ends an answer when the user says a send phrase or stops talking.
final class AnswerListener: @unchecked Sendable {
    private let phrases: [String]
    private let silence: TimeInterval
    /// Report the words heard, not only why listening ended.
    private let transcribe: Bool
    private var heard = ""
    private let engine = AVAudioEngine()
    private let lock = NSLock()
    private var heardSpeech = false
    private var lastSpeech = Date()
    private var noiseFloor: Float?
    private var task: SFSpeechRecognitionTask?

    init(phrases: [String], silence: TimeInterval, transcribe: Bool) {
        self.phrases = phrases.map { $0.lowercased() }
        self.silence = silence
        self.transcribe = transcribe
    }

    func finish(_ reason: String) -> Never {
        if transcribe {
            let text = lock.withLock { heard }
            let data = try? JSONSerialization.data(withJSONObject: ["reason": reason, "text": text])
            FileHandle.standardOutput.write(data ?? Data())
        } else {
            print(reason)
        }
        fflush(stdout)
        exit(0)
    }

    func run(recognizer: SFSpeechRecognizer?, limit: TimeInterval) async {
        let request = SFSpeechAudioBufferRecognitionRequest()
        request.shouldReportPartialResults = true
        request.requiresOnDeviceRecognition = true
        request.contextualStrings = phrases
        // A transcribed answer is sent as typed, so it gets punctuation.
        if #available(macOS 13.0, *) { request.addsPunctuation = transcribe }
        let input = engine.inputNode
        input.installTap(onBus: 0, bufferSize: 1024, format: input.outputFormat(forBus: 0)) {
            [weak self] buffer, _ in
            if recognizer != nil { request.append(buffer) }
            self?.measure(buffer)
        }
        engine.prepare()
        do {
            try engine.start()
        } catch {
            fail("microphone unavailable: \(error)")
        }
        let debug = ProcessInfo.processInfo.environment["DIRI_VOICE_DEBUG"] != nil
        task = recognizer?.recognitionTask(with: request) { [weak self] result, error in
            if debug {
                FileHandle.standardError.write(Data("diri-voice: heard \(result?.bestTranscription.formattedString ?? "-") error \(error.map { "\($0)" } ?? "-")\n".utf8))
            }
            guard let self, let result else { return }
            let text = result.bestTranscription.formattedString
            self.lock.withLock { self.heard = text }
            let heard = text.lowercased()
                .trimmingCharacters(in: .whitespacesAndNewlines.union(.punctuationCharacters))
            if self.phrases.contains(where: { heard.hasSuffix($0) }) {
                self.finish("phrase")
            }
        }
        let deadline = Date().addingTimeInterval(limit)
        while Date() < deadline {
            try? await Task.sleep(nanoseconds: 200_000_000)
            if paused() { finish("silence") }
        }
        finish("limit")
    }

    private func paused() -> Bool {
        lock.withLock { heardSpeech && Date().timeIntervalSince(lastSpeech) > silence }
    }

    /// Speech is anything clearly louder than the room: the quietest level
    /// seen, rising slowly so a fan switching on is not taken for a voice.
    private func measure(_ buffer: AVAudioPCMBuffer) {
        guard let samples = buffer.floatChannelData?[0], buffer.frameLength > 0 else { return }
        var sum: Float = 0
        for index in 0..<Int(buffer.frameLength) { sum += samples[index] * samples[index] }
        let level = 10 * log10(max(sum / Float(buffer.frameLength), 1e-10))
        lock.lock()
        defer { lock.unlock() }
        let floor = min(noiseFloor ?? level, level) + 0.05
        noiseFloor = floor
        if level > max(floor + 15, -55) {
            heardSpeech = true
            lastSpeech = Date()
        }
    }
}

func microphoneAccess() async -> Bool {
    switch AVCaptureDevice.authorizationStatus(for: .audio) {
    case .authorized: return true
    case .notDetermined: return await AVCaptureDevice.requestAccess(for: .audio)
    default: return false
    }
}

/// An on-device recognizer for `language`, preferring this Mac's region.
/// None when recognition is not allowed or would need Apple's servers.
func onDeviceRecognizer(language: String) async -> SFSpeechRecognizer? {
    let status = await withCheckedContinuation { continuation in
        SFSpeechRecognizer.requestAuthorization { continuation.resume(returning: $0) }
    }
    guard status == .authorized else { return nil }
    let region = Locale.current.region?.identifier ?? ""
    let candidates = SFSpeechRecognizer.supportedLocales()
        .filter { baseLanguage($0.identifier) == baseLanguage(language) }
        .sorted { a, _ in a.region?.identifier == region }
    for locale in candidates + [Locale(identifier: "en-US")] {
        if let recognizer = SFSpeechRecognizer(locale: locale), recognizer.supportsOnDeviceRecognition {
            return recognizer
        }
    }
    return nil
}

func runListen(_ arguments: [String], transcribe: Bool = false) async {
    guard await microphoneAccess() else { fail("no microphone access", code: 4) }
    let phrases = values("--phrase", in: arguments)
    let language = values("--language", in: arguments).first ?? "en"
    let silence = values("--silence", in: arguments).first.flatMap(Double.init) ?? 8
    let limit = values("--limit", in: arguments).first.flatMap(Double.init) ?? 110
    let recognizer = phrases.isEmpty && !transcribe ? nil : await onDeviceRecognizer(language: language)
    if transcribe && recognizer == nil {
        fail("on-device speech recognition unavailable", code: exitUnavailable)
    }
    if recognizer == nil {
        FileHandle.standardError.write(Data("diri-voice: listening for pauses only\n".utf8))
    }
    let listener = AnswerListener(phrases: phrases, silence: silence, transcribe: transcribe)
    await listener.run(recognizer: recognizer, limit: limit)
}

/// Presses one shortcut: key down then up, with its modifiers. Posting
/// keyboard events needs Accessibility permission, which macOS asks for once.
func runPress(_ arguments: [String]) {
    guard let code = values("--code", in: arguments).first.flatMap(UInt16.init) else {
        fail("usage: diri-voice press --code KEYCODE [--modifiers cmd,ctrl,alt,shift,fn]", code: 64)
    }
    let prompt = kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String
    guard AXIsProcessTrustedWithOptions([prompt: true] as CFDictionary) else {
        fail("Accessibility permission is needed to press the dictation shortcut", code: 5)
    }
    let modifiers = Set((values("--modifiers", in: arguments).first ?? "").split(separator: ",").map(String.init))
    var flags = CGEventFlags()
    for (name, flag) in [("cmd", CGEventFlags.maskCommand), ("ctrl", .maskControl), ("alt", .maskAlternate),
                         ("shift", .maskShift), ("fn", .maskSecondaryFn)] where modifiers.contains(name) {
        flags.insert(flag)
    }
    let source = CGEventSource(stateID: .hidSystemState)
    for down in [true, false] {
        guard let event = CGEvent(keyboardEventSource: source, virtualKey: code, keyDown: down) else {
            fail("could not create the key event")
        }
        event.flags = flags
        event.post(tap: .cghidEventTap)
        usleep(20_000)
    }
}

@main
struct DiriVoice {
    static func main() async {
        switch CommandLine.arguments.dropFirst().first {
        case "digest":
            await runDigest()
        case "say":
            let arguments = Array(CommandLine.arguments.dropFirst(2))
            let voice = values("--voice", in: arguments).first.flatMap { $0.isEmpty ? nil : $0 }
            let text = readInput().trimmingCharacters(in: .whitespacesAndNewlines)
            guard !text.isEmpty else { return }
            if let kokoro = voice.flatMap({ $0.hasPrefix("kokoro:") ? String($0.dropFirst(7)) : nil }) {
                if let dir = values("--kokoro-dir", in: arguments).first,
                    await speakWithKokoro(text, voice: kokoro, dir: URL(fileURLWithPath: dir))
                {
                    return
                }
                // Not downloaded yet, another language, or an Intel Mac.
                await Speaker().speak(text, voice: nil)
                return
            }
            await Speaker().speak(text, voice: voice)
        case "kokoro-download":
            await runKokoroDownload(Array(CommandLine.arguments.dropFirst(2)))
        case "phonemes":
            runPhonemes(Array(CommandLine.arguments.dropFirst(2)))
        case "listen":
            await runListen(Array(CommandLine.arguments.dropFirst(2)))
        case "transcribe":
            await runListen(Array(CommandLine.arguments.dropFirst(2)), transcribe: true)
        case "press":
            runPress(Array(CommandLine.arguments.dropFirst(2)))
        case "language":
            let recognizer = NLLanguageRecognizer()
            recognizer.processString(readInput())
            print(recognizer.dominantLanguage?.rawValue ?? "")
        case "voices":
            let arguments = Array(CommandLine.arguments.dropFirst(2))
            let languages = arguments.indices
                .filter { arguments[$0] == "--language" && arguments.indices.contains($0 + 1) }
                .map { arguments[$0 + 1] }
            listVoices(extra: languages)
        default:
            fail("usage: diri-voice digest|say|listen|transcribe|press|voices|language|kokoro-download|phonemes", code: 64)
        }
    }
}
