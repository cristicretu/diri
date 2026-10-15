// Kokoro: the open-weights voice (hexgrad/Kokoro-82M, Apache-2.0) for "Talk
// with your agent", run on this Mac with ONNX Runtime. Apple silicon only:
// the x86_64 slice is built without KOKORO.
//
// Files live in one directory the app chooses (see KokoroFiles); `kokoro-download`
// fetches them, pinned by revision and SHA-256, the first time a Kokoro voice is
// picked. Speech is synthesized a sentence at a time and played while the next
// sentence is synthesized.

import AVFoundation
import CryptoKit
import NaturalLanguage
import Foundation

/// What Kokoro needs on disk, pinned to exact revisions and digests.
enum KokoroFiles {
    static let modelRevision = "1939ad2a8e416c0acfeecc08a694d14ef25f2231"
    static let lexiconRevision = "fba1236595f2d2bf21d414ba6e57d25256afada3"

    struct Asset {
        let name: String
        let url: String
        let sha256: String
        let size: Int
    }

    static let model = Asset(
        name: "model_quantized.onnx",
        url: "https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX/resolve/\(modelRevision)/onnx/model_quantized.onnx",
        sha256: "fbae9257e1e05ffc727e951ef9b9c98418e6d79f1c9b6b13bd59f5c9028a1478",
        size: 92_361_116
    )

    static let voices: [String: String] = [
        "af_heart": "d583ccff3cdca2f7fae535cb998ac07e9fcb90f09737b9a41fa2734ec44a8f0b",
        "af_bella": "f69d836209b78eb8c66e75e3cda491e26ea838a3674257e9d4e5703cbaf55c8b",
        "af_nicole": "cd2191ab31b914ed7b318416b0e4440fdf392ddad9106a060819aa600a64f59a",
        "af_sarah": "4409fbc125afabacc615d94db5398d847006a737b0247d6892b7a9a0007a2f0a",
        "am_michael": "1d1f21dd8da39c30705cd4c75d039d265e9bc4a2a93ed09bc9e1b1225eb95ba1",
        "am_fenrir": "c27989f741f7ee34d273a39d8a595cc0837d35f5ced9a29b7cc162614616df43",
        "am_puck": "fcf73c989033e9233e0b98713eca600c8c74dcc1614b37009d5450ff4a2274a0",
        "bf_emma": "669fe0647f9dd04fcab92f1439a40eeb4c8b4ab1f82e4996fe3d918ce4a63b73",
        "bf_isabella": "3754352c4aaa46d17f27654ab7518d65b62ad6163a0f55a5f4330c2da2c4e94f",
        "bm_george": "c4b235a4c1f2cd3b939fed08b899ce9385638b763f7b73a59616c4fc9bd6c9bc",
        "bm_fable": "f889083196807b4adb15e9204252165f503b8d33d3982e681c52443c49d798f1",
    ]

    static func voice(_ name: String) -> Asset? {
        guard let sha256 = voices[name] else { return nil }
        return Asset(
            name: "voices/\(name).bin",
            url: "https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX/resolve/\(modelRevision)/voices/\(name).bin",
            sha256: sha256,
            size: 522_240
        )
    }

    static func lexicons(british: Bool) -> [Asset] {
        let pinned = british
            ? [("gb_gold", "29e62f4b60261c88f7f3c2c7811ca3825978948090b72d2b27d565b729282f71", 2_838_552),
               ("gb_silver", "48131e2d92ccc41655f4543e87e0f938e71463eb5a54be7f0693bb712ebb6bce", 3_663_898)]
            : [("us_gold", "dc414872a49a28ae6c141463d502fd945f3b2fde040484fdc47d00cc4612686f", 3_000_469),
               ("us_silver", "de8f67be911bb6c659187b4a65fd966b6a30e56350e0f790d763210b053ac475", 3_099_517)]
        return pinned.map { name, sha256, size in
            Asset(
                name: "lexicon/\(name).json",
                url: "https://raw.githubusercontent.com/hexgrad/misaki/\(lexiconRevision)/misaki/data/\(name).json",
                sha256: sha256,
                size: size
            )
        }
    }

