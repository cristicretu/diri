import XCTest

@testable import DiriPhone

final class PairingHelpTests: XCTestCase {
  func testPairingCannotWaitIndefinitelyForAnUnavailableVPN() {
    let configuration = DiriClient.pairingConfiguration()
    XCTAssertFalse(configuration.waitsForConnectivity)
    XCTAssertEqual(configuration.timeoutIntervalForRequest, 15)
    XCTAssertEqual(configuration.timeoutIntervalForResource, 20)
  }

  func testRevokedCodeHasDifferentRecoveryFromNetworkFailure() {
    let revoked = PairingHelp.message(for: DiriClient.Failure.unauthorized)
    XCTAssertTrue(revoked.contains("current code"))
    let offline = PairingHelp.message(for: URLError(.notConnectedToInternet))
    XCTAssertTrue(offline.contains("Tailscale"))
    XCTAssertNotEqual(revoked, offline)
  }

  func testPrivateServerAndNetworkDetailsAreNotShownInSetupErrors() {
    for error: Error in [
      DiriClient.Failure.daemonUnreachable("secret"),
      DiriClient.Failure.http(500, "secret"),
      URLError(
        .badURL, userInfo: [NSURLErrorFailingURLStringErrorKey: "http://host/?token=secret"]),
    ] {
      XCTAssertFalse(PairingHelp.message(for: error).contains("secret"))
    }
  }
}
