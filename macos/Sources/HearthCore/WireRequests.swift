import Foundation

public enum VmActionKind: String, Codable, Hashable, Sendable, CaseIterable {
    case start
    case shutdown
    case stop
    case delete
}

public struct CreateVmRequest: Hashable, Sendable {
    public var requestId: UUID
    public var name: String
    public var diskMib: UInt64
    public var memoryMib: UInt64
    public var minMemoryMib: UInt64
    public var cores: UInt16
    public var owner: String?
    public var guest: GuestSpec?

    public init(
        requestId: UUID, name: String, diskMib: UInt64, memoryMib: UInt64, minMemoryMib: UInt64,
        cores: UInt16, owner: String?, guest: GuestSpec? = nil
    ) {
        self.requestId = requestId
        self.name = name
        self.diskMib = diskMib
        self.memoryMib = memoryMib
        self.minMemoryMib = minMemoryMib
        self.cores = cores
        self.owner = owner
        self.guest = guest
    }
}

public struct VmActionRequest: Hashable, Sendable {
    public var requestId: UUID
    public var vmId: UInt16
    public var kind: VmActionKind

    public init(requestId: UUID, vmId: UInt16, kind: VmActionKind) {
        self.requestId = requestId
        self.vmId = vmId
        self.kind = kind
    }
}

public struct CreateUserRequest: Hashable, Sendable {
    public var requestId: UUID
    public var userId: String
    public var budgetMib: UInt64
    public var minMemoryBudgetMib: UInt64
    public var maxMemoryBudgetMib: UInt64
    public var maxCores: UInt16
    public var maxVms: UInt16

    public init(
        requestId: UUID, userId: String, budgetMib: UInt64, minMemoryBudgetMib: UInt64,
        maxMemoryBudgetMib: UInt64, maxCores: UInt16, maxVms: UInt16
    ) {
        self.requestId = requestId
        self.userId = userId
        self.budgetMib = budgetMib
        self.minMemoryBudgetMib = minMemoryBudgetMib
        self.maxMemoryBudgetMib = maxMemoryBudgetMib
        self.maxCores = maxCores
        self.maxVms = maxVms
    }
}

public struct EnrollDeviceRequest: Hashable, Sendable {
    public var userId: String
    public var endpointId: String
    public var label: String

    public init(userId: String, endpointId: String, label: String) {
        self.userId = userId
        self.endpointId = endpointId
        self.label = label
    }
}

public struct RevokeDeviceRequest: Hashable, Sendable {
    public var endpointId: String

    public init(endpointId: String) {
        self.endpointId = endpointId
    }
}

public struct ResourcePolicy: Codable, Hashable, Sendable {
    public var budgetMib: UInt64
    public var minMemoryBudgetMib: UInt64
    public var maxMemoryBudgetMib: UInt64
    public var maxCores: UInt16
    public var maxVms: UInt16
    public init(budgetMib: UInt64, minMemoryBudgetMib: UInt64, maxMemoryBudgetMib: UInt64, maxCores: UInt16, maxVms: UInt16) {
        self.budgetMib = budgetMib; self.minMemoryBudgetMib = minMemoryBudgetMib; self.maxMemoryBudgetMib = maxMemoryBudgetMib
        self.maxCores = maxCores; self.maxVms = maxVms
    }
    enum CodingKeys: String, CodingKey {
        case budgetMib = "budget_mib", minMemoryBudgetMib = "min_memory_budget_mib", maxMemoryBudgetMib = "max_memory_budget_mib"
        case maxCores = "max_cores", maxVms = "max_vms"
    }
}

public struct UpdateUserPolicyRequest: Hashable, Sendable {
    public var requestId: UUID
    public var userId: String
    public var expectedRevision: UInt64
    public var policy: ResourcePolicy
    public init(requestId: UUID, userId: String, expectedRevision: UInt64, policy: ResourcePolicy) {
        self.requestId = requestId; self.userId = userId; self.expectedRevision = expectedRevision; self.policy = policy
    }
}

