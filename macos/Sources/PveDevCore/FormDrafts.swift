import Foundation

public enum DraftValidation {
    static func unsigned<T: FixedWidthInteger & UnsignedInteger>(
        _ raw: String, field: String, as type: T.Type, minimum: T = 1, issues: inout [String]
    ) -> T? {
        let trimmed = raw.trimmingCharacters(in: .whitespaces)
        guard let value = T(trimmed) else {
            issues.append("\(field) must be a whole number.")
            return nil
        }
        guard value >= minimum else {
            issues.append("\(field) must be at least \(minimum).")
            return nil
        }
        return value
    }

    public static func isValidVmName(_ name: String) -> Bool {
        guard (1...63).contains(name.count), let first = name.first, let last = name.last else { return false }
        guard first.isASCII, first.isLowercase || first.isNumber else { return false }
        guard last != "-" else { return false }
        return name.allSatisfy { $0.isASCII && ($0.isLowercase || $0.isNumber || $0 == "-") }
    }

    public static func isValidUserId(_ userId: String) -> Bool {
        guard (1...32).contains(userId.count), let first = userId.first, first.isASCII, first.isLowercase else {
            return false
        }
        return userId.allSatisfy { $0.isASCII && ($0.isLowercase || $0.isNumber || $0 == "-" || $0 == "_") }
    }
}

public struct CreateVmDraft: Hashable, Sendable {
    public var name = ""
    public var diskMib = "32"
    public var memoryMib = "128"
    public var minMemoryMib = "128"
    public var cores = "1"
    public var owner = ""
    public var imageSha256 = ""
    public var sshPublicKey = ""

    public init() {}

    public func build(requestId: UUID, allowOwner: Bool, requireGuest: Bool = false, catalog: ImageCatalog? = nil) -> Result<RpcRequest, DraftIssues> {
        var issues: [String] = []
        let trimmedName = name.trimmingCharacters(in: .whitespaces)
        if !DraftValidation.isValidVmName(trimmedName) {
            issues.append("Name must be 1–63 lowercase letters, digits, or hyphens, and cannot end with a hyphen.")
        }
        let disk = DraftValidation.unsigned(diskMib, field: "Disk", as: UInt64.self, issues: &issues)
        let memory = DraftValidation.unsigned(memoryMib, field: "Memory", as: UInt64.self, issues: &issues)
        let minMemory = DraftValidation.unsigned(minMemoryMib, field: "Minimum memory", as: UInt64.self, issues: &issues)
        let coreCount = DraftValidation.unsigned(cores, field: "Cores", as: UInt16.self, issues: &issues)
        if let memory, let minMemory, minMemory > memory {
            issues.append("Minimum memory cannot exceed memory.")
        }
        let trimmedOwner = owner.trimmingCharacters(in: .whitespaces)
        var resolvedOwner: String?
        if !trimmedOwner.isEmpty {
            if !allowOwner {
                issues.append("Only administrators can create VMs for another user.")
            } else if !DraftValidation.isValidUserId(trimmedOwner) {
                issues.append("Owner is not a valid user ID.")
            } else {
                resolvedOwner = trimmedOwner
            }
        }
        var guest: GuestSpec?
        if requireGuest || !imageSha256.isEmpty || !sshPublicKey.isEmpty {
            if let image = catalog?.images.first(where: { $0.imageSha256 == imageSha256 }) {
                if let disk, disk < image.minDiskMib { issues.append("The selected image requires at least \(image.minDiskMib) MiB of disk.") }
                if let key = GuestPublicKey.canonicalEd25519(sshPublicKey) {
                    guest = GuestSpec(imageSha256: image.imageSha256, sshPublicKey: key)
                } else { issues.append("Paste one Ed25519 SSH public key (ssh-ed25519 …).") }
            } else { issues.append("Choose an available server-approved boot image. Reload the catalog if needed.") }
        }
        guard issues.isEmpty, let disk, let memory, let minMemory, let coreCount else {
            return .failure(DraftIssues(messages: issues))
        }
        return .success(
            .createVm(
                CreateVmRequest(
                    requestId: requestId, name: trimmedName, diskMib: disk, memoryMib: memory,
                    minMemoryMib: minMemory, cores: coreCount, owner: resolvedOwner, guest: guest)))
    }
}