    /// Kokoro's voice names start with the accent: a… American, b… British.
    static func isBritish(_ voice: String) -> Bool { voice.hasPrefix("b") }

    static func assets(voice: String) -> [Asset] {
        [model] + (KokoroFiles.voice(voice).map { [$0] } ?? []) + lexicons(british: isBritish(voice))
    }

    static func missing(in dir: URL, voice: String) -> [Asset] {
        assets(voice: voice).filter {
            !FileManager.default.fileExists(atPath: dir.appendingPathComponent($0.name).path)
        }
    }
}

/// `diri-voice kokoro-download --dir DIR --voice NAME`: fetches what is
/// missing, verifying each file before it is moved into place. Prints
/// "progress <done> <total>" lines in bytes, then "ready".
func runKokoroDownload(_ arguments: [String]) async {
    guard let dir = values("--dir", in: arguments).first.map({ URL(fileURLWithPath: $0) }),
        let voice = values("--voice", in: arguments).first,
        KokoroFiles.voices[voice] != nil
    else { fail("usage: diri-voice kokoro-download --dir DIR --voice NAME", code: 64) }
    let missing = KokoroFiles.missing(in: dir, voice: voice)
    let total = missing.reduce(0) { $0 + $1.size }
    var done = 0
    for asset in missing {
        let destination = dir.appendingPathComponent(asset.name)
        do {
            try FileManager.default.createDirectory(
                at: destination.deletingLastPathComponent(), withIntermediateDirectories: true)
            guard let url = URL(string: asset.url) else { fail("bad url \(asset.url)") }
            let (temporary, response) = try await ProgressDownload(before: done, total: total).run(url)
            guard (response as? HTTPURLResponse)?.statusCode == 200 else {
                fail("download failed: \(asset.name)")
            }
            let digest = try sha256(of: temporary)
            guard digest == asset.sha256 else {
                try? FileManager.default.removeItem(at: temporary)
                fail("\(asset.name) does not match its pinned SHA-256")
            }
            _ = try? FileManager.default.removeItem(at: destination)
            try FileManager.default.moveItem(at: temporary, to: destination)
        } catch {
            fail("download failed: \(asset.name): \(error)")
        }
        done += asset.size
        print("progress \(done) \(total)")
        fflush(stdout)
    }
    print("ready")
}

/// Downloads one file, printing overall progress at most every 2%.
final class ProgressDownload: NSObject, URLSessionDownloadDelegate, @unchecked Sendable {
    private let before: Int
    private let total: Int
    private var reported = -1
    private var continuation: CheckedContinuation<(URL, URLResponse), Error>?
    private var kept: URL?

    init(before: Int, total: Int) {
        self.before = before
        self.total = max(total, 1)
    }

    /// The downloaded file (the caller moves or deletes it) and response.
    func run(_ url: URL) async throws -> (URL, URLResponse) {
        let session = URLSession(configuration: .ephemeral, delegate: self, delegateQueue: nil)
        defer { session.finishTasksAndInvalidate() }
        return try await withCheckedThrowingContinuation { continuation in
            self.continuation = continuation
            session.downloadTask(with: url).resume()
        }
    }

    func urlSession(
        _: URLSession, downloadTask _: URLSessionDownloadTask, didWriteData _: Int64,
        totalBytesWritten written: Int64, totalBytesExpectedToWrite _: Int64
    ) {
        let done = before + Int(written)
        let percent = done * 100 / total
        guard percent >= reported + 2 else { return }
        reported = percent
        print("progress \(done) \(total)")
        fflush(stdout)
    }

    func urlSession(_: URLSession, downloadTask: URLSessionDownloadTask, didFinishDownloadingTo location: URL) {
        // The system deletes `location` when this returns.
        let kept = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        if (try? FileManager.default.moveItem(at: location, to: kept)) != nil {
            self.kept = kept
        }
    }