public struct DeleteUserRequest: Hashable, Sendable {
    public var requestId: UUID
    public var userId: String
    public var expectedRevision: UInt64
    public init(requestId: UUID, userId: String, expectedRevision: UInt64) {
        self.requestId = requestId; self.userId = userId; self.expectedRevision = expectedRevision
    }
}

public struct ShrinkVmRequest: Hashable, Sendable {
    public var requestId: UUID
    public var vmId: UInt16
    public var expectedDiskMib: UInt64
    public var targetDiskMib: UInt64
    public var confirmDowntime: Bool
    public var rootPartitionUuid: String?
    public init(requestId: UUID, vmId: UInt16, expectedDiskMib: UInt64, targetDiskMib: UInt64, confirmDowntime: Bool, rootPartitionUuid: String? = nil) {
        self.requestId = requestId; self.vmId = vmId; self.expectedDiskMib = expectedDiskMib
        self.targetDiskMib = targetDiskMib; self.confirmDowntime = confirmDowntime; self.rootPartitionUuid = rootPartitionUuid
    }
}

public struct FinishVmShrinkRequest: Hashable, Sendable {
    public var requestId: UUID
    public var vmId: UInt16
    public var preparationId: String
    public var confirmGuestVerified: Bool
    public var confirmDeleteRecovery: Bool
    public init(requestId: UUID, vmId: UInt16, preparationId: String, confirmGuestVerified: Bool, confirmDeleteRecovery: Bool) {
        self.requestId = requestId; self.vmId = vmId; self.preparationId = preparationId
        self.confirmGuestVerified = confirmGuestVerified; self.confirmDeleteRecovery = confirmDeleteRecovery
    }
}

public enum RpcRequest: Hashable, Sendable {
    case dashboard
    case hostInventory
    case listImageCatalog
    case inspectOperation(operationId: String)
    case recoverOperation(RecoverOperationRequest)
    case planVmGrowth(vmId: UInt16, targetDiskMib: UInt64, rootPartitionUuid: String? = nil)
    case growVm(GrowVmRequest)
    case planVmShrink(vmId: UInt16, targetDiskMib: UInt64, rootPartitionUuid: String? = nil)
    case planVmEnrollment(vmId: UInt16, owner: String)
    case enrollVm(EnrollVmRequest)
    case migration(MigrationAdminRequest)
    case shrinkVm(ShrinkVmRequest)
    case finishVmShrink(FinishVmShrinkRequest)
    case createVm(CreateVmRequest)
    case vmAction(VmActionRequest)
    case createUser(CreateUserRequest)
    case updateUserPolicy(UpdateUserPolicyRequest)
    case deleteUser(DeleteUserRequest)
    case enrollDevice(EnrollDeviceRequest)
    case revokeDevice(RevokeDeviceRequest)

    public var action: String {
        switch self {
        case .dashboard: "dashboard"
        case .hostInventory: "host_inventory"
        case .listImageCatalog: "list_image_catalog"
        case .inspectOperation: "inspect_operation"
        case .recoverOperation: "recover_operation"
        case .planVmGrowth: "plan_vm_growth"
        case .growVm: "grow_vm"
        case .planVmShrink: "plan_vm_shrink"
        case .planVmEnrollment: "plan_vm_enrollment"
        case .enrollVm: "enroll_vm"
        case .migration: "migration"
        case .shrinkVm: "shrink_vm"
        case .finishVmShrink: "finish_vm_shrink"
        case .createVm: "create_vm"
        case .vmAction: "vm_action"
        case .createUser: "create_user"
        case .updateUserPolicy: "update_user_policy"
        case .deleteUser: "delete_user"
        case .enrollDevice: "enroll_device"
        case .revokeDevice: "revoke_device"
        }
    }

