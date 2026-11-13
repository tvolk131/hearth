import Foundation

/// Public routing metadata is forwarded intact; private profile paths and keys
/// stay in pve-devctl. Endpoint addresses are not interpreted as authority.
public enum WireJSONValue: Codable, Hashable, Sendable {
    case object([String: WireJSONValue]), array([WireJSONValue]), string(String), unsigned(UInt64), signed(Int64), number(Double), bool(Bool), null
    public init(from decoder: any Decoder) throws {
        let c = try decoder.singleValueContainer()
        if c.decodeNil() { self = .null }
        else if let v = try? c.decode(Bool.self) { self = .bool(v) }
        else if let v = try? c.decode(UInt64.self) { self = .unsigned(v) }
        else if let v = try? c.decode(Int64.self) { self = .signed(v) }
        else if let v = try? c.decode(Double.self) { self = .number(v) }
        else if let v = try? c.decode(String.self) { self = .string(v) }
        else if let v = try? c.decode([WireJSONValue].self) { self = .array(v) }
        else { self = .object(try c.decode([String: WireJSONValue].self)) }
    }
    public func encode(to encoder: any Encoder) throws {
        var c = encoder.singleValueContainer()
        switch self {
        case .object(let v): try c.encode(v)
        case .array(let v): try c.encode(v)
        case .string(let v): try c.encode(v)
        case .unsigned(let v): try c.encode(v)
        case .signed(let v): try c.encode(v)
        case .number(let v): try c.encode(v)
        case .bool(let v): try c.encode(v)
        case .null: try c.encodeNil()
        }
    }
    public subscript(key: String) -> WireJSONValue? { if case .object(let value) = self { value[key] } else { nil } }
    public var string: String? { if case .string(let value) = self { value } else { nil } }
}

public struct MigrationDescriptor: Codable, Hashable, Sendable {
    public var schema: UInt32
    public var endpoint: WireJSONValue
    public var environment: String
    public var network: String
    public var serverId: String? { endpoint["id"]?.string }
    public func matches(_ identity: ServerIdentity) -> Bool {
        schema == 1 && serverId == identity.serverId && environment == identity.environment
    }
}

public struct MigrationPeer: Codable, Hashable, Sendable {
    public var serverId: String
    public var environment: String
    enum CodingKeys: String, CodingKey { case serverId = "server_id", environment }
    public func matches(_ identity: ServerIdentity) -> Bool { serverId == identity.serverId && environment == identity.environment }
}

public struct MigrationPlan: Codable, Hashable, Sendable {
    public var schema: UInt32
    public var migrationId: String
    public var source: MigrationPeer
    public var destination: MigrationPeer
    public var sourceVmId: UInt16
    public var targetVmId: UInt16
    public var sourceOwner: String
    public var targetOwner: String
    public var sourceMachineSha256: String
    public var sourceConfigSha256: String
    public var name: String
    public var diskMib: UInt64
    public var memoryMib: UInt64
    public var minMemoryMib: UInt64
    public var cores: UInt16
    public var guest: GuestSpec?
    public var byteCeiling: UInt64
    public var targetBridge: String
    enum CodingKeys: String, CodingKey {
        case schema, source, destination, name, cores, guest
        case migrationId = "migration_id", sourceVmId = "source_vm_id", targetVmId = "target_vm_id"
        case sourceOwner = "source_owner", targetOwner = "target_owner", sourceMachineSha256 = "source_machine_sha256"
        case sourceConfigSha256 = "source_config_sha256", diskMib = "disk_mib", memoryMib = "memory_mib"
        case minMemoryMib = "min_memory_mib", byteCeiling = "byte_ceiling", targetBridge = "target_bridge"
    }
}

public struct MigrationGrant: Codable, Hashable, Sendable {
    public var schema: UInt32
    public var plan: MigrationPlan
    public var role: String
    public var issuer: MigrationPeer
    public var recipient: MigrationPeer
    public var actor: String
    public var nonce: String
    public var sourceGrantSha256: String?
    public var issuedAt: UInt64
    public var expiresAt: UInt64
    enum CodingKeys: String, CodingKey {
        case schema, plan, role, issuer, recipient, actor, nonce
        case sourceGrantSha256 = "source_grant_sha256"
        case issuedAt = "issued_at", expiresAt = "expires_at"
    }
}