public struct CreateUserDraft: Hashable, Sendable {
    public var userId = ""
    public var budgetMib = "128"
    public var minMemoryBudgetMib = "256"
    public var maxMemoryBudgetMib = "1024"
    public var maxCores = "8"
    public var maxVms = "4"

    public init() {}

    public func build(requestId: UUID) -> Result<RpcRequest, DraftIssues> {
        var issues: [String] = []
        let trimmedUser = userId.trimmingCharacters(in: .whitespaces)
        if !DraftValidation.isValidUserId(trimmedUser) {
            issues.append("User ID must start with a lowercase letter and contain only lowercase letters, digits, hyphens, or underscores (max 32).")
        }
        let budget = DraftValidation.unsigned(budgetMib, field: "Disk budget", as: UInt64.self, issues: &issues)
        let minMemory = DraftValidation.unsigned(
            minMemoryBudgetMib, field: "Minimum memory budget", as: UInt64.self, issues: &issues)
        let maxMemory = DraftValidation.unsigned(
            maxMemoryBudgetMib, field: "Maximum memory budget", as: UInt64.self, issues: &issues)
        let coreLimit = DraftValidation.unsigned(maxCores, field: "Max cores", as: UInt16.self, issues: &issues)
        let vmLimit = DraftValidation.unsigned(maxVms, field: "Max VMs", as: UInt16.self, issues: &issues)
        if let minMemory, let maxMemory, minMemory > maxMemory {
            issues.append("Minimum memory budget cannot exceed maximum memory budget.")
        }
        guard issues.isEmpty, let budget, let minMemory, let maxMemory, let coreLimit, let vmLimit else {
            return .failure(DraftIssues(messages: issues))
        }
        return .success(
            .createUser(
                CreateUserRequest(
                    requestId: requestId, userId: trimmedUser, budgetMib: budget, minMemoryBudgetMib: minMemory,
                    maxMemoryBudgetMib: maxMemory, maxCores: coreLimit, maxVms: vmLimit)))
    }
}

public struct UpdateUserPolicyDraft: Hashable, Sendable {
    public let previous: User
    public var fields: CreateUserDraft
    public init(user: User) {
        previous = user
        fields = CreateUserDraft()
        fields.userId = user.userId; fields.budgetMib = String(user.budgetMib)
        fields.minMemoryBudgetMib = String(user.minMemoryBudgetMib); fields.maxMemoryBudgetMib = String(user.maxMemoryBudgetMib)
        fields.maxCores = String(user.maxCores); fields.maxVms = String(user.maxVms)
    }
    public func build(requestId: UUID, vms: [Vm], backend: BackendKind = .lab) -> Result<RpcRequest, DraftIssues> {
        guard previous.ready && previous.state == .active else { return .failure(DraftIssues(messages: ["This user is inactive or has unfinished work. Refresh after it is verified."])) }
        switch fields.build(requestId: requestId) {
        case .failure(let issues): return .failure(issues)
        case .success(.createUser(let value)):
            let owned = vms.filter { $0.owner == previous.userId && $0.state != .deleted }
            var overflow = false
            func sum(_ values: [UInt64]) -> UInt64 {
                values.reduce(0) { total, next in
                    let addition = total.addingReportingOverflow(next)
                    overflow = overflow || addition.overflow
                    return addition.partialValue
                }
            }
            let disk = sum(owned.flatMap { [$0.diskMib, 4, max(8, $0.diskMib / 20 + ($0.diskMib % 20 == 0 ? 0 : 1))] })
            let minMemory = sum(owned.map(\.minMemoryMib))
            let maxMemory = sum(owned.map(\.memoryMib))
            let cores = sum(owned.map { UInt64($0.cores) })
            guard !overflow else { return .failure(DraftIssues(messages: ["Observed allocations exceed supported accounting. Refresh and investigate before editing."])) }
            var issues: [String] = []
            if backend != .managed && (!(64...8192).contains(value.budgetMib) || !(128...1024).contains(value.minMemoryBudgetMib)
                || value.maxMemoryBudgetMib > 2048 || value.maxCores > 16 || value.maxVms > 8) {
                issues.append("Use the positive lab limits: disk 64–8192 MiB, minimum memory 128–1024 MiB, maximum memory ≤2048 MiB, cores ≤16 and VMs ≤8. Zero/removal and unlimited quotas are unavailable.")
            }
            if value.budgetMib < disk { issues.append("Disk budget must cover at least \(disk) MiB of committed thick disks and overhead. Guest free space does not reduce this allocation.") }
            if value.minMemoryBudgetMib < minMemory || value.maxMemoryBudgetMib < maxMemory || UInt64(value.maxCores) < cores || Int(value.maxVms) < owned.count {
                issues.append("Target budgets must cover every existing VM allocation, including stopped VMs.")
            }
            if value.budgetMib == previous.budgetMib && value.minMemoryBudgetMib == previous.minMemoryBudgetMib
                && value.maxMemoryBudgetMib == previous.maxMemoryBudgetMib && value.maxCores == previous.maxCores && value.maxVms == previous.maxVms {
                issues.append("Change at least one budget before submitting.")
            }
            guard issues.isEmpty else { return .failure(DraftIssues(messages: issues)) }
            return .success(.updateUserPolicy(UpdateUserPolicyRequest(requestId: requestId, userId: previous.userId,
                expectedRevision: previous.policyRevision, policy: ResourcePolicy(budgetMib: value.budgetMib,
                    minMemoryBudgetMib: value.minMemoryBudgetMib, maxMemoryBudgetMib: value.maxMemoryBudgetMib,
                    maxCores: value.maxCores, maxVms: value.maxVms))))
        case .success: return .failure(DraftIssues(messages: ["Invalid policy draft."]))
        }
    }
}