    func urlSession(_: URLSession, task: URLSessionTask, didCompleteWithError error: Error?) {
        guard let continuation else { return }
        self.continuation = nil
        if let error {
            continuation.resume(throwing: error)
        } else if let kept, let response = task.response {
            continuation.resume(returning: (kept, response))
        } else {
            continuation.resume(throwing: URLError(.cannotCreateFile))
        }
    }
}

func sha256(of file: URL) throws -> String {
    let handle = try FileHandle(forReadingFrom: file)
    defer { try? handle.close() }
    var hasher = SHA256()
    while let chunk = try handle.read(upToCount: 1 << 20), !chunk.isEmpty {
        hasher.update(data: chunk)
    }
    return hasher.finalize().map { String(format: "%02x", $0) }.joined()
}

#if KOKORO

/// Kokoro's phoneme vocabulary (onnx-community/Kokoro-82M-v1.0-ONNX
/// tokenizer.json). Keys are single scalars: some are combining marks.
let kokoroVocab: [Unicode.Scalar: Int64] = [
    "$": 0, ";": 1, ":": 2, ",": 3, ".": 4, "!": 5, "?": 6, "—": 9,
    "…": 10, "\"": 11, "(": 12, ")": 13, "“": 14, "”": 15, " ": 16, "̃": 17,
    "ʣ": 18, "ʥ": 19, "ʦ": 20, "ʨ": 21, "ᵝ": 22, "ꭧ": 23, "A": 24, "I": 25,
    "O": 31, "Q": 33, "S": 35, "T": 36, "W": 39, "Y": 41, "ᵊ": 42, "a": 43,
    "b": 44, "c": 45, "d": 46, "e": 47, "f": 48, "h": 50, "i": 51, "j": 52,
    "k": 53, "l": 54, "m": 55, "n": 56, "o": 57, "p": 58, "q": 59, "r": 60,
    "s": 61, "t": 62, "u": 63, "v": 64, "w": 65, "x": 66, "y": 67, "z": 68,
    "ɑ": 69, "ɐ": 70, "ɒ": 71, "æ": 72, "β": 75, "ɔ": 76, "ɕ": 77, "ç": 78,
    "ɖ": 80, "ð": 81, "ʤ": 82, "ə": 83, "ɚ": 85, "ɛ": 86, "ɜ": 87, "ɟ": 90,
    "ɡ": 92, "ɥ": 99, "ɨ": 101, "ɪ": 102, "ʝ": 103, "ɯ": 110, "ɰ": 111, "ŋ": 112,
    "ɳ": 113, "ɲ": 114, "ɴ": 115, "ø": 116, "ɸ": 118, "θ": 119, "œ": 120, "ɹ": 123,
    "ɾ": 125, "ɻ": 126, "ʁ": 128, "ɽ": 129, "ʂ": 130, "ʃ": 131, "ʈ": 132, "ʧ": 133,
    "ʊ": 135, "ʋ": 136, "ʌ": 138, "ɣ": 139, "ɤ": 140, "χ": 142, "ʎ": 143, "ʒ": 147,
    "ʔ": 148, "ˈ": 156, "ˌ": 157, "ː": 158, "ʰ": 162, "ʲ": 164, "↓": 169, "→": 171,
    "↗": 172, "↘": 173, "ᵻ": 177,
]

enum KokoroError: Error, CustomStringConvertible {
    case runtime(String)
    case files(String)
    var description: String {
        switch self {
        case .runtime(let message), .files(let message): return message
        }
    }
}

/// One loaded model and voice. Not thread-safe; one per process.
final class KokoroVoice {
    static let sampleRate = 24_000.0
    /// The model's context, padding included.
    static let maxTokens = 510

    private let api: OrtApi
    private var env: OpaquePointer?
    private var session: OpaquePointer?
    private var memory: OpaquePointer?
    /// 510 rows of 256: the style for an input of each length.
    private let styles: [Float]
    private let g2p: EnglishG2P

