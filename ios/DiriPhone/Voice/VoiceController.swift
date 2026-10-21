import AVFoundation
import Foundation
import Observation

/// Gym mode: when the agent finishes a turn, read it aloud, then listen for
/// the reply and send it. Driven by the gateway's event stream — no polling.
@MainActor
@Observable
final class VoiceController {
    enum Phase: String {
        case starting, idle, speaking, listening, sending, waiting
    }

    private(set) var phase: Phase = .starting
    /// What the agent last said, as it is (or would be) read aloud.
    private(set) var agentText: String = ""
    /// The utterance most recently handed to the synthesizer. Exposed so a UI
    /// test can assert what was spoken.
    private(set) var lastSpoken: String = ""
    /// Live dictation.
    private(set) var heard: String = ""
    private(set) var lastSent: String = ""
    private(set) var error: String?
    /// Microphone loudness while listening, 0…1, smoothed for display.
    private(set) var level: Double = 0
    /// The word being read aloud, as a range of `agentText`.
    private(set) var spokenRange: NSRange?
    /// When the synthesizer last started a word: what the orb pulses on.
    private(set) var lastWordAt: Date = .distantPast
    var muted = false {
        didSet { if muted, phase == .speaking { synthesizer.stopSpeaking(at: .immediate); phase = .idle } }
    }

    /// Silence after speech that ends an utterance. The recogniser revises
    /// its text in bursts and people pause mid-thought, so 1.5 s cut sentences.
    static let silenceToSend: Duration = .milliseconds(2500)
    /// Mic level (0…1, about −36 dBFS) that counts as still talking, even
    /// while the recogniser has not caught up with new words.
    static let speakingLevel: Float = 0.35
    /// Loud audio may hold the send only this long past the last new words,
    /// so a noisy gym cannot keep the microphone open forever.
    static let loudHold: Duration = .seconds(3)
    /// Nothing heard at all for this long: stop listening rather than leave
    /// the microphone hot.
    static let nothingHeard: Duration = .seconds(10)

    private let sessionID: String
    private let client: DiriClient
    private let dictation = Dictation()
    private let synthesizer = AVSpeechSynthesizer()
    private let speechDelegate = SpeechDelegate()
    private var eventTask: Task<Void, Never>?
    private var silenceTask: Task<Void, Never>?
    private var lastHandledTurn: Int?
    private var lastHeardChange = ContinuousClock.now
    private var lastLoud = ContinuousClock.now
    private var previousReadAloud: Bool?
    private var autoListenAfterSpeech = false
    private var currentUtterance: ObjectIdentifier?
    /// Held so its identifier cannot be reused while callbacks are in flight.
    private var retainedUtterance: AVSpeechUtterance?
    private let testMode = Dictation.testAudioURL != nil || Dictation.testTranscript != nil

    init(sessionID: String, client: DiriClient) {
        self.sessionID = sessionID
        self.client = client
        synthesizer.delegate = speechDelegate
        speechDelegate.onFinish = { [weak self] id, finished in self?.speechEnded(id, finished: finished) }
        speechDelegate.onWord = { [weak self] id, range in
            guard let self, id == self.currentUtterance else { return }
            self.spokenRange = range
            self.lastWordAt = .now
        }
        dictation.onLevel = { [weak self] value in
            Task { @MainActor in
                guard let self, self.phase == .listening else { return }
                // Fast attack, slow release, so the orb breathes rather than
                // flickers with every buffer.
                let next = Double(value)
                self.level = next > self.level ? next : self.level * 0.8 + next * 0.2
                if value >= Self.speakingLevel { self.lastLoud = .now }
            }
        }
    }

    // MARK: Lifecycle

    func start() async {
        guard await Dictation.requestPermissions(needsMicrophone: !testMode) else {
            error = Dictation.Failure.notAuthorized.localizedDescription
            phase = .idle
            return
        }
        if !testMode {
            do {
                let audio = AVAudioSession.sharedInstance()
                try audio.setCategory(
                    .playAndRecord, mode: .default,
                    options: [.allowBluetoothHFP, .allowBluetoothA2DP, .defaultToSpeaker, .duckOthers]
                )
                try audio.setActive(true)
                try dictation.startEngine()
            } catch {
                self.error = "Couldn’t start audio: \(error.localizedDescription)"
            }
        }
        previousReadAloud = try? await client.readAloud()
        try? await client.setReadAloud(true)
        await loadLatest()
        phase = .idle
        startEvents()
    }