public struct SignedMigrationGrant: Codable, Hashable, Sendable {
    public var value: MigrationGrant
    public var signature: String
    public func isFresh(now: Date = Date()) -> Bool {
        let interval = now.timeIntervalSince1970
        guard interval.isFinite, interval >= 0, interval < Double(UInt64.max) else { return false }
        let seconds = UInt64(interval)
        return !signature.isEmpty && value.expiresAt > seconds && value.expiresAt > value.issuedAt
            && value.expiresAt - value.issuedAt <= 86400
    }
}

public struct MigrationAbortReceipt: Codable, Hashable, Sendable {
    public var migrationId: String
    public var planSha256: String
    public var source: MigrationPeer
    public var abortedAt: UInt64
    enum CodingKeys: String, CodingKey {
        case source
        case migrationId = "migration_id", planSha256 = "plan_sha256", abortedAt = "aborted_at"
    }
}
public struct SignedMigrationAbort: Codable, Hashable, Sendable {
    public var value: MigrationAbortReceipt
    public var signature: String
    public func matches(_ plan: MigrationPlan) -> Bool {
        !signature.isEmpty && value.migrationId == plan.migrationId && value.source == plan.source
    }
}

public struct MigrationTransferRecord: Codable, Hashable, Sendable, Identifiable {
    public var plan: MigrationPlan
    public var role: String
    public var phase: String
    public var sourceGrant: SignedMigrationGrant
    public var destinationGrant: SignedMigrationGrant?
    public var offsets: [String: UInt64]
    public var handoff: WireJSONValue?
    public var acceptance: WireJSONValue?
    public var abortProof: SignedMigrationAbort?
    public var createdAt: UInt64
    public var updatedAt: UInt64
    public var error: String?
    public var id: String { plan.migrationId }
    public var transferredBytes: UInt64? {
        var total: UInt64 = 0
        for amount in offsets.values {
            let (sum, overflow) = total.addingReportingOverflow(amount)
            guard !overflow else { return nil }
            total = sum
        }
        return total
    }
    public var isTerminal: Bool { phase == "complete" || phase == "aborted" }
    public var mayAbort: Bool { handoff == nil && ["authorized", "preparing", "fenced", "receiving", "ready", "blocked"].contains(phase) }
    public var mayFinish: Bool { role == "source" && handoff != nil && acceptance != nil && ["relinquished", "accepted", "cleaning"].contains(phase) }
    public var mayRenew: Bool { role == "source" && handoff == nil && !isTerminal && ["authorized", "preparing", "fenced", "receiving", "ready", "blocked"].contains(phase) }
    enum CodingKeys: String, CodingKey {
        case plan, role, phase, offsets, handoff, acceptance, error
        case abortProof = "abort_proof"
        case sourceGrant = "source_grant", destinationGrant = "destination_grant"
        case createdAt = "created_at", updatedAt = "updated_at"
    }
}

public enum DestinationCleanupState: Sendable, Equatable {
    case pending, verified

    public static func review(_ record: MigrationTransferRecord, plan: MigrationPlan,
                              sourceProof: SignedMigrationAbort) -> Self? {
        guard sourceProof.matches(plan), record.role == "destination", record.plan == plan,
              record.abortProof == sourceProof else { return nil }
        switch record.phase {
        case "cleaning": return .pending
        case "aborted": return .verified
        default: return nil
        }
    }
}

public enum MigrationAdminRequest: Codable, Hashable, Sendable {
    case describe
    case plan(requestId: UUID, vmId: UInt16, destination: MigrationDescriptor, targetOwner: String, targetVmId: UInt16, expiresInSeconds: UInt64)
    case authorizeDestination(sourceGrant: SignedMigrationGrant, expiresInSeconds: UInt64)
    case start(migrationId: String, destinationGrant: SignedMigrationGrant)
    case status(migrationId: String)
    case list
    case abort(migrationId: String, confirmCleanup: Bool, sourceAbort: SignedMigrationAbort? = nil)
    case renew(requestId: UUID, migrationId: String, expiresInSeconds: UInt64, sourceGrant: SignedMigrationGrant? = nil)
    case finish(migrationId: String, confirmGuestVerified: Bool, confirmDeleteRecovery: Bool)

