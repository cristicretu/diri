import Foundation
import Security

/// One paired `diri-web` — "Mac", "forge". The list is ordered by priority:
/// the first reachable gateway is the live one, which new sessions, agents and
/// folders come from. Sessions are shown from every reachable gateway.
struct Gateway: Codable, Equatable, Identifiable, Sendable {
    var id: String
    var name: String
    var endpoint: DiriClient.Endpoint

    init(id: String = UUID().uuidString, name: String, endpoint: DiriClient.Endpoint) {
        self.id = id
        self.name = name
        self.endpoint = endpoint
    }

    /// The fallback name before the gateway has introduced itself.
    static func provisionalName(for endpoint: DiriClient.Endpoint) -> String {
        endpoint.baseURL.host ?? "Mac"
    }
}

// MARK: - Storage

/// Where bearer credentials live. The keychain in the app, memory in tests.
protocol SecretStore: AnyObject, Sendable {
    func read(_ account: String) -> Data?
    /// `nil` deletes. Returns whether the store now holds `data`.
    @discardableResult func write(_ account: String, _ data: Data?) -> Bool
}

final class KeychainSecretStore: SecretStore, @unchecked Sendable {
    private let service: String

    init(service: String = "com.cristicretu.diri.phone") {
        self.service = service
    }

    func read(_ account: String) -> Data? {
        var query = baseQuery(account)
        query[kSecReturnData as String] = true
        query[kSecMatchLimit as String] = kSecMatchLimitOne
        var item: CFTypeRef?
        guard SecItemCopyMatching(query as CFDictionary, &item) == errSecSuccess else { return nil }
        return item as? Data
    }

    @discardableResult
    func write(_ account: String, _ data: Data?) -> Bool {
        let query = baseQuery(account)
        SecItemDelete(query as CFDictionary)
        guard let data else { return true }
        var add = query
        add[kSecValueData as String] = data
        add[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlock
        return SecItemAdd(add as CFDictionary, nil) == errSecSuccess
    }

    private func baseQuery(_ account: String) -> [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
    }
}

/// The gateway list lives in the keychain: each entry carries a bearer token
/// that can start and kill processes on a real machine.
enum GatewayStore {
    static let listAccount = "gateways"
    /// The single-endpoint slot every build before multi-host used.
    static let legacyAccount = "endpoint"

    static func load(from store: SecretStore) -> [Gateway] {
        if let data = store.read(listAccount),
           let list = try? JSONDecoder().decode([Gateway].self, from: data)
        {
            return list
        }
        guard let data = store.read(legacyAccount),
              let endpoint = try? JSONDecoder().decode(DiriClient.Endpoint.self, from: data)
        else { return [] }
        let migrated = [Gateway(name: Gateway.provisionalName(for: endpoint), endpoint: endpoint)]
        // The old slot is only removed once the new one is safely written, so
        // a failed write cannot unpair the phone.
        if save(migrated, to: store) { store.write(legacyAccount, nil) }
        return migrated
    }

    @discardableResult
    static func save(_ gateways: [Gateway], to store: SecretStore) -> Bool {
        if gateways.isEmpty { return store.write(listAccount, nil) }
        guard let data = try? JSONEncoder().encode(gateways) else { return false }
        return store.write(listAccount, data)
    }

    /// Adds a newly paired gateway, or refreshes the token of one already
    /// paired at the same address (a re-pair keeps its priority).
    static func adding(_ gateway: Gateway, to list: [Gateway]) -> [Gateway] {
        var list = list
        if let index = list.firstIndex(where: { $0.endpoint.baseURL == gateway.endpoint.baseURL }) {
            list[index].endpoint = gateway.endpoint
            list[index].name = gateway.name
        } else {
            list.append(gateway)
        }
        return list
    }
}

// MARK: - Selection

enum GatewayProbe {
    enum Outcome: Equatable, Sendable {
        case reachable(host: String)
        case unauthorized
        /// Reached, but tailnet admission refused this device (the sentence
        /// says why and what to do).
        case refused(String)
        case unreachable(String)

        var isReachable: Bool {
            if case .reachable = self { return true }
            return false
        }
    }

    /// Probes every gateway at once. A gateway that has not answered within
    /// `timeout` is unreachable — one dead tailnet peer must not hold the app
    /// on a spinner while another host is right there.
    static func probeAll(
        _ gateways: [Gateway],
        timeout: Duration,
        probe: @escaping @Sendable (Gateway) async -> Outcome
    ) async -> [String: Outcome] {
        await withTaskGroup(of: (String, Outcome).self) { group in
            for gateway in gateways {
                group.addTask {
                    let outcome = await withTaskGroup(of: Outcome?.self) { race in
                        race.addTask { await probe(gateway) }
                        race.addTask {
                            try? await Task.sleep(for: timeout)
                            return Task.isCancelled ? nil : .unreachable("No answer in time")
                        }
                        var first: Outcome?
                        for await result in race {
                            if let result { first = result; break }
                        }
                        race.cancelAll()
                        return first ?? .unreachable("No answer in time")
                    }
                    return (gateway.id, outcome)
                }
            }
            var outcomes: [String: Outcome] = [:]
            for await (id, outcome) in group { outcomes[id] = outcome }
            return outcomes
        }
    }

    /// The highest-priority reachable gateway.
    static func live(_ gateways: [Gateway], outcomes: [String: Outcome]) -> Gateway? {
        gateways.first { outcomes[$0.id]?.isReachable == true }
    }
}
