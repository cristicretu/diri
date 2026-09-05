import XCTest
@testable import DiriPhone

/// The requests are a contract with `diri-web`. A wrong path, a missing token
/// header or a mis-shaped body all surface as "the daemon is not reachable",
/// so they are worth pinning down here rather than discovering on a phone.
final class DiriClientTests: XCTestCase {
    private var client: DiriClient!

    override func setUp() {
        super.setUp()
        StubProtocol.reset()
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [StubProtocol.self]
        client = DiriClient(
            endpoint: .init(baseURL: URL(string: "http://forge:7380")!, token: "tok"),
            configuration: configuration
        )
    }

    override func tearDown() {
        StubProtocol.reset()
        client = nil
        super.tearDown()
    }

    func testSessionsCarriesTheTokenHeader() async throws {
        StubProtocol.respond(json: #"{"host":"forge","sessions":[],"projects":[]}"#)
        _ = try await client.sessions()
        let request = try XCTUnwrap(StubProtocol.lastRequest)
        XCTAssertEqual(request.url?.path, "/api/sessions")
        XCTAssertEqual(request.value(forHTTPHeaderField: "X-Diri-Token"), "tok")
        XCTAssertEqual(request.httpMethod, "GET")
    }

    func testPairingRejectsUnsafeOrAmbiguousURLs() {
        for link in ["file:///tmp?token=t", "ftp://host?token=t", "http://user:pass@host?token=t",
                     "http://host?token=a&token=b", "http://host?token=t#secret", "http://host"] {
            XCTAssertNil(DiriClient.Endpoint(enrolmentURL: link), link)
        }
        let endpoint = DiriClient.Endpoint(enrolmentURL: " http://100.90.0.2:7380/?token=test \n")
        XCTAssertEqual(endpoint?.token, "test")
        XCTAssertEqual(endpoint?.baseURL.absoluteString, "http://100.90.0.2:7380")
    }

    func testRemoteWorktreeSpawnCarriesHostAndMainExplicitly() async throws {
        StubProtocol.respond(json: """
        {"id":"s_9","kind":{"shell":{}},"cwd":"/r-wt","projectID":"p","title":"shell",
         "host":"remote-a","status":{"starting":{}},"pinned":false,"createdAt":1,"updatedAt":1}
        """)
        let record = try await client.spawn(kind: "shell", cwd: "/r", prompt: "build",
            host: "remote-a", worktree: true, branch: "phone/fix")
        XCTAssertEqual(record.host, "remote-a")
        let body = try XCTUnwrap(StubProtocol.lastBody)
        let json = try XCTUnwrap(try JSONSerialization.jsonObject(with: body) as? [String: Any])
        XCTAssertEqual(json["host"] as? String, "remote-a")
        XCTAssertEqual(json["worktree"] as? Bool, true)
        XCTAssertEqual(json["base"] as? String, "main")
        XCTAssertEqual(json["branch"] as? String, "phone/fix")
        XCTAssertEqual(StubProtocol.lastRequest?.timeoutInterval, 330)
    }

    func testReadinessUsesTheSelectedHostAndIncludesShell() async throws {
        StubProtocol.respond(json: #"{"agents":[{"kind":"shell","binary":""},{"kind":"codex","binary":"codex","path":"/bin/codex"},{"kind":"claude-code","binary":"claude"}]}"#)
        let agents = try await client.agents(host: "remote-a")
        XCTAssertEqual(agents.map(\.kind.id), ["shell", "codex"])
        XCTAssertEqual(StubProtocol.lastRequest?.url?.query, "host=remote-a")
    }

    func testDirectoryPathsAreEncodedAsQueryData() async throws {
        StubProtocol.respond(json: #"{"path":"/repo","entries":[],"truncated":false}"#)
        _ = try await client.directories(host: "remote-a", path: "/repo & other/#folder")
        let components = try XCTUnwrap(URLComponents(url: try XCTUnwrap(StubProtocol.lastRequest?.url), resolvingAgainstBaseURL: true))
        XCTAssertEqual(components.path, "/api/directories")
        XCTAssertEqual(components.queryItems?.first { $0.name == "path" }?.value, "/repo & other/#folder")
        XCTAssertEqual(components.queryItems?.first { $0.name == "host" }?.value, "remote-a")
    }

    func testRemoteProjectRetainsHostIdentity() throws {
        let project = try JSONDecoder().decode(Project.self, from: Data(#"{"id":"p1","root":"/repo","name":"Project","host":"remote-a"}"#.utf8))
        XCTAssertEqual(project.host, "remote-a")
    }

    func testChangesUsesTheSessionEndpoint() async throws {
        StubProtocol.respond(json: #"{"patch":"+new","repoRoot":"/repo","truncated":false}"#)
        let diff = try await client.diff(sessionID: "s_1")
        XCTAssertEqual(diff.patch, "+new")
        XCTAssertEqual(StubProtocol.lastRequest?.url?.path, "/api/session/s_1/diff")
    }

    func testProjectsAreOptionalForOlderServers() async throws {
        // A `diri-web` that predates the projects field must not break the app.
        StubProtocol.respond(json: #"{"host":"forge","sessions":[]}"#)
        let payload = try await client.sessions()
        XCTAssertNil(payload.projects)
        XCTAssertEqual(payload.host, "forge")
    }

    func testSendPostsTextAndSubmitFlag() async throws {
        StubProtocol.respond(json: #"{"ok":true}"#)
        try await client.send(text: "hello", to: "s_1")
        let request = try XCTUnwrap(StubProtocol.lastRequest)
        XCTAssertEqual(request.httpMethod, "POST")
        XCTAssertEqual(request.url?.path, "/api/session/s_1/send")
        let body = try XCTUnwrap(StubProtocol.lastBody)
        let json = try XCTUnwrap(
            try JSONSerialization.jsonObject(with: body) as? [String: Any]
        )
        XCTAssertEqual(json["text"] as? String, "hello")
        XCTAssertEqual(json["submit"] as? Bool, true)
    }

    /// Keys go as `submit: false`, which `diri-web` writes to the PTY verbatim.
    /// Submitting them would frame the escape sequence as a bracketed paste and
    /// append an Enter, turning ⌃C into a typed prompt.
    func testKeysUseTheirWireName() async throws {
        StubProtocol.respond(json: #"{"ok":true}"#)
        try await client.send(key: .ctrlC, to: "s_1")
        let body = try XCTUnwrap(StubProtocol.lastBody)
        let json = try XCTUnwrap(try JSONSerialization.jsonObject(with: body) as? [String: Any])
        XCTAssertEqual(json["key"] as? String, "ctrl-c")
        XCTAssertEqual(StubProtocol.lastRequest?.url?.path, "/api/session/s_1/key")
    }

    func testSpawnOmitsAnEmptyPrompt() async throws {
        StubProtocol.respond(json: """
        {"id":"s_9","kind":{"shell":{}},"cwd":"/r","projectID":"/r","title":"shell",
         "status":{"starting":{}},"pinned":false,"createdAt":1.0,"updatedAt":1.0}
        """)
        let record = try await client.spawn(kind: "shell", cwd: "/r", prompt: "   ")
        XCTAssertEqual(record.id, "s_9")
        let body = try XCTUnwrap(StubProtocol.lastBody)
        let json = try XCTUnwrap(try JSONSerialization.jsonObject(with: body) as? [String: Any])
        XCTAssertNil(json["prompt"], "a whitespace-only prompt must not be sent")
        XCTAssertEqual(json["kind"] as? String, "shell")
    }

    func testA401BecomesUnauthorized() async {
        StubProtocol.respond(json: #"{"error":"unauthorized"}"#, status: 401)
        do {
            _ = try await client.sessions()
            XCTFail("expected an unauthorized failure")
        } catch DiriClient.Failure.unauthorized {
            // expected
        } catch {
            XCTFail("expected unauthorized, got \(error)")
        }
    }

    func testAServerErrorSurfacesItsMessage() async {
        StubProtocol.respond(json: #"{"error":"no such session"}"#, status: 400)
        do {
            _ = try await client.screen(sessionID: "gone")
            XCTFail("expected a failure")
        } catch {
            XCTAssertTrue(
                error.localizedDescription.contains("no such session"),
                "got \(error.localizedDescription)"
            )
        }
    }

    /// Session ids are opaque; anything needing escaping must survive the path.
    func testSessionIDsArePercentEncoded() async throws {
        StubProtocol.respond(json: #"{"text":"","cols":80,"rows":24}"#)
        _ = try await client.screen(sessionID: "a b/c")
        let url = try XCTUnwrap(StubProtocol.lastRequest?.url?.absoluteString)
        XCTAssertTrue(url.contains("a%20b"), "spaces must be escaped: \(url)")
    }
}

/// A `URLProtocol` that answers every request from a canned response and
/// records what it was asked.
private final class StubProtocol: URLProtocol {
    nonisolated(unsafe) private static var body = Data()
    nonisolated(unsafe) private static var status = 200
    nonisolated(unsafe) static var lastRequest: URLRequest?
    nonisolated(unsafe) static var lastBody: Data?

    static func reset() {
        body = Data()
        status = 200
        lastRequest = nil
        lastBody = nil
    }

    static func respond(json: String, status: Int = 200) {
        body = Data(json.utf8)
        self.status = status
    }

    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }

    override func startLoading() {
        Self.lastRequest = request
        // `URLProtocol` strips the body into a stream, so read it back out.
        Self.lastBody = request.httpBody ?? request.httpBodyStream.map { stream in
            stream.open()
            defer { stream.close() }
            var data = Data()
            var buffer = [UInt8](repeating: 0, count: 4096)
            while stream.hasBytesAvailable {
                let read = stream.read(&buffer, maxLength: buffer.count)
                if read <= 0 { break }
                data.append(buffer, count: read)
            }
            return data
        }

        let response = HTTPURLResponse(
            url: request.url!, statusCode: Self.status, httpVersion: "HTTP/1.1", headerFields: nil
        )!
        client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: Self.body)
        client?.urlProtocolDidFinishLoading(self)
    }

    override func stopLoading() {}
}
