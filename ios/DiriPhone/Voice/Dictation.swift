import AVFoundation
import Foundation
import Speech

/// On-device dictation for voice mode.
///
/// The microphone engine runs for as long as voice mode is open, not just
/// while dictating: an app with the `audio` background mode is only kept
/// alive while its audio session is doing something, and voice mode has to
/// hear `turn_finished` with the screen locked. Buffers are dropped unless a
/// recognizer is listening.
///
/// iOS 26's `SpeechAnalyzer` is used when its model is available for the
/// current locale; otherwise `SFSpeechRecognizer` with on-device recognition
/// required. Audio never leaves the phone.
@MainActor
final class Dictation {
    enum Failure: LocalizedError {
        case notAuthorized
        case unavailable

        var errorDescription: String? {
            switch self {
            case .notAuthorized: "Allow microphone and speech recognition for Diri in Settings."
            case .unavailable: "On-device dictation isn’t available for this language."
            }
        }
    }

    /// DEBUG launch hook: transcribe this file instead of the microphone, so a
    /// UI test can drive a full voice turn without a human talking.
    static let testAudioURL: URL? = {
        #if DEBUG
        if let path = ProcessInfo.processInfo.environment["DIRI_VOICE_TEST_AUDIO"], !path.isEmpty {
            return URL(fileURLWithPath: path)
        }
        #endif
        return nil
    }()

    /// DEBUG launch hook for the simulator, where no speech recogniser can
    /// start: "hear" this text instead, so a UI test still drives the
    /// silence → send leg. Recognition itself needs a real device.
    static let testTranscript: String? = {
        #if DEBUG
        if let text = ProcessInfo.processInfo.environment["DIRI_VOICE_TEST_TRANSCRIPT"], !text.isEmpty {
            return text
        }
        #endif
        return nil
    }()

    private let engine = AVAudioEngine()
    private let relay = TapRelay()
    /// Plays the cues through the same running engine, so they reach the
    /// headphones with the screen locked, where system sounds may not.
    private let cuePlayer = AVAudioPlayerNode()
    private let cueFormat = AVAudioFormat(standardFormatWithSampleRate: 44_100, channels: 1)!
    /// Loudness of each microphone buffer, 0…1, on the audio thread.
    var onLevel: (@Sendable (Float) -> Void)? {
        get { relay.onLevel }
        set { relay.onLevel = newValue }
    }
    private var engineRunning = false
    private var session: (any RecognitionSession)?

    // MARK: Permissions + engine

    static func requestPermissions(needsMicrophone: Bool) async -> Bool {
        let speech = await withCheckedContinuation { continuation in
            SFSpeechRecognizer.requestAuthorization { continuation.resume(returning: $0 == .authorized) }
        }
        guard speech else { return false }
        guard needsMicrophone else { return true }
        return await AVAudioApplication.requestRecordPermission()
    }

    func startEngine() throws {
        guard !engineRunning else { return }
        let input = engine.inputNode
        let format = input.outputFormat(forBus: 0)
        let relay = relay
        input.installTap(onBus: 0, bufferSize: 2048, format: format) { buffer, _ in
            relay.deliver(buffer)
        }
        engine.attach(cuePlayer)
        engine.connect(cuePlayer, to: engine.mainMixerNode, format: cueFormat)
        engine.prepare()
        try engine.start()
        engineRunning = true
    }

    /// Short tones that say what happened without a screen: listening began,
    /// the message was sent, or it was not.
    enum Cue { case listening, sent, failed }

    func play(_ cue: Cue) {
        guard engineRunning else { return }
        let samples = Self.samples(for: cue, sampleRate: cueFormat.sampleRate)
        guard let buffer = AVAudioPCMBuffer(pcmFormat: cueFormat, frameCapacity: AVAudioFrameCount(samples.count)),
              let channel = buffer.floatChannelData?[0] else { return }
        buffer.frameLength = AVAudioFrameCount(samples.count)
        samples.withUnsafeBufferPointer { channel.update(from: $0.baseAddress!, count: samples.count) }
        cuePlayer.scheduleBuffer(buffer, at: nil, options: .interrupts)
        if !cuePlayer.isPlaying { cuePlayer.play() }
    }

