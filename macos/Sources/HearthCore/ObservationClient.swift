import Foundation

public struct ObservationIdentity: Codable, Hashable, Sendable {
    public let hostId: String
    public let binding: String
    public let node: String
    public let hostKeySha256: String
    public let machineIdSha256: String
    public let zpoolGuid: String
    public let zfsRoot: String
    public let knownHostsSha256: String

    enum CodingKeys: String, CodingKey {
        case hostId = "host_id", binding, node
        case hostKeySha256 = "host_key_sha256", machineIdSha256 = "machine_id_sha256"
        case zpoolGuid = "zpool_guid", zfsRoot = "zfs_root"
        case knownHostsSha256 = "known_hosts_sha256"
    }

    public var isValid: Bool {
        [hostId, binding, machineIdSha256, knownHostsSha256].allSatisfy { $0.utf8.count == 64 && $0.utf8.allSatisfy { (48...57).contains($0) || (97...102).contains($0) } }
            && hostKeySha256.hasPrefix("SHA256:") && hostKeySha256.utf8.count == 50
            && !node.isEmpty && UInt64(zpoolGuid) != nil && !zfsRoot.isEmpty
    }
}

public struct ObservedVM: Codable, Hashable, Sendable, Identifiable {
    public let id: String
    public let vmid: UInt32
    public let name: String
    public let status: String
    public let cores: UInt64?
    public let memoryMib: UInt64?
    public let balloonMib: UInt64?
    public let diskBytes: UInt64
    public let note: String?
    public let pendingChanges: Bool
    enum CodingKeys: String, CodingKey {
        case id, vmid, name, status, cores, note
        case memoryMib = "memory_mib", balloonMib = "balloon_mib", diskBytes = "disk_bytes", pendingChanges = "pending_changes"
    }
}

public struct ObservedDataset: Codable, Hashable, Sendable, Identifiable {
    public var id: String { guid }
    public let name: String
    public let guid: String
    public let usedBytes: UInt64?
    public let availableBytes: UInt64?
    public let quotaBytes: UInt64?
    public let reservationBytes: UInt64?
    enum CodingKeys: String, CodingKey {
        case name, guid
        case usedBytes = "used_bytes", availableBytes = "available_bytes", quotaBytes = "quota_bytes", reservationBytes = "reservation_bytes"
    }
}

public struct ObservationResult: Codable, Hashable, Sendable {
    public let schema: String
    public let identity: ObservationIdentity
    public let capturedAt: String
    public let receivedAtUnixMs: UInt64
    public let snapshotDigest: String
    public let collectorSha256: String
    public let pveVersion: String
    public let poolName: String
    public let poolHealth: String
    public let rootUsedBytes: UInt64?
    public let rootAvailableBytes: UInt64?
    public let vms: [ObservedVM]
    public let datasets: [ObservedDataset]
    public let containerCount: UInt64
    public let warnings: [String]
    public let managed: Bool
    public let authorizesManagement: Bool
    public let createsControllerState: Bool
    enum CodingKeys: String, CodingKey {
        case schema, identity, vms, datasets, warnings, managed
        case capturedAt = "captured_at", receivedAtUnixMs = "received_at_unix_ms", snapshotDigest = "snapshot_digest"
        case collectorSha256 = "collector_sha256", pveVersion = "pve_version", poolName = "pool_name", poolHealth = "pool_health"
        case rootUsedBytes = "root_used_bytes", rootAvailableBytes = "root_available_bytes", containerCount = "container_count"
        case authorizesManagement = "authorizes_management", createsControllerState = "creates_controller_state"
    }

    public var isValidReadOnlyResult: Bool {
        schema == "pve-dev-observation-result/1" && identity.isValid
            && !managed && !authorizesManagement && !createsControllerState
            && vms.count <= 4096 && datasets.count <= 16384 && warnings.count <= 4096
            && Set(vms.map(\.id)).count == vms.count
            && Set(vms.map(\.vmid)).count == vms.count
            && vms.allSatisfy { $0.id == "\(identity.hostId)/\($0.vmid)" }
    }
}

public struct ObservationConfiguration: Hashable, Sendable {
    public let executableURL: URL
    public let profilePath: String
    public let expectedIdentity: ObservationIdentity?
    public let limits: ProcessLimits

    public init(executableURL: URL, profilePath: String, expectedIdentity: ObservationIdentity?, limits: ProcessLimits) {
        self.executableURL = executableURL; self.profilePath = profilePath
        self.expectedIdentity = expectedIdentity; self.limits = limits
    }
}

public struct ObservationClient: Sendable {
    public let runner: any ProcessRunning
    public let environment: [String: String]

    public init(runner: any ProcessRunning = ChildProcessRunner(), environment: [String: String]? = nil) {
        self.runner = runner
        var inherited = ChildEnvironment.make()
        // Only the explicit read-only SSH flow receives the existing agent socket.
        if let socket = ProcessInfo.processInfo.environment["SSH_AUTH_SOCK"] { inherited["SSH_AUTH_SOCK"] = socket }
        self.environment = environment ?? inherited
    }

    public func profileIdentity(configuration: ObservationConfiguration) async throws(RpcFailure) -> ObservationIdentity {
        let data = try await execute(arguments: ["observe-profile-info", "--profile", configuration.profilePath], configuration: configuration)
        guard let identity = try? JSONDecoder().decode(ObservationIdentity.self, from: data), identity.isValid else {
            throw .malformedResponse("The inventory CLI did not return valid public host pins.")
        }
        return identity
    }

    public func observe(configuration: ObservationConfiguration) async throws(RpcFailure) -> ObservationResult {
        guard let expected = configuration.expectedIdentity, expected.isValid else {
            throw .configuration("Inspect and trust this read-only host profile before refreshing.")
        }
        let data = try await execute(arguments: ["observe", "--profile", configuration.profilePath,
            "--expected-binding", expected.binding, "--timeout-seconds", String(Int(configuration.limits.timeout))], configuration: configuration)
        guard let result = try? JSONDecoder().decode(ObservationResult.self, from: data), result.isValidReadOnlyResult else {
            throw .malformedResponse("The inventory CLI returned an invalid read-only observation.")
        }
        guard result.identity == expected else { throw .configuration("Observed host identity changed. Review the pinned host profile before trusting it again.") }
        return result
    }

    private func execute(arguments: [String], configuration: ObservationConfiguration) async throws(RpcFailure) -> Data {
        let result: ProcessResult
        do {
            result = try await runner.run(ProcessInvocation(executableURL: configuration.executableURL,
                arguments: arguments, standardInput: Data(), environment: environment, limits: configuration.limits))
        } catch {
            switch error {
            case .launchFailed(let reason): throw .launchFailed(reason)
            case .timedOut: throw .timedOut(seconds: Int(configuration.limits.timeout.rounded()))
            case .cancelled: throw .cancelled
            case .stdoutOverflow: throw .outputTooLarge
            }
        }
        guard !result.stdoutOverflowed else { throw .outputTooLarge }
        guard result.terminationStatus == 0 && !result.terminatedBySignal else {
            throw .processFailed(status: result.terminationStatus, signaled: result.terminatedBySignal,
                stderr: SafeText.tail(result.stderr, limit: 1500))
        }
        return result.stdout
    }
}