    init(dir: URL, voice: String) throws {
        guard let base = OrtGetApiBase(), let api = base.pointee.GetApi(UInt32(ORT_API_VERSION)) else {
            throw KokoroError.runtime("ONNX Runtime unavailable")
        }
        self.api = api.pointee
        guard let voiceAsset = KokoroFiles.voice(voice) else { throw KokoroError.files("unknown voice \(voice)") }
        let missing = KokoroFiles.missing(in: dir, voice: voice)
        guard missing.isEmpty else { throw KokoroError.files("missing \(missing.map(\.name))") }
        let voiceData = try Data(contentsOf: dir.appendingPathComponent(voiceAsset.name))
        styles = voiceData.withUnsafeBytes { Array($0.bindMemory(to: Float.self)) }
        let lexicons = KokoroFiles.lexicons(british: KokoroFiles.isBritish(voice))
        g2p = try EnglishG2P(
            gold: Data(contentsOf: dir.appendingPathComponent(lexicons[0].name)),
            silver: Data(contentsOf: dir.appendingPathComponent(lexicons[1].name)),
            british: KokoroFiles.isBritish(voice)
        )
        try check(self.api.CreateEnv(ORT_LOGGING_LEVEL_ERROR, "diri-voice", &env))
        var options: OpaquePointer?
        try check(self.api.CreateSessionOptions(&options))
        defer { self.api.ReleaseSessionOptions(options) }
        try check(self.api.SetIntraOpNumThreads(options, 4))
        try check(self.api.SetSessionGraphOptimizationLevel(options, ORT_ENABLE_ALL))
        let model = dir.appendingPathComponent(KokoroFiles.model.name).path
        try check(self.api.CreateSession(env, model, options, &session))
        try check(self.api.CreateCpuMemoryInfo(OrtArenaAllocator, OrtMemTypeDefault, &memory))
    }

    deinit {
        if let memory { api.ReleaseMemoryInfo(memory) }
        if let session { api.ReleaseSession(session) }
        if let env { api.ReleaseEnv(env) }
    }

    private func check(_ status: OpaquePointer?) throws {
        guard let status else { return }
        let message = api.GetErrorMessage(status).map { String(cString: $0) } ?? "ONNX Runtime error"
        api.ReleaseStatus(status)
        throw KokoroError.runtime(message)
    }

    func phonemes(_ text: String) -> String { g2p.phonemes(text) }

    /// Token ids for `phonemes`, cut to the model's context.
    func tokens(_ phonemes: String) -> [Int64] {
        Array(phonemes.unicodeScalars.compactMap { kokoroVocab[$0] }.prefix(Self.maxTokens - 2))
    }

    /// 24 kHz mono samples for one sentence's phonemes.
    func synthesize(tokens: [Int64], speed: Float) throws -> [Float] {
        guard !tokens.isEmpty else { return [] }
        var ids: [Int64] = [0] + tokens + [0]
        let row = min(tokens.count, styles.count / 256 - 1)
        var style = Array(styles[(row * 256)..<(row * 256 + 256)])
        var speed = speed
        var idShape: [Int64] = [1, Int64(ids.count)]
        var styleShape: [Int64] = [1, 256]
        var speedShape: [Int64] = [1]
        var inputs: [OpaquePointer?] = [nil, nil, nil]
        defer { inputs.forEach { if let value = $0 { api.ReleaseValue(value) } } }
        try ids.withUnsafeMutableBytes { idBytes in
            try check(api.CreateTensorWithDataAsOrtValue(
                memory, idBytes.baseAddress, idBytes.count, &idShape, 2,
                ONNX_TENSOR_ELEMENT_DATA_TYPE_INT64, &inputs[0]))
        }
        try style.withUnsafeMutableBytes { styleBytes in
            try check(api.CreateTensorWithDataAsOrtValue(
                memory, styleBytes.baseAddress, styleBytes.count, &styleShape, 2,
                ONNX_TENSOR_ELEMENT_DATA_TYPE_FLOAT, &inputs[1]))
        }
        try withUnsafeMutableBytes(of: &speed) { speedBytes in
            try check(api.CreateTensorWithDataAsOrtValue(
                memory, speedBytes.baseAddress, speedBytes.count, &speedShape, 1,
                ONNX_TENSOR_ELEMENT_DATA_TYPE_FLOAT, &inputs[2]))
        }
        var output: OpaquePointer?
        let cString = { (name: String) in name.withCString { strdup($0) } }
        let inputNames = ["input_ids", "style", "speed"].map(cString)
        let outputNames = ["waveform"].map(cString)
        defer { (inputNames + outputNames).forEach { free($0) } }
        var inputNamePointers: [UnsafePointer<CChar>?] = inputNames.map { $0.map { UnsafePointer($0) } }
        var outputNamePointers: [UnsafePointer<CChar>?] = outputNames.map { $0.map { UnsafePointer($0) } }
        try inputs.withUnsafeBufferPointer { values in
            try check(api.Run(
                session, nil, &inputNamePointers, values.baseAddress, 3,
                &outputNamePointers, 1, &output))
        }
        defer { if let output { api.ReleaseValue(output) } }
        var info: OpaquePointer?
        try check(api.GetTensorTypeAndShape(output, &info))
        defer { api.ReleaseTensorTypeAndShapeInfo(info) }
        var count = 0
        try check(api.GetTensorShapeElementCount(info, &count))
        var data: UnsafeMutableRawPointer?
        try check(api.GetTensorMutableData(output, &data))
        guard let data else { return [] }
        return Array(UnsafeBufferPointer(start: data.assumingMemoryBound(to: Float.self), count: count))
    }
}