    public var isMutation: Bool {
        switch self { case .describe, .status, .list: false; default: true }
    }
    public var requestId: UUID? {
        switch self { case .plan(let value, _, _, _, _, _), .renew(let value, _, _, _): value; default: nil }
    }
    public var vmId: UInt16? { if case .plan(_, let value, _, _, _, _) = self { value } else { nil } }
    public var withoutRequestId: Self {
        let zero = UUID(uuid: (0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0))
        switch self {
        case .plan(_, let vm, let destination, let owner, let target, let expiry):
            return .plan(requestId: zero, vmId: vm, destination: destination, targetOwner: owner, targetVmId: target, expiresInSeconds: expiry)
        case .renew(_, let id, let expiry, let source): return .renew(requestId: zero, migrationId: id, expiresInSeconds: expiry, sourceGrant: source)
        default: return self
        }
    }
    public var summary: String {
        switch self {
        case .describe: "Describe migration endpoint"
        case .plan(_, let vm, _, _, _, _): "Authorize migration of VM \(vm) at source"
        case .authorizeDestination(let grant, _): "Authorize destination for VM \(grant.value.plan.targetVmId)"
        case .start: "Start reviewed VM migration"
        case .status: "Load migration status"
        case .list: "Load migrations"
        case .abort: "Abort uncommitted VM migration"
        case .renew: "Renew authorization for the same VM transfer"
        case .finish: "Finish verified migration and remove source recovery copy"
        }
    }
    enum CodingKeys: String, CodingKey {
        case kind, destination
        case requestId = "request_id", vmId = "vm_id", targetOwner = "target_owner", targetVmId = "target_vm_id"
        case expiresInSeconds = "expires_in_seconds", sourceGrant = "source_grant", destinationGrant = "destination_grant"
        case migrationId = "migration_id", confirmCleanup = "confirm_cleanup", confirmGuestVerified = "confirm_guest_verified"
        case confirmDeleteRecovery = "confirm_delete_recovery", sourceAbort = "source_abort"
    }
    public func encode(to encoder: any Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .describe: try c.encode("describe", forKey: .kind)
        case .list: try c.encode("list", forKey: .kind)
        case .status(let id): try c.encode("status", forKey: .kind); try c.encode(id, forKey: .migrationId)
        case .plan(let id, let vm, let destination, let owner, let target, let expiry):
            try c.encode("plan", forKey: .kind); try c.encode(WireUUID.string(id), forKey: .requestId)
            try c.encode(vm, forKey: .vmId); try c.encode(destination, forKey: .destination)
            try c.encode(owner, forKey: .targetOwner); try c.encode(target, forKey: .targetVmId); try c.encode(expiry, forKey: .expiresInSeconds)
        case .authorizeDestination(let grant, let expiry):
            try c.encode("authorize_destination", forKey: .kind); try c.encode(grant, forKey: .sourceGrant)
            try c.encode(expiry, forKey: .expiresInSeconds)
        case .start(let id, let grant):
            try c.encode("start", forKey: .kind); try c.encode(id, forKey: .migrationId); try c.encode(grant, forKey: .destinationGrant)
        case .abort(let id, let confirm, let sourceAbort):
            try c.encode("abort", forKey: .kind); try c.encode(id, forKey: .migrationId); try c.encode(confirm, forKey: .confirmCleanup)
            try c.encodeIfPresent(sourceAbort, forKey: .sourceAbort)
        case .renew(let requestId, let id, let expiry, let grant):
            try c.encode("renew", forKey: .kind); try c.encode(WireUUID.string(requestId), forKey: .requestId)
            try c.encode(id, forKey: .migrationId); try c.encode(expiry, forKey: .expiresInSeconds)
            try c.encodeIfPresent(grant, forKey: .sourceGrant)
        case .finish(let id, let verified, let remove):
            try c.encode("finish", forKey: .kind); try c.encode(id, forKey: .migrationId)
            try c.encode(verified, forKey: .confirmGuestVerified); try c.encode(remove, forKey: .confirmDeleteRecovery)
        }
    }
    public init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        switch try c.decode(String.self, forKey: .kind) {
        case "describe": self = .describe
        case "list": self = .list
        case "status": self = .status(migrationId: try c.decode(String.self, forKey: .migrationId))
        case "plan":
            let raw = try c.decode(String.self, forKey: .requestId)
            guard let id = UUID(uuidString: raw) else { throw DecodingError.dataCorruptedError(forKey: .requestId, in: c, debugDescription: "Invalid request ID") }
            self = .plan(requestId: id, vmId: try c.decode(UInt16.self, forKey: .vmId), destination: try c.decode(MigrationDescriptor.self, forKey: .destination),
                         targetOwner: try c.decode(String.self, forKey: .targetOwner), targetVmId: try c.decode(UInt16.self, forKey: .targetVmId), expiresInSeconds: try c.decode(UInt64.self, forKey: .expiresInSeconds))
        case "authorize_destination": self = .authorizeDestination(sourceGrant: try c.decode(SignedMigrationGrant.self, forKey: .sourceGrant), expiresInSeconds: try c.decode(UInt64.self, forKey: .expiresInSeconds))
        case "start": self = .start(migrationId: try c.decode(String.self, forKey: .migrationId), destinationGrant: try c.decode(SignedMigrationGrant.self, forKey: .destinationGrant))
        case "abort": self = .abort(migrationId: try c.decode(String.self, forKey: .migrationId), confirmCleanup: try c.decode(Bool.self, forKey: .confirmCleanup), sourceAbort: try c.decodeIfPresent(SignedMigrationAbort.self, forKey: .sourceAbort))
        case "renew":
            let raw = try c.decode(String.self, forKey: .requestId)
            guard let id = UUID(uuidString: raw) else { throw DecodingError.dataCorruptedError(forKey: .requestId, in: c, debugDescription: "Invalid renewal request ID") }
            self = .renew(requestId: id, migrationId: try c.decode(String.self, forKey: .migrationId), expiresInSeconds: try c.decode(UInt64.self, forKey: .expiresInSeconds), sourceGrant: try c.decodeIfPresent(SignedMigrationGrant.self, forKey: .sourceGrant))
        case "finish": self = .finish(migrationId: try c.decode(String.self, forKey: .migrationId), confirmGuestVerified: try c.decode(Bool.self, forKey: .confirmGuestVerified), confirmDeleteRecovery: try c.decode(Bool.self, forKey: .confirmDeleteRecovery))
        default: throw DecodingError.dataCorruptedError(forKey: .kind, in: c, debugDescription: "Unsupported migration action")
        }
    }
}

