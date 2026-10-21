import XCTest
@testable import DiriPhone

final class VoicePauseTests: XCTestCase {
  func testAShortPauseMidSentenceDoesNotSend() {
    XCTAssertFalse(VoiceController.utteranceEnded(sinceWords: .milliseconds(1600), sinceLoud: .seconds(5)))
  }

  func testQuietAfterTheLastWordsSends() {
    XCTAssertTrue(VoiceController.utteranceEnded(sinceWords: .milliseconds(2600), sinceLoud: .milliseconds(2600)))
  }

  func testStillTalkingWhileTheRecogniserLagsHoldsTheSend() {
    XCTAssertFalse(VoiceController.utteranceEnded(sinceWords: .seconds(3), sinceLoud: .milliseconds(200)))
  }

  func testGymNoiseCannotHoldTheMicrophoneOpenForever() {
    XCTAssertTrue(VoiceController.utteranceEnded(sinceWords: .milliseconds(5600), sinceLoud: .zero))
  }
}

final class VoiceCueTests: XCTestCase {
  func testCuesAreShortAudibleAndClickFree() {
    for cue in [Dictation.Cue.listening, .sent, .failed] {
      let samples = Dictation.samples(for: cue, sampleRate: 44_100)
      XCTAssertLessThan(Double(samples.count) / 44_100, 0.5, "\(cue) must stay short")
      XCTAssertGreaterThan(samples.map(abs).max() ?? 0, 0.2, "\(cue) must be audible")
      XCTAssertLessThanOrEqual(samples.map(abs).max() ?? 0, 0.25)
      XCTAssertEqual(samples.first ?? 1, 0, accuracy: 0.001)
      XCTAssertEqual(samples.last ?? 1, 0, accuracy: 0.01)
    }
  }

  func testSentAndFailedSoundDifferent() {
    XCTAssertNotEqual(Dictation.samples(for: .sent, sampleRate: 44_100).count,
                      Dictation.samples(for: .failed, sampleRate: 44_100).count)
  }
}
