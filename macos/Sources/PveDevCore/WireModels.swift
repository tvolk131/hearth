import Foundation

public struct UserRole: RawRepresentable, Codable, Hashable, Sendable {
    public let rawValue: String
    public init(rawValue: String) { self.rawValue = rawValue }

    public static let admin = UserRole(rawValue: "admin")
    public static let member = UserRole(rawValue: "member")

    public var isAdmin: Bool { self == .admin }
    public var isRecognized: Bool { self == .admin || self == .member }
}

public struct VmState: RawRepresentable, Codable, Hashable, Sendable {
    public let rawValue: String
    public init(rawValue: String) { self.rawValue = rawValue }

    public static let creating = VmState(rawValue: "creating")
    public static let stopped = VmState(rawValue: "stopped")
    public static let running = VmState(rawValue: "running")
    public static let deleting = VmState(rawValue: "deleting")
    public static let deleted = VmState(rawValue: "deleted")
    public static let blocked = VmState(rawValue: "blocked")

    public static let known: [VmState] = [.creating, .stopped, .running, .deleting, .deleted, .blocked]
    public var isRecognized: Bool { Self.known.contains(self) }
    public var isTransitional: Bool { self == .creating || self == .deleting }
}

public struct OperationStatus: RawRepresentable, Codable, Hashable, Sendable {
    public let rawValue: String
    public init(rawValue: String) { self.rawValue = rawValue }

    public static let queued = OperationStatus(rawValue: "queued")
    public static let running = OperationStatus(rawValue: "running")
    public static let verified = OperationStatus(rawValue: "verified")
    public static let blocked = OperationStatus(rawValue: "blocked")
    public static let cancelled = OperationStatus(rawValue: "cancelled")

    public static let known: [OperationStatus] = [.queued, .running, .verified, .blocked, .cancelled]
    public var isRecognized: Bool { Self.known.contains(self) }
    public var isActive: Bool { self == .queued || self == .running }
    public var isTerminal: Bool { self == .verified || self == .cancelled }
}

public struct BackendKind: RawRepresentable, Codable, Hashable, Sendable {
    public let rawValue: String
    public init(rawValue: String) { self.rawValue = rawValue }

    public static let lab = BackendKind(rawValue: "lab")
    public static let simulated = BackendKind(rawValue: "simulated")
    public static let physicalTrial = BackendKind(rawValue: "physical_trial")
    public static let managed = BackendKind(rawValue: "managed")
    public var isRecognized: Bool { [.lab, .simulated, .physicalTrial, .managed].contains(self) }
}

public struct UserState: RawRepresentable, Codable, Hashable, Sendable {
    public let rawValue: String
    public init(rawValue: String) { self.rawValue = rawValue }
    public static let active = UserState(rawValue: "active")
    public static let deleting = UserState(rawValue: "deleting")
    public static let deleted = UserState(rawValue: "deleted")
}

public struct User: Codable, Hashable, Sendable, Identifiable {
    public var userId: String
    public var role: UserRole
    public var budgetMib: UInt64
    public var minMemoryBudgetMib: UInt64
    public var maxMemoryBudgetMib: UInt64
    public var maxCores: UInt16
    public var maxVms: UInt16
    public var ready: Bool
    public var policyRevision: UInt64
    public var state: UserState

    public var id: String { userId }

    public init(
        userId: String, role: UserRole, budgetMib: UInt64, minMemoryBudgetMib: UInt64,
        maxMemoryBudgetMib: UInt64, maxCores: UInt16, maxVms: UInt16, ready: Bool, policyRevision: UInt64 = 0,
        state: UserState = .active
    ) {
        self.userId = userId
        self.role = role
        self.budgetMib = budgetMib
        self.minMemoryBudgetMib = minMemoryBudgetMib
        self.maxMemoryBudgetMib = maxMemoryBudgetMib
        self.maxCores = maxCores
        self.maxVms = maxVms
        self.ready = ready
        self.policyRevision = policyRevision
        self.state = state
    }

    public init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        self.init(userId: try c.decode(String.self, forKey: .userId), role: try c.decode(UserRole.self, forKey: .role),
            budgetMib: try c.decode(UInt64.self, forKey: .budgetMib), minMemoryBudgetMib: try c.decode(UInt64.self, forKey: .minMemoryBudgetMib),
            maxMemoryBudgetMib: try c.decode(UInt64.self, forKey: .maxMemoryBudgetMib), maxCores: try c.decode(UInt16.self, forKey: .maxCores),
            maxVms: try c.decode(UInt16.self, forKey: .maxVms), ready: try c.decode(Bool.self, forKey: .ready),
            policyRevision: try c.decodeIfPresent(UInt64.self, forKey: .policyRevision) ?? 0,
            state: try c.decodeIfPresent(UserState.self, forKey: .state) ?? .active)
    }

    enum CodingKeys: String, CodingKey {
        case userId = "user_id"
        case role
        case budgetMib = "budget_mib"
        case minMemoryBudgetMib = "min_memory_budget_mib"
        case maxMemoryBudgetMib = "max_memory_budget_mib"
        case maxCores = "max_cores"
        case maxVms = "max_vms"
        case ready
        case policyRevision = "policy_revision"
        case state
    }
}