    /// Rising for listening, a bright double ding for sent, falling and low
    /// for failed. Each tone fades in and out over 5 ms so it never clicks.
    nonisolated static func samples(for cue: Cue, sampleRate: Double) -> [Float] {
        let tones: [(hertz: Double, seconds: Double)] = switch cue {
        case .listening: [(660, 0.07), (880, 0.09)]
        case .sent: [(880, 0.08), (0, 0.04), (1320, 0.12)]
        case .failed: [(330, 0.18), (220, 0.22)]
        }
        var samples: [Float] = []
        for tone in tones {
            let count = Int(tone.seconds * sampleRate)
            let fade = max(1, Int(0.005 * sampleRate))
            for index in 0..<count {
                guard tone.hertz > 0 else { samples.append(0); continue }
                let envelope = Float(min(1, Double(min(index, count - 1 - index)) / Double(fade)))
                let phase = 2 * Double.pi * tone.hertz * Double(index) / sampleRate
                samples.append(0.25 * envelope * Float(sin(phase)))
            }
        }
        return samples
    }

    func stopEngine() {
        cancel()
        guard engineRunning else { return }
        engine.inputNode.removeTap(onBus: 0)
        engine.stop()
        engineRunning = false
    }

    // MARK: Recognition

    /// Starts recognising. `onText` receives the whole utterance so far each
    /// time it changes; `onError` reports a recogniser that gave up, which
    /// would otherwise look like silence.
    func begin(onText: @escaping @MainActor (String) -> Void,
               onError: @escaping @MainActor (String) -> Void) async throws {
        cancel()
        if let text = Self.testTranscript {
            session = ScriptedRecognition(text: text, onText: onText)
            return
        }
        if let url = Self.testAudioURL {
            if #available(iOS 26, *) {
                do {
                    guard let analyzer = try await FileAnalyzerRecognition.make(
                        url: url, onText: onText, onError: onError
                    ) else { throw Failure.unavailable }
                    session = analyzer
                } catch {
                    onError(error.localizedDescription)
                }
            } else {
                session = try FileRecognition(url: url, onText: onText, onError: onError)
            }
            return
        }
        if #available(iOS 26, *), let analyzer = try? await AnalyzerRecognition.make(
            inputFormat: engine.inputNode.outputFormat(forBus: 0), onText: onText, onError: onError
        ) {
            session = analyzer
        } else {
            session = try LegacyRecognition(onText: onText, onError: onError)
        }
        let target = session
        relay.set { buffer in target?.append(buffer) }
    }

    /// Stops listening and returns the final text.
    func end() async -> String {
        relay.set(nil)
        guard let session else { return "" }
        self.session = nil
        return await session.finish()
    }

    func cancel() {
        relay.set(nil)
        session?.cancel()
        session = nil
    }
}

/// Hands microphone buffers from the realtime tap thread to whichever
/// recogniser is listening, without touching main-actor state.
private final class TapRelay: @unchecked Sendable {
    private let lock = NSLock()
    private var handler: (@Sendable (AVAudioPCMBuffer) -> Void)?

    func set(_ handler: (@Sendable (AVAudioPCMBuffer) -> Void)?) {
        lock.lock(); self.handler = handler; lock.unlock()
    }

    private var levelHandler: (@Sendable (Float) -> Void)?
    var onLevel: (@Sendable (Float) -> Void)? {
        get { lock.lock(); defer { lock.unlock() }; return levelHandler }
        set { lock.lock(); levelHandler = newValue; lock.unlock() }
    }

    func deliver(_ buffer: AVAudioPCMBuffer) {
        lock.lock(); let handler = handler; let onLevel = levelHandler; lock.unlock()
        handler?(buffer)
        if let onLevel { onLevel(Self.level(of: buffer)) }
    }

    /// RMS of the first channel mapped from roughly −50…−10 dBFS onto 0…1:
    /// speech at arm's length lands mid-range, room noise near zero.
    static func level(of buffer: AVAudioPCMBuffer) -> Float {
        guard let samples = buffer.floatChannelData?[0], buffer.frameLength > 0 else { return 0 }
        let count = Int(buffer.frameLength)
        var sum: Float = 0
        for index in 0..<count { sum += samples[index] * samples[index] }
        let rms = (sum / Float(count)).squareRoot()
        let decibels = 20 * log10(max(rms, 1e-7))
        return min(1, max(0, (decibels + 50) / 40))
    }
}

private protocol RecognitionSession: AnyObject, Sendable {
    func append(_ buffer: AVAudioPCMBuffer)
    @MainActor func finish() async -> String
    func cancel()
}

/// Latest text, written from recogniser callbacks and read on finish.
private final class TextBox: @unchecked Sendable {
    private let lock = NSLock()
    private var value = ""
    var text: String {
        get { lock.lock(); defer { lock.unlock() }; return value }
        set { lock.lock(); value = newValue; lock.unlock() }
    }
}