    public var requestId: UUID? {
        switch self {
        case .dashboard, .hostInventory, .listImageCatalog, .inspectOperation, .planVmGrowth, .planVmShrink, .planVmEnrollment, .enrollDevice, .revokeDevice: nil
        case .recoverOperation(let value): value.requestId
        case .growVm(let value): value.requestId
        case .enrollVm(let value): value.requestId
        case .migration(let value): value.requestId
        case .shrinkVm(let value): value.requestId
        case .finishVmShrink(let value): value.requestId
        case .createVm(let value): value.requestId
        case .vmAction(let value): value.requestId
        case .createUser(let value): value.requestId
        case .updateUserPolicy(let value): value.requestId
        case .deleteUser(let value): value.requestId
        }
    }

    public var isMutation: Bool {
        switch self {
        case .dashboard, .hostInventory, .listImageCatalog, .inspectOperation, .planVmGrowth, .planVmShrink, .planVmEnrollment: false
        case .migration(let value): value.isMutation
        default: true
        }
    }

    public var requiresAdministrator: Bool {
        switch self {
        case .hostInventory, .migration: true
        case .createUser, .updateUserPolicy, .deleteUser, .enrollDevice, .revokeDevice, .shrinkVm, .finishVmShrink, .inspectOperation, .recoverOperation, .planVmGrowth, .growVm, .planVmShrink, .planVmEnrollment, .enrollVm: true
        default: false
        }
    }

    public var requiredCapability: String? {
        switch self {
        case .vmAction(let value) where value.kind == .shutdown: "shutdown"
        case .migration: "migration"
        case .inspectOperation: "inspect_operation"
        case .recoverOperation: "recover_operation"
        case .planVmGrowth: "plan_vm_growth"
        case .growVm: "grow_vm"
        case .planVmEnrollment: "plan_vm_enrollment"
        case .enrollVm: "enroll_vm"
        case .planVmShrink, .shrinkVm, .finishVmShrink: "shrink_vm"
        default: nil
        }
    }

    public var isDestructive: Bool {
        switch self {
        case .migration(let value):
            switch value { case .start, .abort, .finish: true; default: false }
        case .vmAction(let value): value.kind == .delete || value.kind == .stop || value.kind == .shutdown
        case .revokeDevice, .deleteUser, .shrinkVm, .finishVmShrink, .recoverOperation, .growVm, .enrollVm: true
        default: false
        }
    }

    public var expectsOperation: Bool {
        switch self {
        case .createVm, .vmAction, .createUser, .updateUserPolicy, .deleteUser, .shrinkVm, .finishVmShrink, .recoverOperation, .growVm, .enrollVm: true
        default: false
        }
    }

    public var withoutRequestId: RpcRequest {
        let zero = UUID(uuid: (0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0))
        switch self {
        case .migration(let value): return .migration(value.withoutRequestId)
        case .recoverOperation(var value): value.requestId = zero; return .recoverOperation(value)
        case .growVm(var value): value.requestId = zero; return .growVm(value)
        case .enrollVm(var value): value.requestId = zero; return .enrollVm(value)
        case .shrinkVm(var value): value.requestId = zero; return .shrinkVm(value)
        case .finishVmShrink(var value): value.requestId = zero; return .finishVmShrink(value)
        case .createVm(var value):
            value.requestId = zero
            return .createVm(value)
        case .vmAction(var value):
            value.requestId = zero
            return .vmAction(value)
        case .updateUserPolicy(var value):
            value.requestId = zero
            return .updateUserPolicy(value)
        case .deleteUser(var value):
            value.requestId = zero
            return .deleteUser(value)
        case .createUser(var value):
            value.requestId = zero
            return .createUser(value)
        default:
            return self
        }
    }

    public func hasSameIntent(as other: RpcRequest) -> Bool {
        withoutRequestId == other.withoutRequestId
    }

