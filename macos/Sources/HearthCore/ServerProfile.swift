import Foundation

/// Public identifiers returned by Rust. No profile contents or private keys enter Swift.
public struct ServerIdentity: Codable, Hashable, Sendable {
    public let serverId: String
    public let environment: String
    public let deviceId: String

    enum CodingKeys: String, CodingKey {
        case serverId = "server_id"
        case environment
        case deviceId = "device_id"
    }

    public init(serverId: String, environment: String, deviceId: String) {
        self.serverId = serverId
        self.environment = environment
        self.deviceId = deviceId
    }

    public var isValid: Bool {
        Self.hex(serverId, count: 64) && Self.hex(environment, count: 32) && Self.hex(deviceId, count: 64)
    }

    private static func hex(_ value: String, count: Int) -> Bool {
        value.utf8.count == count && value.utf8.allSatisfy { (48...57).contains($0) || (97...102).contains($0) }
    }

    public var arguments: [String] {
        ["--expected-server-id", serverId, "--expected-environment", environment, "--expected-device-id", deviceId]
    }
}

public struct ServerProfile: Codable, Hashable, Sendable, Identifiable {
    public enum Mode: String, Codable, Hashable, Sendable { case controller, observation }
    public var id: UUID
    public var name: String
    public var profilePath: String
    public var identity: ServerIdentity?
    /// Optional for backward-compatible decoding of existing controller profiles.
    public var mode: Mode?
    public var observationIdentity: ObservationIdentity?
    public var expectedBackend: BackendKind?

    public init(id: UUID = UUID(), name: String = "New Server", profilePath: String = "", identity: ServerIdentity? = nil,
                mode: Mode = .controller, observationIdentity: ObservationIdentity? = nil, expectedBackend: BackendKind? = nil) {
        self.id = id
        self.name = name
        self.profilePath = profilePath
        self.identity = identity
        self.mode = mode
        self.observationIdentity = observationIdentity
        self.expectedBackend = expectedBackend
    }

    public var isReadOnly: Bool { mode == .observation }
    public var isTrusted: Bool { isReadOnly ? observationIdentity != nil : identity != nil }

    public var canonicalProfilePath: String {
        URL(fileURLWithPath: profilePath).standardizedFileURL.path
    }
}