public struct Budget: Codable, Hashable, Sendable {
    public var budgetMib: UInt64
    public var allocatedMib: UInt64
    public var remainingMib: UInt64
    public var minMemoryAllocatedMib: UInt64
    public var memoryAllocatedMib: UInt64
    public var coresAllocated: UInt64

    public init(
        budgetMib: UInt64, allocatedMib: UInt64, remainingMib: UInt64,
        minMemoryAllocatedMib: UInt64, memoryAllocatedMib: UInt64, coresAllocated: UInt64
    ) {
        self.budgetMib = budgetMib
        self.allocatedMib = allocatedMib
        self.remainingMib = remainingMib
        self.minMemoryAllocatedMib = minMemoryAllocatedMib
        self.memoryAllocatedMib = memoryAllocatedMib
        self.coresAllocated = coresAllocated
    }

    enum CodingKeys: String, CodingKey {
        case budgetMib = "budget_mib"
        case allocatedMib = "allocated_mib"
        case remainingMib = "remaining_mib"
        case minMemoryAllocatedMib = "min_memory_allocated_mib"
        case memoryAllocatedMib = "memory_allocated_mib"
        case coresAllocated = "cores_allocated"
    }
}

public struct GuestSpec: Codable, Hashable, Sendable {
    public var imageSha256: String
    public var sshPublicKey: String
    public init(imageSha256: String, sshPublicKey: String) {
        self.imageSha256 = imageSha256; self.sshPublicKey = sshPublicKey
    }
    enum CodingKeys: String, CodingKey { case imageSha256 = "image_sha256", sshPublicKey = "ssh_public_key" }
}

public struct Vm: Codable, Hashable, Sendable, Identifiable {
    public var vmId: UInt16
    public var owner: String
    public var name: String
    public var diskMib: UInt64
    public var memoryMib: UInt64
    public var minMemoryMib: UInt64
    public var cores: UInt16
    public var state: VmState
    public var guest: GuestSpec?

    public var id: UInt16 { vmId }

    public init(
        vmId: UInt16, owner: String, name: String, diskMib: UInt64, memoryMib: UInt64,
        minMemoryMib: UInt64, cores: UInt16, state: VmState, guest: GuestSpec? = nil
    ) {
        self.vmId = vmId
        self.owner = owner
        self.name = name
        self.diskMib = diskMib
        self.memoryMib = memoryMib
        self.minMemoryMib = minMemoryMib
        self.cores = cores
        self.state = state
        self.guest = guest
    }

    enum CodingKeys: String, CodingKey {
        case vmId = "vm_id"
        case owner
        case name
        case diskMib = "disk_mib"
        case memoryMib = "memory_mib"
        case minMemoryMib = "min_memory_mib"
        case cores
        case state
        case guest
    }
}

public struct OperationRecord: Codable, Hashable, Sendable, Identifiable {
    public var operationId: String
    public var owner: String
    public var requestId: String
    public var kind: String
    public var vmId: UInt16?
    public var status: OperationStatus
    public var step: String
    public var error: String?
    public var createdAt: UInt64
    public var progress: OperationProgress?

    public var id: String { operationId }
    public var createdDate: Date { Date(timeIntervalSince1970: TimeInterval(createdAt)) }

    public init(
        operationId: String, owner: String, requestId: String, kind: String, vmId: UInt16?,
        status: OperationStatus, step: String, error: String?, createdAt: UInt64,
        progress: OperationProgress? = nil
    ) {
        self.operationId = operationId
        self.owner = owner
        self.requestId = requestId
        self.kind = kind
        self.vmId = vmId
        self.status = status
        self.step = step
        self.error = error
        self.createdAt = createdAt
        self.progress = progress
    }

    enum CodingKeys: String, CodingKey {
        case operationId = "operation_id"
        case owner
        case requestId = "request_id"
        case kind
        case vmId = "vm_id"
        case status
        case step
        case error
        case createdAt = "created_at"
        case progress
    }
}

public struct OperationProgress: Codable, Hashable, Sendable {
    public var startedAt: UInt64?
    public var updatedAt: UInt64
    public var finishedAt: UInt64?
    public var completedSteps: UInt32
    public var totalSteps: UInt32
    public var detail: String?

    public init(startedAt: UInt64?, updatedAt: UInt64, finishedAt: UInt64?, completedSteps: UInt32,
                totalSteps: UInt32, detail: String?) {
        self.startedAt = startedAt; self.updatedAt = updatedAt; self.finishedAt = finishedAt
        self.completedSteps = completedSteps; self.totalSteps = totalSteps; self.detail = detail
    }

    public var fractionCompleted: Double? {
        guard totalSteps > 0, completedSteps <= totalSteps else { return nil }
        return Double(completedSteps) / Double(totalSteps)
    }