public enum MigrationReview {
    private static func isDigest(_ value: String) -> Bool {
        value.utf8.count == 64 && value.utf8.allSatisfy { (48...57).contains($0) || (97...102).contains($0) }
    }

    public static func sourceGrant(_ grant: SignedMigrationGrant, matches vm: Vm, source: ServerIdentity,
                                   destination: ServerIdentity, targetVmId: UInt16, targetOwner: String) -> Bool {
        let value = grant.value, plan = value.plan
        return !grant.signature.isEmpty && value.schema == 1 && plan.schema == 1 && value.role == "source"
            && value.actor == source.deviceId && isDigest(value.nonce) && value.sourceGrantSha256 == nil && value.issuer == plan.source && value.recipient == plan.destination
            && plan.source.matches(source) && plan.destination.matches(destination)
            && source.serverId != destination.serverId && source.environment != destination.environment
            && plan.sourceVmId == vm.vmId && plan.targetVmId == targetVmId
            && plan.sourceOwner == vm.owner && plan.targetOwner == targetOwner
            && plan.name == vm.name && plan.diskMib == vm.diskMib && plan.memoryMib == vm.memoryMib
            && plan.minMemoryMib == vm.minMemoryMib && plan.cores == vm.cores && plan.guest == vm.guest
            && value.expiresAt > value.issuedAt && value.expiresAt - value.issuedAt <= 86400
    }

    public static func destinationGrant(_ grant: SignedMigrationGrant, matches sourceGrant: SignedMigrationGrant,
                                        destination: ServerIdentity) -> Bool {
        let value = grant.value
        return !grant.signature.isEmpty && value.schema == 1 && value.role == "destination"
            && value.actor == destination.deviceId && isDigest(value.nonce) && value.sourceGrantSha256.map(isDigest) == true
            && value.plan == sourceGrant.value.plan
            && value.issuer == value.plan.destination && value.recipient == value.plan.source
            && value.issuer.matches(destination) && value.expiresAt > value.issuedAt
            && value.expiresAt - value.issuedAt <= 86400
    }
}