    func stop() {
        eventTask?.cancel()
        silenceTask?.cancel()
        autoListenAfterSpeech = false
        synthesizer.stopSpeaking(at: .immediate)
        dictation.stopEngine()
        if !testMode {
            try? AVAudioSession.sharedInstance().setActive(false, options: .notifyOthersOnDeactivation)
        }
        let client = client
        let restore = previousReadAloud ?? false
        Task { try? await client.setReadAloud(restore) }
    }

    private func loadLatest() async {
        let spoken = try? await client.spoken(sessionID: sessionID)
        let transcript = (try? await client.transcript(sessionID: sessionID, limit: 6)) ?? []
        if let text = VoiceText.utterance(spoken: spoken?.text, transcript: transcript) {
            agentText = text
        }
        lastHandledTurn = spoken?.turn ?? transcript.last(where: \.isAgent)?.turn
    }

    private func startEvents() {
        let client = client
        let sessionID = sessionID
        eventTask = Task { [weak self] in
            while !Task.isCancelled {
                for await event in await client.events() {
                    if event.name == "ready" {
                        // A turn that ended before this stream connected
                        // sent its event to nobody.
                        await self?.catchUp()
                        continue
                    }
                    guard let finished = DiriClient.TurnFinished(event), finished.sessionID == sessionID else {
                        continue
                    }
                    await self?.turnFinished(finished)
                }
                if Task.isCancelled { return }
                try? await Task.sleep(for: .seconds(2))
            }
        }
    }

    // MARK: Turns

    private func catchUp() async {
        let spoken = try? await client.spoken(sessionID: sessionID)
        let transcript = (try? await client.transcript(sessionID: sessionID, limit: 6)) ?? []
        guard let turn = spoken?.turn ?? transcript.last(where: \.isAgent)?.turn,
              turn != lastHandledTurn else { return }
        await turnFinished(DiriClient.TurnFinished(
            sessionID: sessionID, turn: turn, spoken: spoken?.turn == turn ? spoken?.text : nil
        ))
    }

    func turnFinished(_ event: DiriClient.TurnFinished) async {
        if let turn = event.turn {
            if turn == lastHandledTurn { return }
            lastHandledTurn = turn
        }
        var text = VoiceText.utterance(spoken: event.spoken, transcript: [], turn: event.turn)
        if text == nil {
            let transcript = (try? await client.transcript(sessionID: sessionID, limit: 6)) ?? []
            text = VoiceText.utterance(spoken: nil, transcript: transcript, turn: event.turn)
        }
        guard let text else { return }
        agentText = text
        // Never talk over the user: a turn that lands mid-dictation waits on
        // screen for "repeat".
        guard phase != .listening, phase != .sending else { return }
        if muted {
            phase = .idle
        } else {
            speak(text, thenListen: true)
        }
    }

    func repeatLast() {
        guard !agentText.isEmpty else { return }
        dictation.cancel()
        silenceTask?.cancel()
        speak(agentText, thenListen: !muted)
    }

    private func speak(_ text: String, thenListen: Bool) {
        synthesizer.stopSpeaking(at: .immediate)
        let utterance = AVSpeechUtterance(string: text)
        utterance.prefersAssistiveTechnologySettings = true
        if testMode { utterance.volume = 0 }
        autoListenAfterSpeech = thenListen
        lastSpoken = text
        spokenRange = nil
        currentUtterance = ObjectIdentifier(utterance)
        retainedUtterance = utterance
        phase = .speaking
        synthesizer.speak(utterance)
    }

    private func speechEnded(_ id: ObjectIdentifier, finished: Bool) {
        guard phase == .speaking, id == currentUtterance else { return }
        spokenRange = nil
        if finished, autoListenAfterSpeech {
            autoListenAfterSpeech = false
            Task { await listen() }
        } else {
            phase = .idle
        }
    }

    // MARK: Talk

