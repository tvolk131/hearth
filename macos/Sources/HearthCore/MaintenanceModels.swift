import Foundation

public struct RecoveryEvidence: Codable, Hashable, Sendable, Identifiable {
    public var name: String
    public var status: String
    public var detail: String
    public var id: String { name }
}

public struct OperationInspection: Codable, Hashable, Sendable {
    public var operation: OperationRecord
    public var inspectionId: String
    public var observedAt: UInt64
    public var classification: String
    public var evidence: [RecoveryEvidence]
    public var allowedActions: [String]

    public var offeredActions: [RecoveryAction] {
        guard operation.status == .blocked else { return [] }
        return RecoveryAction.allCases.filter { allowedActions.contains($0.rawValue) }
    }

    enum CodingKeys: String, CodingKey {
        case operation, classification, evidence
        case inspectionId = "inspection_id", observedAt = "observed_at", allowedActions = "allowed_actions"
    }
}

public enum RecoveryAction: String, Codable, Hashable, Sendable, CaseIterable {
    case resumeVerifiedEffect = "resume_verified_effect"
    case abortPreparation = "abort_preparation"
    case acknowledgeFailedStart = "acknowledge_failed_start"

    public var title: String {
        switch self {
        case .resumeVerifiedEffect: "Resume Verified Effect"
        case .abortPreparation: "Abort Preparation"
        case .acknowledgeFailedStart: "Acknowledge Failed Start"
        }
    }
    public var explanation: String {
        switch self {
        case .resumeVerifiedEffect: "Record the effect independently verified by this inspection and continue the existing operation."
        case .abortPreparation: "Abort this preparation using the server's verified recovery path."
        case .acknowledgeFailedStart: "Close this failed start attempt after the server verifies the failed Proxmox task, stopped guest and original resource identities. The VM and its data remain allocated. You can then request a new Start."
        }
    }
}

public struct RecoverOperationRequest: Hashable, Sendable {
    public var requestId: UUID
    public var operationId: String
    public var inspectionId: String
    public var kind: RecoveryAction
    public var confirmRecovery: Bool

    public init(requestId: UUID, operationId: String, inspectionId: String, kind: RecoveryAction, confirmRecovery: Bool) {
        self.requestId = requestId; self.operationId = operationId; self.inspectionId = inspectionId
        self.kind = kind; self.confirmRecovery = confirmRecovery
    }
}

public struct GrowVmRequest: Hashable, Sendable {
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

public struct DiskGrowthPlan: Codable, Hashable, Sendable {
    public var vmId: UInt16
    public var expectedDiskMib: UInt64
    public var targetDiskMib: UInt64
    public var additionalChargeMib: UInt64
    public var requiresStoppedVm: Bool
    public var filesystemCheckedDuringPreparation: Bool

    enum CodingKeys: String, CodingKey {
        case vmId = "vm_id", expectedDiskMib = "expected_disk_mib", targetDiskMib = "target_disk_mib"
        case additionalChargeMib = "additional_charge_mib", requiresStoppedVm = "requires_stopped_vm"
        case filesystemCheckedDuringPreparation = "filesystem_checked_during_preparation"
    }
}

public struct DiskShrinkPlan: Codable, Hashable, Sendable {
    public var vmId: UInt16
    public var expectedDiskMib: UInt64
    public var targetDiskMib: UInt64
    public var workspaceMib: UInt64
    public var requiresStoppedVm: Bool
    public var recoveryCopyRetained: Bool
    public var filesystemCheckedDuringPreparation: Bool

    enum CodingKeys: String, CodingKey {
        case vmId = "vm_id", expectedDiskMib = "expected_disk_mib", targetDiskMib = "target_disk_mib"
        case workspaceMib = "workspace_mib", requiresStoppedVm = "requires_stopped_vm"
        case recoveryCopyRetained = "recovery_copy_retained", filesystemCheckedDuringPreparation = "filesystem_checked_during_preparation"
    }
}

public enum OperationPresentation {
    public static func title(for kind: String) -> String {
        switch kind {
        case "ensure_user": "Create user"
        case "update_user_policy": "Update resource budgets"
        case "delete_user": "Delete user"
        case "create", "create_guest": "Create virtual machine"
        case "start": "Start virtual machine"
        case "stop": "Stop virtual machine"
        case "shutdown": "Shut down virtual machine"
        case "delete": "Delete virtual machine"
        case "shrink_vm": "Prepare smaller disk"
        case "finish_vm_shrink": "Finish disk shrink"
        case "grow_vm": "Expand virtual disk"
        case "enroll_vm": "Enroll virtual machine"
        case "migrate_vm": "Move virtual machine"
        default: SafeText.sanitize(kind.replacingOccurrences(of: "_", with: " "), limit: 80)
        }
    }

    public static func duration(_ seconds: UInt64) -> String {
        if seconds < 60 { return "\(seconds)s" }
        if seconds < 3600 { return "\(seconds / 60)m \(seconds % 60)s" }
        return "\(seconds / 3600)h \((seconds % 3600) / 60)m"
    }
}

public struct EnrollmentPlan: Codable, Hashable, Sendable {
    public var planId: String
    public var vm: Vm
    public var configurationSha256: String
    public var rootGuid: String
    public var cloudInitGuid: String
    public var observedAt: UInt64
    public var eligible: Bool
    public var blockedReasons: [String]

    enum CodingKeys: String, CodingKey {
        case vm, eligible
        case planId = "plan_id", configurationSha256 = "configuration_sha256", rootGuid = "root_guid"
        case cloudInitGuid = "cloud_init_guid", observedAt = "observed_at", blockedReasons = "blocked_reasons"
    }
}

public struct EnrollVmRequest: Hashable, Sendable {
    public var requestId: UUID
    public var planId: String
    public var vmId: UInt16
    public var owner: String
    public var confirmEnrollment: Bool

    public init(requestId: UUID, planId: String, vmId: UInt16, owner: String, confirmEnrollment: Bool) {
        self.requestId = requestId; self.planId = planId; self.vmId = vmId; self.owner = owner
        self.confirmEnrollment = confirmEnrollment
    }
}

public struct RootPartitionSelection: Hashable, Sendable {
    public var text: String
    public init(text: String = "") { self.text = text }
    public var isValid: Bool { text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || uuid != nil }
    public var uuid: String? { UUID(uuidString: text.trimmingCharacters(in: .whitespacesAndNewlines))?.uuidString.lowercased() }
}