    public var summary: String {
        switch self {
        case .hostInventory: "Load server hardware and storage"
        case .dashboard:
            "Load dashboard"
        case .listImageCatalog: "Load approved boot images"
        case .inspectOperation(let operationId): "Inspect operation \(SafeText.sanitize(operationId, limit: 64))"
        case .recoverOperation(let value): "\(value.kind.title) for operation \(SafeText.sanitize(value.operationId, limit: 64))"
        case .planVmGrowth(let vmId, _, _): "Plan disk expansion for VM \(vmId)"
        case .growVm(let value): "Expand VM \(value.vmId) disk to \(Formatting.mib(value.targetDiskMib))"
        case .planVmShrink(let vmId, _, _): "Plan disk shrink for VM \(vmId)"
        case .planVmEnrollment(let vmId, _): "Plan enrollment of VM \(vmId)"
        case .enrollVm(let value): "Enroll VM \(value.vmId) for \(value.owner)"
        case .migration(let value): value.summary
        case .shrinkVm(let value): "Prepare VM \(value.vmId) disk shrink to \(Formatting.mib(value.targetDiskMib))"
        case .finishVmShrink(let value): "Finish VM \(value.vmId) shrink and remove recovery disk"
        case .createVm(let value):
            "Create VM \(value.name)" + (value.owner.map { " for \($0)" } ?? "")
        case .vmAction(let value):
            "\(value.kind.rawValue.capitalized) VM \(value.vmId)"
        case .createUser(let value):
            "Create user \(value.userId)"
        case .deleteUser(let value):
            "Delete empty user \(value.userId)"
        case .updateUserPolicy(let value):
            "Update budgets for \(value.userId) from revision \(value.expectedRevision)"
        case .enrollDevice(let value):
            "Enroll device \(value.label) for \(value.userId)"
        case .revokeDevice(let value):
            "Revoke device \(EndpointIdentifier.abbreviated(value.endpointId))"
        }
    }
}

public enum WireUUID {
    public static func string(_ uuid: UUID) -> String {
        uuid.uuidString.lowercased()
    }
}

extension RpcRequest: Codable {
    enum CodingKeys: String, CodingKey {
        case action, request
        case policy
        case expectedRevision = "expected_revision"
        case requestId = "request_id"
        case name
        case diskMib = "disk_mib"
        case memoryMib = "memory_mib"
        case minMemoryMib = "min_memory_mib"
        case cores
        case owner, guest
        case vmId = "vm_id"
        case kind
        case userId = "user_id"
        case budgetMib = "budget_mib"
        case minMemoryBudgetMib = "min_memory_budget_mib"
        case maxMemoryBudgetMib = "max_memory_budget_mib"
        case maxCores = "max_cores"
        case maxVms = "max_vms"
        case endpointId = "endpoint_id"
        case label
        case expectedDiskMib = "expected_disk_mib"
        case targetDiskMib = "target_disk_mib"
        case confirmDowntime = "confirm_downtime"
        case preparationId = "preparation_id"
        case confirmGuestVerified = "confirm_guest_verified"
        case confirmDeleteRecovery = "confirm_delete_recovery"
        case operationId = "operation_id", inspectionId = "inspection_id", confirmRecovery = "confirm_recovery"
        case planId = "plan_id", confirmEnrollment = "confirm_enrollment"
        case rootPartitionUuid = "root_partition_uuid"
    }