private final class LegacyRecognition: RecognitionSession, @unchecked Sendable {
    private let request = SFSpeechAudioBufferRecognitionRequest()
    private var task: SFSpeechRecognitionTask?
    private let box = TextBox()

    init(onText: @escaping @MainActor (String) -> Void, onError: @escaping @MainActor (String) -> Void) throws {
        guard let recognizer = SFSpeechRecognizer(locale: .current) ?? SFSpeechRecognizer(),
              recognizer.isAvailable, recognizer.supportsOnDeviceRecognition
        else { throw Dictation.Failure.unavailable }
        request.requiresOnDeviceRecognition = true
        request.shouldReportPartialResults = true
        request.taskHint = .dictation
        let box = box
        task = recognizer.recognitionTask(with: request) { result, error in
            if let error, result == nil {
                let message = error.localizedDescription
                Task { @MainActor in onError(message) }
                return
            }
            guard let result else { return }
            let text = result.bestTranscription.formattedString
            box.text = text
            Task { @MainActor in onText(text) }
        }
    }

    func append(_ buffer: AVAudioPCMBuffer) { request.append(buffer) }

    @MainActor func finish() async -> String {
        request.endAudio()
        // The final result trails endAudio slightly; partials are already the
        // whole utterance, so a short grace is enough.
        try? await Task.sleep(for: .milliseconds(300))
        task?.finish()
        return box.text
    }

    func cancel() { task?.cancel() }
}

/// The DEBUG file hook. Server recognition is allowed when the simulator has
/// no on-device model; this path never ships in Release.
private final class FileRecognition: RecognitionSession, @unchecked Sendable {
    private var task: SFSpeechRecognitionTask?
    private let box = TextBox()

    init(url: URL, onText: @escaping @MainActor (String) -> Void, onError: @escaping @MainActor (String) -> Void) throws {
        guard let recognizer = SFSpeechRecognizer(locale: Locale(identifier: "en-US")) ?? SFSpeechRecognizer()
        else { throw Dictation.Failure.unavailable }
        let request = SFSpeechURLRecognitionRequest(url: url)
        request.requiresOnDeviceRecognition = false
        request.shouldReportPartialResults = true
        let box = box
        task = recognizer.recognitionTask(with: request) { result, error in
            if let error, result == nil {
                let message = error.localizedDescription
                Task { @MainActor in onError(message) }
                return
            }
            guard let result else { return }
            let text = result.bestTranscription.formattedString
            box.text = text
            Task { @MainActor in onText(text) }
        }
    }

    func append(_ buffer: AVAudioPCMBuffer) {}

    @MainActor func finish() async -> String {
        task?.finish()
        return box.text
    }

    func cancel() { task?.cancel() }
}

@available(iOS 26, *)
private final class AnalyzerRecognition: RecognitionSession, @unchecked Sendable {
    private let analyzer: SpeechAnalyzer
    private let continuation: AsyncStream<AnalyzerInput>.Continuation
    private let converter: AVAudioConverter?
    private let format: AVAudioFormat
    private var results: Task<Void, Never>?
    private let box = TextBox()

    private init(analyzer: SpeechAnalyzer, format: AVAudioFormat, inputFormat: AVAudioFormat,
                 continuation: AsyncStream<AnalyzerInput>.Continuation) {
        self.analyzer = analyzer
        self.format = format
        self.continuation = continuation
        converter = inputFormat == format ? nil : AVAudioConverter(from: inputFormat, to: format)
    }

    @MainActor
    static func make(inputFormat: AVAudioFormat,
                     onText: @escaping @MainActor (String) -> Void,
                     onError: @escaping @MainActor (String) -> Void) async throws -> AnalyzerRecognition? {
        guard SpeechTranscriber.isAvailable,
              let locale = await SpeechTranscriber.supportedLocale(equivalentTo: .current)
        else { return nil }
        let transcriber = SpeechTranscriber(
            locale: locale, transcriptionOptions: [], reportingOptions: [.volatileResults], attributeOptions: []
        )
        if let install = try await AssetInventory.assetInstallationRequest(supporting: [transcriber]) {
            try await install.downloadAndInstall()
        }
        guard let format = await SpeechAnalyzer.bestAvailableAudioFormat(compatibleWith: [transcriber]) else {
            return nil
        }
        let analyzer = SpeechAnalyzer(modules: [transcriber])
        let (stream, continuation) = AsyncStream<AnalyzerInput>.makeStream()
        let session = AnalyzerRecognition(
            analyzer: analyzer, format: format, inputFormat: inputFormat, continuation: continuation
        )
        let box = session.box
        session.results = Task {
            var finalized = ""
            do {
                for try await result in transcriber.results {
                    let piece = String(result.text.characters)
                    let text: String
                    if result.isFinal {
                        finalized += piece
                        text = finalized
                    } else {
                        text = finalized + piece
                    }
                    box.text = text.trimmingCharacters(in: .whitespaces)
                    let snapshot = box.text
                    await MainActor.run { onText(snapshot) }
                }
            } catch is CancellationError {
            } catch {
                let message = error.localizedDescription
                await MainActor.run { onError(message) }
            }
        }
        try await analyzer.start(inputSequence: stream)
        return session
    }