    public func elapsedSeconds(now: Date = Date()) -> UInt64? {
        guard let startedAt, startedAt > 0 else { return nil }
        let end: UInt64
        if let finishedAt { end = finishedAt }
        else {
            let interval = now.timeIntervalSince1970
            guard interval.isFinite, interval >= 0, interval < Double(UInt64.max) else { return nil }
            end = UInt64(interval)
        }
        guard end >= startedAt else { return nil }
        return end - startedAt
    }

    enum CodingKeys: String, CodingKey {
        case startedAt = "started_at", updatedAt = "updated_at", finishedAt = "finished_at"
        case completedSteps = "completed_steps", totalSteps = "total_steps", detail
    }
}

public struct DashboardResult: Codable, Hashable, Sendable {
    public var identity: User
    public var users: [User]
    public var budget: Budget
    public var vms: [Vm]
    public var operations: [OperationRecord]
    public var environment: String
    public var backend: BackendKind
    public var diskShrinks: [PendingDiskShrink]
    public var capabilities: [String]
    public var importedVms: [ImportedVm]
    public var storageAllocations: [StorageAllocation]

    public init(
        identity: User, users: [User], budget: Budget, vms: [Vm], operations: [OperationRecord],
        environment: String, backend: BackendKind, diskShrinks: [PendingDiskShrink] = [], capabilities: [String] = [], importedVms: [ImportedVm] = [], storageAllocations: [StorageAllocation] = []
    ) {
        self.identity = identity
        self.users = users
        self.budget = budget
        self.vms = vms
        self.operations = operations
        self.environment = environment
        self.backend = backend
        self.diskShrinks = diskShrinks
        self.capabilities = capabilities
        self.importedVms = importedVms
        self.storageAllocations = storageAllocations
    }

    enum CodingKeys: String, CodingKey { case identity, users, budget, vms, operations, environment, backend, capabilities; case diskShrinks = "disk_shrinks", importedVms = "imported_vms", storageAllocations = "storage_allocations" }
    public init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        self.init(identity: try c.decode(User.self, forKey: .identity), users: try c.decode([User].self, forKey: .users),
            budget: try c.decode(Budget.self, forKey: .budget), vms: try c.decode([Vm].self, forKey: .vms),
            operations: try c.decode([OperationRecord].self, forKey: .operations), environment: try c.decode(String.self, forKey: .environment),
            backend: try c.decode(BackendKind.self, forKey: .backend), diskShrinks: try c.decodeIfPresent([PendingDiskShrink].self, forKey: .diskShrinks) ?? [], capabilities: try c.decodeIfPresent([String].self, forKey: .capabilities) ?? [],
            importedVms: try c.decodeIfPresent([ImportedVm].self, forKey: .importedVms) ?? [],
            storageAllocations: try c.decodeIfPresent([StorageAllocation].self, forKey: .storageAllocations) ?? [])
    }
}

extension DashboardResult {
    public func supports(_ action: String, legacyLabFallback: Bool = false) -> Bool {
        guard backend.isRecognized else { return false }
        if backend == .managed {
            let capability = Self.managedCapabilityByAction[action] ?? action
            return Self.managedCapabilities.contains(capability) && capabilities.contains(capability)
        }
        return capabilities.contains(action) || (legacyLabFallback && capabilities.isEmpty && (backend == .lab || backend == .simulated))
    }
}

public struct DiskShrinkSpec: Codable, Hashable, Sendable {
    public var previousDiskMib: UInt64
    public var targetDiskMib: UInt64
    public var preparationId: String?
    public var rootPartitionUuid: String?
    enum CodingKeys: String, CodingKey { case previousDiskMib = "previous_disk_mib", targetDiskMib = "target_disk_mib", preparationId = "preparation_id", rootPartitionUuid = "root_partition_uuid" }
    public init(previousDiskMib: UInt64, targetDiskMib: UInt64, preparationId: String? = nil, rootPartitionUuid: String? = nil) {
        self.previousDiskMib = previousDiskMib; self.targetDiskMib = targetDiskMib; self.preparationId = preparationId; self.rootPartitionUuid = rootPartitionUuid
    }
    public var workspaceMib: UInt64 {
        guard targetDiskMib >= 2048, targetDiskMib < previousDiskMib else { return 0 }
        let sourceOverhead = previousDiskMib / 20 + (previousDiskMib % 20 == 0 ? 0 : 1)
        let targetOverhead = targetDiskMib / 20 + (targetDiskMib % 20 == 0 ? 0 : 1)
        var result: UInt64 = 0
        for component in [previousDiskMib, sourceOverhead, targetDiskMib, targetOverhead, 64] {
            let (sum, overflow) = result.addingReportingOverflow(component)
            guard !overflow else { return 0 }
            result = sum
        }
        return result
    }
}

public struct PendingDiskShrink: Codable, Hashable, Sendable {
    public var operationId: String
    public var vmId: UInt16
    public var owner: String
    public var spec: DiskShrinkSpec
    public var prepared: Bool
    enum CodingKeys: String, CodingKey { case operationId = "operation_id", vmId = "vm_id", owner, spec, prepared }
}

public struct UpdatedResult: Codable, Hashable, Sendable {
    public var updated: Bool
    public init(updated: Bool) { self.updated = updated }
}