    public func encode(to encoder: any Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        try container.encode(action, forKey: .action)
        switch self {
        case .dashboard, .hostInventory, .listImageCatalog:
            break
        case .migration(let value): try container.encode(value, forKey: .request)
        case .inspectOperation(let operationId):
            try container.encode(operationId, forKey: .operationId)
        case .planVmEnrollment(let vmId, let owner):
            try container.encode(vmId, forKey: .vmId)
            try container.encode(owner, forKey: .owner)
        case .enrollVm(let value):
            try container.encode(WireUUID.string(value.requestId), forKey: .requestId)
            try container.encode(value.planId, forKey: .planId)
            try container.encode(value.vmId, forKey: .vmId)
            try container.encode(value.owner, forKey: .owner)
            try container.encode(value.confirmEnrollment, forKey: .confirmEnrollment)
        case .recoverOperation(let value):
            try container.encode(WireUUID.string(value.requestId), forKey: .requestId)
            try container.encode(value.operationId, forKey: .operationId)
            try container.encode(value.inspectionId, forKey: .inspectionId)
            try container.encode(value.kind, forKey: .kind)
            try container.encode(value.confirmRecovery, forKey: .confirmRecovery)
        case .planVmGrowth(let vmId, let targetDiskMib, let rootPartitionUuid), .planVmShrink(let vmId, let targetDiskMib, let rootPartitionUuid):
            try container.encode(vmId, forKey: .vmId)
            try container.encode(targetDiskMib, forKey: .targetDiskMib)
            try container.encodeIfPresent(rootPartitionUuid, forKey: .rootPartitionUuid)
        case .growVm(let value):
            try container.encode(WireUUID.string(value.requestId), forKey: .requestId)
            try container.encode(value.vmId, forKey: .vmId)
            try container.encode(value.expectedDiskMib, forKey: .expectedDiskMib)
            try container.encode(value.targetDiskMib, forKey: .targetDiskMib)
            try container.encode(value.confirmDowntime, forKey: .confirmDowntime)
            try container.encodeIfPresent(value.rootPartitionUuid, forKey: .rootPartitionUuid)
        case .shrinkVm(let value):
            try container.encode(WireUUID.string(value.requestId), forKey: .requestId)
            try container.encode(value.vmId, forKey: .vmId)
            try container.encode(value.expectedDiskMib, forKey: .expectedDiskMib)
            try container.encode(value.targetDiskMib, forKey: .targetDiskMib)
            try container.encode(value.confirmDowntime, forKey: .confirmDowntime)
            try container.encodeIfPresent(value.rootPartitionUuid, forKey: .rootPartitionUuid)
        case .finishVmShrink(let value):
            try container.encode(WireUUID.string(value.requestId), forKey: .requestId)
            try container.encode(value.vmId, forKey: .vmId)
            try container.encode(value.preparationId, forKey: .preparationId)
            try container.encode(value.confirmGuestVerified, forKey: .confirmGuestVerified)
            try container.encode(value.confirmDeleteRecovery, forKey: .confirmDeleteRecovery)
        case .createVm(let value):
            try container.encode(WireUUID.string(value.requestId), forKey: .requestId)
            try container.encode(value.name, forKey: .name)
            try container.encode(value.diskMib, forKey: .diskMib)
            try container.encode(value.memoryMib, forKey: .memoryMib)
            try container.encode(value.minMemoryMib, forKey: .minMemoryMib)
            try container.encode(value.cores, forKey: .cores)
            try container.encodeIfPresent(value.guest, forKey: .guest)
            if let owner = value.owner {
                try container.encode(owner, forKey: .owner)
            } else {
                try container.encodeNil(forKey: .owner)
            }
        case .vmAction(let value):
            try container.encode(WireUUID.string(value.requestId), forKey: .requestId)
            try container.encode(value.vmId, forKey: .vmId)
            try container.encode(value.kind, forKey: .kind)
        case .createUser(let value):
            try container.encode(WireUUID.string(value.requestId), forKey: .requestId)
            try container.encode(value.userId, forKey: .userId)
            try container.encode(value.budgetMib, forKey: .budgetMib)
            try container.encode(value.minMemoryBudgetMib, forKey: .minMemoryBudgetMib)
            try container.encode(value.maxMemoryBudgetMib, forKey: .maxMemoryBudgetMib)
            try container.encode(value.maxCores, forKey: .maxCores)
            try container.encode(value.maxVms, forKey: .maxVms)
        case .updateUserPolicy(let value):
            try container.encode(WireUUID.string(value.requestId), forKey: .requestId)
            try container.encode(value.userId, forKey: .userId)
            try container.encode(value.expectedRevision, forKey: .expectedRevision)
            try container.encode(value.policy, forKey: .policy)
        case .deleteUser(let value):
            try container.encode(WireUUID.string(value.requestId), forKey: .requestId)
            try container.encode(value.userId, forKey: .userId)
            try container.encode(value.expectedRevision, forKey: .expectedRevision)
        case .enrollDevice(let value):
            try container.encode(value.userId, forKey: .userId)
            try container.encode(value.endpointId, forKey: .endpointId)
            try container.encode(value.label, forKey: .label)
        case .revokeDevice(let value):
            try container.encode(value.endpointId, forKey: .endpointId)
        }
    }