    /// The one big button: barge in on speech, finish dictation early, or
    /// start talking.
    func primaryAction() {
        switch phase {
        case .speaking:
            autoListenAfterSpeech = false
            synthesizer.stopSpeaking(at: .immediate)
            Task { await listen() }
        case .listening:
            Task { await finishListening() }
        case .idle, .waiting:
            Task { await listen() }
        case .sending, .starting:
            break
        }
    }

    private func listen() async {
        error = nil
        heard = ""
        level = 0
        phase = .listening
        lastHeardChange = .now
        lastLoud = .now
        dictation.play(.listening)
        do {
            try await dictation.begin { [weak self] text in
                guard let self, self.phase == .listening, text != self.heard else { return }
                self.heard = text
                self.lastHeardChange = .now
            } onError: { [weak self] message in
                guard let self, self.phase == .listening, self.heard.isEmpty else { return }
                self.silenceTask?.cancel()
                self.dictation.cancel()
                self.error = "Couldn’t hear that: \(message)"
                self.phase = .idle
            }
        } catch {
            self.error = error.localizedDescription
            phase = .idle
            return
        }
        silenceTask?.cancel()
        let began = ContinuousClock.now
        silenceTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: .milliseconds(200))
                guard let self, self.phase == .listening else { return }
                let now = ContinuousClock.now
                if !self.heard.isEmpty,
                   Self.utteranceEnded(sinceWords: now - self.lastHeardChange, sinceLoud: now - self.lastLoud) {
                    // Outside this task: finishing cancels the silence task,
                    // and the send must not inherit that cancellation.
                    Task { await self.finishListening() }
                    return
                }
                if self.heard.isEmpty, ContinuousClock.now - began >= Self.nothingHeard {
                    self.dictation.cancel()
                    self.phase = .idle
                    return
                }
            }
        }
    }

    /// Whether a pause ends the utterance: no new words for `silenceToSend`,
    /// and no loud audio either, unless the words stopped `loudHold` ago.
    nonisolated static func utteranceEnded(sinceWords: Duration, sinceLoud: Duration) -> Bool {
        guard sinceWords >= silenceToSend else { return false }
        return sinceLoud >= silenceToSend || sinceWords >= silenceToSend + loudHold
    }

    private func finishListening() async {
        guard phase == .listening else { return }
        silenceTask?.cancel()
        phase = .sending
        level = 0
        let final = await dictation.end()
        let text = (final.isEmpty ? heard : final).trimmingCharacters(in: .whitespacesAndNewlines)
        heard = text
        guard !text.isEmpty else {
            phase = .idle
            return
        }
        do {
            try await client.send(text: text, to: sessionID)
            lastSent = text
            heard = ""
            phase = .waiting
            dictation.play(.sent)
        } catch {
            // Never retried automatically: the send may have arrived even
            // though the response did not.
            self.error = "Not confirmed: \(error.localizedDescription) Check the session before saying it again."
            phase = .idle
            dictation.play(.failed)
            // With the phone in a pocket the tone alone is ambiguous.
            if !muted { synthesizer.speak(AVSpeechUtterance(string: "Not sent. Check the session before saying it again.")) }
        }
    }
}

private final class SpeechDelegate: NSObject, AVSpeechSynthesizerDelegate, @unchecked Sendable {
    var onFinish: (@MainActor (ObjectIdentifier, Bool) -> Void)?
    var onWord: (@MainActor (ObjectIdentifier, NSRange) -> Void)?

    func speechSynthesizer(_ synthesizer: AVSpeechSynthesizer, willSpeakRangeOfSpeechString range: NSRange,
                           utterance: AVSpeechUtterance) {
        let onWord = onWord
        let id = ObjectIdentifier(utterance)
        Task { @MainActor in onWord?(id, range) }
    }

    func speechSynthesizer(_ synthesizer: AVSpeechSynthesizer, didFinish utterance: AVSpeechUtterance) {
        let onFinish = onFinish
        let id = ObjectIdentifier(utterance)
        Task { @MainActor in onFinish?(id, true) }
    }

    func speechSynthesizer(_ synthesizer: AVSpeechSynthesizer, didCancel utterance: AVSpeechUtterance) {
        let onFinish = onFinish
        let id = ObjectIdentifier(utterance)
        Task { @MainActor in onFinish?(id, false) }
    }
}