    func append(_ buffer: AVAudioPCMBuffer) {
        guard let converter else {
            continuation.yield(AnalyzerInput(buffer: buffer))
            return
        }
        let ratio = format.sampleRate / buffer.format.sampleRate
        let capacity = AVAudioFrameCount(Double(buffer.frameLength) * ratio) + 1
        guard let output = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: capacity) else { return }
        var consumed = false
        var error: NSError?
        converter.convert(to: output, error: &error) { _, status in
            if consumed {
                status.pointee = .noDataNow
                return nil
            }
            consumed = true
            status.pointee = .haveData
            return buffer
        }
        if error == nil, output.frameLength > 0 {
            continuation.yield(AnalyzerInput(buffer: output))
        }
    }

    @MainActor func finish() async -> String {
        continuation.finish()
        try? await analyzer.finalizeAndFinishThroughEndOfInput()
        await results?.value
        return box.text
    }

    func cancel() {
        continuation.finish()
        results?.cancel()
        let analyzer = analyzer
        Task { await analyzer.cancelAndFinishNow() }
    }
}

/// The DEBUG file hook on the same SpeechAnalyzer path real devices use. The
/// simulator's legacy recogniser fails to initialise, so this is what a UI
/// test actually exercises.
@available(iOS 26, *)
private final class FileAnalyzerRecognition: RecognitionSession, @unchecked Sendable {
    private let box = TextBox()
    private var results: Task<Void, Never>?
    private var analyzer: SpeechAnalyzer?

    @MainActor
    static func make(url: URL,
                     onText: @escaping @MainActor (String) -> Void,
                     onError: @escaping @MainActor (String) -> Void) async throws -> FileAnalyzerRecognition? {
        guard SpeechTranscriber.isAvailable,
              let locale = await SpeechTranscriber.supportedLocale(equivalentTo: Locale(identifier: "en-US"))
        else { return nil }
        let transcriber = SpeechTranscriber(
            locale: locale, transcriptionOptions: [], reportingOptions: [], attributeOptions: []
        )
        if let install = try await AssetInventory.assetInstallationRequest(supporting: [transcriber]) {
            try await install.downloadAndInstall()
        }
        let file = try AVAudioFile(forReading: url)
        let analyzer = SpeechAnalyzer(modules: [transcriber])
        let session = FileAnalyzerRecognition()
        session.analyzer = analyzer
        let box = session.box
        session.results = Task {
            do {
                for try await result in transcriber.results where result.isFinal {
                    box.text = (box.text + String(result.text.characters)).trimmingCharacters(in: .whitespaces)
                    let snapshot = box.text
                    await MainActor.run { onText(snapshot) }
                }
            } catch is CancellationError {
            } catch {
                let message = error.localizedDescription
                await MainActor.run { onError(message) }
            }
        }
        try await analyzer.start(inputAudioFile: file, finishAfterFile: true)
        return session
    }

    func append(_ buffer: AVAudioPCMBuffer) {}

    @MainActor func finish() async -> String {
        await results?.value
        return box.text
    }

    func cancel() {
        results?.cancel()
        let analyzer = analyzer
        Task { await analyzer?.cancelAndFinishNow() }
    }
}

/// The DEBUG transcript hook: reports `text` as one utterance, then silence.
private final class ScriptedRecognition: RecognitionSession, @unchecked Sendable {
    private let text: String
    private let delivery: Task<Void, Never>

    init(text: String, onText: @escaping @MainActor (String) -> Void) {
        self.text = text
        delivery = Task {
            try? await Task.sleep(for: .milliseconds(500))
            await MainActor.run { onText(text) }
        }
    }

    func append(_ buffer: AVAudioPCMBuffer) {}
    @MainActor func finish() async -> String { text }
    func cancel() { delivery.cancel() }
}