public struct DeleteUserDraft: Hashable, Sendable {
    public let user: User
    public init(user: User) { self.user = user }
    public func build(requestId: UUID, vms: [Vm], operations: [OperationRecord]) -> Result<RpcRequest, DraftIssues> {
        var issues: [String] = []
        if user.role != .member { issues.append("Only member accounts can be deleted.") }
        if !user.ready || user.state != .active { issues.append("This account is inactive or has unfinished work.") }
        if vms.contains(where: { $0.owner == user.userId && $0.state != .deleted }) {
            issues.append("Delete or move every VM first, including stopped VMs. Account deletion never deletes VMs.")
        }
        if operations.contains(where: { $0.owner == user.userId && !$0.status.isTerminal }) {
            issues.append("All pending or blocked operations must be resolved before deleting this account.")
        }
        guard issues.isEmpty else { return .failure(DraftIssues(messages: issues)) }
        return .success(.deleteUser(DeleteUserRequest(requestId: requestId, userId: user.userId,
            expectedRevision: user.policyRevision)))
    }
}

public struct EnrollDeviceDraft: Hashable, Sendable {
    public var userId = ""
    public var endpointId = ""
    public var label = ""

    public init() {}

    public func build() -> Result<RpcRequest, DraftIssues> {
        var issues: [String] = []
        let trimmedUser = userId.trimmingCharacters(in: .whitespaces)
        if !DraftValidation.isValidUserId(trimmedUser) {
            issues.append("Choose a valid user.")
        }
        let endpoint = EndpointIdentifier.normalize(endpointId)
        if endpoint == nil {
            issues.append("Endpoint ID must be a 64-character hexadecimal iroh public key.")
        }
        let trimmedLabel = label.trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmedLabel.isEmpty || trimmedLabel.count > 64 {
            issues.append("Label must be 1–64 characters.")
        }
        guard issues.isEmpty, let endpoint else { return .failure(DraftIssues(messages: issues)) }
        return .success(.enrollDevice(EnrollDeviceRequest(userId: trimmedUser, endpointId: endpoint, label: trimmedLabel)))
    }
}

public struct RevokeDeviceDraft: Hashable, Sendable {
    public var endpointId = ""

    public init() {}

    public func build() -> Result<RpcRequest, DraftIssues> {
        guard let endpoint = EndpointIdentifier.normalize(endpointId) else {
            return .failure(DraftIssues(messages: ["Endpoint ID must be a 64-character hexadecimal iroh public key."]))
        }
        return .success(.revokeDevice(RevokeDeviceRequest(endpointId: endpoint)))
    }
}

public struct DraftIssues: Error, Hashable, Sendable {
    public let messages: [String]
}