    public init(from decoder: any Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        let action = try container.decode(String.self, forKey: .action)
        func requestId() throws -> UUID {
            let raw = try container.decode(String.self, forKey: .requestId)
            guard let uuid = UUID(uuidString: raw) else {
                throw DecodingError.dataCorruptedError(
                    forKey: .requestId, in: container, debugDescription: "request_id is not a UUID")
            }
            return uuid
        }
        switch action {
        case "migration": self = .migration(try container.decode(MigrationAdminRequest.self, forKey: .request))
        case "inspect_operation":
            self = .inspectOperation(operationId: try container.decode(String.self, forKey: .operationId))
        case "plan_vm_enrollment":
            self = .planVmEnrollment(vmId: try container.decode(UInt16.self, forKey: .vmId), owner: try container.decode(String.self, forKey: .owner))
        case "enroll_vm":
            self = .enrollVm(EnrollVmRequest(requestId: try requestId(), planId: try container.decode(String.self, forKey: .planId),
                vmId: try container.decode(UInt16.self, forKey: .vmId), owner: try container.decode(String.self, forKey: .owner),
                confirmEnrollment: try container.decode(Bool.self, forKey: .confirmEnrollment)))
        case "recover_operation":
            self = .recoverOperation(RecoverOperationRequest(requestId: try requestId(), operationId: try container.decode(String.self, forKey: .operationId),
                inspectionId: try container.decode(String.self, forKey: .inspectionId), kind: try container.decode(RecoveryAction.self, forKey: .kind),
                confirmRecovery: try container.decode(Bool.self, forKey: .confirmRecovery)))
        case "plan_vm_growth":
            self = .planVmGrowth(vmId: try container.decode(UInt16.self, forKey: .vmId), targetDiskMib: try container.decode(UInt64.self, forKey: .targetDiskMib), rootPartitionUuid: try container.decodeIfPresent(String.self, forKey: .rootPartitionUuid))
        case "plan_vm_shrink":
            self = .planVmShrink(vmId: try container.decode(UInt16.self, forKey: .vmId), targetDiskMib: try container.decode(UInt64.self, forKey: .targetDiskMib), rootPartitionUuid: try container.decodeIfPresent(String.self, forKey: .rootPartitionUuid))
        case "grow_vm":
            self = .growVm(GrowVmRequest(requestId: try requestId(), vmId: try container.decode(UInt16.self, forKey: .vmId),
                expectedDiskMib: try container.decode(UInt64.self, forKey: .expectedDiskMib), targetDiskMib: try container.decode(UInt64.self, forKey: .targetDiskMib),
                confirmDowntime: try container.decode(Bool.self, forKey: .confirmDowntime), rootPartitionUuid: try container.decodeIfPresent(String.self, forKey: .rootPartitionUuid)))
        case "shrink_vm":
            self = .shrinkVm(ShrinkVmRequest(requestId: try requestId(), vmId: try container.decode(UInt16.self, forKey: .vmId),
                expectedDiskMib: try container.decode(UInt64.self, forKey: .expectedDiskMib), targetDiskMib: try container.decode(UInt64.self, forKey: .targetDiskMib),
                confirmDowntime: try container.decode(Bool.self, forKey: .confirmDowntime), rootPartitionUuid: try container.decodeIfPresent(String.self, forKey: .rootPartitionUuid)))
        case "finish_vm_shrink":
            self = .finishVmShrink(FinishVmShrinkRequest(requestId: try requestId(), vmId: try container.decode(UInt16.self, forKey: .vmId),
                preparationId: try container.decode(String.self, forKey: .preparationId), confirmGuestVerified: try container.decode(Bool.self, forKey: .confirmGuestVerified),
                confirmDeleteRecovery: try container.decode(Bool.self, forKey: .confirmDeleteRecovery)))
        case "list_image_catalog": self = .listImageCatalog
        case "host_inventory": self = .hostInventory
        case "dashboard":
            self = .dashboard
        case "create_vm":
            self = .createVm(
                CreateVmRequest(
                    requestId: try requestId(),
                    name: try container.decode(String.self, forKey: .name),
                    diskMib: try container.decode(UInt64.self, forKey: .diskMib),
                    memoryMib: try container.decode(UInt64.self, forKey: .memoryMib),
                    minMemoryMib: try container.decode(UInt64.self, forKey: .minMemoryMib),
                    cores: try container.decode(UInt16.self, forKey: .cores),
                    owner: try container.decodeIfPresent(String.self, forKey: .owner),
                    guest: try container.decodeIfPresent(GuestSpec.self, forKey: .guest)))
        case "vm_action":
            self = .vmAction(
                VmActionRequest(
                    requestId: try requestId(),
                    vmId: try container.decode(UInt16.self, forKey: .vmId),
                    kind: try container.decode(VmActionKind.self, forKey: .kind)))
        case "create_user":
            self = .createUser(
                CreateUserRequest(
                    requestId: try requestId(),
                    userId: try container.decode(String.self, forKey: .userId),
                    budgetMib: try container.decode(UInt64.self, forKey: .budgetMib),
                    minMemoryBudgetMib: try container.decode(UInt64.self, forKey: .minMemoryBudgetMib),
                    maxMemoryBudgetMib: try container.decode(UInt64.self, forKey: .maxMemoryBudgetMib),
                    maxCores: try container.decode(UInt16.self, forKey: .maxCores),
                    maxVms: try container.decode(UInt16.self, forKey: .maxVms)))
        case "update_user_policy":
            self = .updateUserPolicy(UpdateUserPolicyRequest(requestId: try requestId(),
                userId: try container.decode(String.self, forKey: .userId),
                expectedRevision: try container.decode(UInt64.self, forKey: .expectedRevision),
                policy: try container.decode(ResourcePolicy.self, forKey: .policy)))
        case "delete_user":
            self = .deleteUser(DeleteUserRequest(requestId: try requestId(),
                userId: try container.decode(String.self, forKey: .userId),
                expectedRevision: try container.decode(UInt64.self, forKey: .expectedRevision)))
        case "enroll_device":
            self = .enrollDevice(
                EnrollDeviceRequest(
                    userId: try container.decode(String.self, forKey: .userId),
                    endpointId: try container.decode(String.self, forKey: .endpointId),
                    label: try container.decode(String.self, forKey: .label)))
        case "revoke_device":
            self = .revokeDevice(
                RevokeDeviceRequest(endpointId: try container.decode(String.self, forKey: .endpointId)))
        default:
            throw DecodingError.dataCorruptedError(
                forKey: .action, in: container, debugDescription: "unsupported action")
        }
    }
}

public enum WireCoding {
    public static func encodeRequest(_ request: RpcRequest) throws -> Data {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys, .withoutEscapingSlashes]
        return try encoder.encode(request)
    }
}