/// Plays sentences as they are synthesized: the next one is generated while
/// the current one plays.
final class KokoroSpeaker {
    private let engine = AVAudioEngine()
    private let player = AVAudioPlayerNode()
    private let format = AVAudioFormat(standardFormatWithSampleRate: KokoroVoice.sampleRate, channels: 1)!

    init() throws {
        engine.attach(player)
        engine.connect(player, to: engine.mainMixerNode, format: format)
        try engine.start()
        player.play()
    }

    func speak(_ text: String, voice: KokoroVoice, speed: Float) async throws {
        var pending: [Task<Void, Never>] = []
        for sentence in kokoroSentences(text) {
            let samples = try voice.synthesize(tokens: voice.tokens(voice.phonemes(sentence)), speed: speed)
            guard !samples.isEmpty,
                let buffer = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: AVAudioFrameCount(samples.count))
            else { continue }
            buffer.frameLength = buffer.frameCapacity
            samples.withUnsafeBufferPointer { source in
                buffer.floatChannelData![0].update(from: source.baseAddress!, count: samples.count)
            }
            let player = self.player
            pending.append(Task {
                await withCheckedContinuation { continuation in
                    player.scheduleBuffer(buffer, completionCallbackType: .dataPlayedBack) { _ in
                        continuation.resume()
                    }
                }
            })
        }
        for task in pending { await task.value }
    }
}

/// Sentences, joined back into chunks short enough for the model's context.
func kokoroSentences(_ text: String) -> [String] {
    var sentences: [String] = []
    text.enumerateSubstrings(in: text.startIndex..., options: .bySentences) { sentence, _, _, _ in
        if let sentence = sentence?.trimmingCharacters(in: .whitespacesAndNewlines), !sentence.isEmpty {
            sentences.append(sentence)
        }
    }
    return sentences.isEmpty ? [text] : sentences
}

/// Speaks `text` with Kokoro when its files are present and the text is
/// English; false means the caller should use a system voice instead.
func speakWithKokoro(_ text: String, voice: String, dir: URL) async -> Bool {
    let recognizer = NLLanguageRecognizer()
    recognizer.processString(text)
    guard recognizer.dominantLanguage.map({ $0 == .english }) ?? true,
        KokoroFiles.missing(in: dir, voice: voice).isEmpty
    else { return false }
    do {
        let model = try KokoroVoice(dir: dir, voice: voice)
        try await KokoroSpeaker().speak(text, voice: model, speed: 1.0)
        return true
    } catch {
        FileHandle.standardError.write(Data("diri-voice: kokoro: \(error)\n".utf8))
        return false
    }
}

#else

func speakWithKokoro(_: String, voice _: String, dir _: URL) async -> Bool { false }

#endif
