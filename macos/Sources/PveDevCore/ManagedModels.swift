import Foundation

/// The v1 catalog is an RPC result array, independent of the legacy `images` request.
public struct ImageCatalog: Codable, Hashable, Sendable {
    public let images: [ApprovedImage]
    public init(images: [ApprovedImage]) { self.images = images }
    public init(from decoder: any Decoder) throws {
        let c = try decoder.singleValueContainer()
        images = try c.decode([ApprovedImage].self)
        guard Set(images.map(\.imageSha256)).count == images.count else {
            throw DecodingError.dataCorruptedError(in: c, debugDescription: "Duplicate image digest")
        }
    }
    public func encode(to encoder: any Encoder) throws {
        var c = encoder.singleValueContainer(); try c.encode(images)
    }
}

public struct ApprovedImage: Codable, Hashable, Sendable, Identifiable {
    public let imageSha256: String
    public let label: String
    public let osVersion: String
    public let defaultUser: String
    public let minDiskMib: UInt64
    public var id: String { imageSha256 }
    public var displayLabel: String { "\(label) · \(osVersion)" }
    public var accessLabel: String { "Default SSH user: \(defaultUser)" }
    enum CodingKeys: String, CodingKey, CaseIterable {
        case imageSha256 = "image_sha256", label, osVersion = "os_version"
        case defaultUser = "default_user", minDiskMib = "min_disk_mib"
    }
    private struct AnyKey: CodingKey {
        var stringValue: String
        var intValue: Int? { nil }
        init?(stringValue: String) { self.stringValue = stringValue }
        init?(intValue: Int) { return nil }
    }
    public init(imageSha256: String, label: String, osVersion: String, defaultUser: String, minDiskMib: UInt64) {
        self.imageSha256 = imageSha256; self.label = label; self.osVersion = osVersion
        self.defaultUser = defaultUser; self.minDiskMib = minDiskMib
    }
    public init(from decoder: any Decoder) throws {
        let keys = try decoder.container(keyedBy: AnyKey.self).allKeys.map(\.stringValue)
        let c = try decoder.container(keyedBy: CodingKeys.self)
        self.init(imageSha256: try c.decode(String.self, forKey: .imageSha256), label: try c.decode(String.self, forKey: .label),
            osVersion: try c.decode(String.self, forKey: .osVersion), defaultUser: try c.decode(String.self, forKey: .defaultUser),
            minDiskMib: try c.decode(UInt64.self, forKey: .minDiskMib))
        guard Set(keys) == Set(CodingKeys.allCases.map(\.rawValue)), Self.validDigest(imageSha256), minDiskMib > 0,
              [label, osVersion, defaultUser].allSatisfy({ !$0.isEmpty && $0.count <= 128 && !$0.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains) }),
              DraftValidation.isValidUserId(defaultUser) else {
            throw DecodingError.dataCorrupted(.init(codingPath: decoder.codingPath, debugDescription: "Unsupported catalog schema or image metadata"))
        }
    }
    public static func validDigest(_ value: String) -> Bool {
        value.utf8.count == 64 && value.utf8.allSatisfy { (48...57).contains($0) || (97...102).contains($0) }
    }
}

public enum GuestPublicKey {
    /// Strip an optional comment, validate the SSH wire blob and return the canonical key.
    /// Authorized-keys options, other algorithms and multiple lines are never accepted.
    public static func canonicalEd25519(_ input: String) -> String? {
        let value = input.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !value.contains("\n"), !value.contains("\r"), value.utf8.count <= 4096 else { return nil }
        let parts = value.split(whereSeparator: { $0 == " " || $0 == "\t" })
        guard parts.count >= 2, parts[0] == "ssh-ed25519", let bytes = Data(base64Encoded: String(parts[1])),
              bytes.count == 51, bytes.base64EncodedString() == parts[1] else { return nil }
        let prefix: [UInt8] = [0, 0, 0, 11] + Array("ssh-ed25519".utf8) + [0, 0, 0, 32]
        guard Array(bytes.prefix(prefix.count)) == prefix else { return nil }
        return "ssh-ed25519 \(parts[1])"
    }
}

/// Database-only metadata. This type intentionally exposes no mutable VM route.
public struct ImportedVm: Codable, Hashable, Sendable, Identifiable {
    public let vm: Vm
    public let label: String
    public let observedAt: UInt64
    public let configurationSha256: String
    public let reason: String
    public let recoveryNote: String?
    public var id: UInt16 { vm.vmId } // Always scoped to its controller session/view.
    public func scopedId(serverId: UUID) -> String { "\(serverId.uuidString)/imported/\(vm.vmId)" }
    public var recoveryDescription: String {
        recoveryNote ?? (vm.state == .stopped
            ? "Stopped at capture. Retained recovery copies remain separate from VMs on other servers."
            : "Existing VM on this server; records with the same ID on other servers remain separate.")
    }
    public init(vm: Vm, label: String, observedAt: UInt64, configurationSha256: String, reason: String, recoveryNote: String? = nil) {
        self.vm = vm; self.label = label; self.observedAt = observedAt; self.configurationSha256 = configurationSha256
        self.reason = reason; self.recoveryNote = recoveryNote
    }
    enum CodingKeys: String, CodingKey {
        case vm, label, observedAt = "observed_at", configurationSha256 = "configuration_sha256", reason, recoveryNote = "recovery_note"
    }
}

extension DashboardResult {
    /// Only these operations have been qualified for the persistent managed namespace.
    public static let managedCapabilityByAction: [String: String] = [
        "host_inventory": "host_inventory", "manage_existing_vm": "manage_existing_vm",
        "list_image_catalog": "list_image_catalog", "create_vm": "create_guest",
        "start": "start_new_vm", "shutdown": "shutdown_new_vm", "stop": "stop_new_vm", "delete": "delete_new_vm",
        "create_user": "create_user", "update_user_policy": "update_user_policy", "delete_user": "delete_empty_user",
        "inspect_operation": "inspect_operation", "recover_operation": "recover_operation"
    ]
    public static let managedCapabilities = Set(managedCapabilityByAction.values)

    public func isManagedVm(_ id: UInt16) -> Bool {
        vms.contains(where: { $0.vmId == id }) && ((9201...9299).contains(id)
            || (supports("manage_existing_vm") && storageAllocations.contains(where: { !$0.acceptsNewVms && $0.vmIds.contains(id) })))
    }

    public func availableActions(for vm: Vm) -> [VmActionKind] {
        guard backend.isRecognized, !importedVms.contains(where: { $0.vm.vmId == vm.vmId }) else { return [] }
        if backend == .managed && !isManagedVm(vm.vmId) { return [] }
        return Permissions(identity: identity).availableActions(for: vm).filter {
            backend != .managed || (identity.role.isAdmin && supports($0.rawValue))
        }
    }

    public var canCreateManagedVm: Bool {
        Permissions(identity: identity).canCreateVm && (backend != .managed ||
            (Permissions(identity: identity).isAdmin && supports("create_vm") && supports("list_image_catalog")))
            && backend.isRecognized
    }

    /// Shared by forms, explicit routes and pending retries, before any ledger write/send.
    public func requestIssue(_ request: RpcRequest, catalog: ImageCatalog?) -> String? {
        guard backend.isRecognized else { return "This controller backend is unsupported. Changes are unavailable." }
        let target: UInt16?
        switch request {
        case .vmAction(let v): target = v.vmId
        case .growVm(let v): target = v.vmId
        case .shrinkVm(let v): target = v.vmId
        case .finishVmShrink(let v): target = v.vmId
        case .enrollVm(let v): target = v.vmId
        case .planVmGrowth(let id, _, _), .planVmShrink(let id, _, _), .planVmEnrollment(let id, _): target = id
        case .migration(let v): target = v.vmId
        default: target = nil
        }
        if let target, importedVms.contains(where: { $0.vm.vmId == target }) {
            return "Existing VM metadata is imported read-only. Power, deletion, resize, enrollment and move are unavailable."
        }
        guard backend == .managed else {
            if let capability = request.requiredCapability,
               !supports(capability, legacyLabFallback: capability == "shrink_vm" || capability == "shutdown") {
                return "This server has not enabled this action."
            }
            return nil
        }
        if request == .dashboard { return nil }
        func isManagedStart(_ operationId: String) -> Bool {
            guard let operation = operations.first(where: { $0.operationId == operationId }), operation.kind == "start",
                  let vmId = operation.vmId, isManagedVm(vmId),
                  !importedVms.contains(where: { $0.vm.vmId == vmId }),
                  vms.contains(where: { $0.vmId == vmId && $0.owner == operation.owner }) else { return false }
            return Permissions(identity: identity).isAdmin
        }
        let capability: String
        switch request {
        case .hostInventory: capability = "host_inventory"
        case .listImageCatalog: capability = "list_image_catalog"
        case .vmAction(let value):
            // Do not gate a durable retry on the guest's current power state:
            // an accepted request may already have changed it before its reply was lost.
            guard isManagedVm(value.vmId) else {
                return "Only a VM bound to this controller can receive this action."
            }
            capability = value.kind.rawValue
        case .createVm: capability = "create_vm"
        case .createUser: capability = "create_user"
        case .updateUserPolicy: capability = "update_user_policy"
        case .deleteUser: capability = "delete_user"
        case .inspectOperation(let operationId):
            guard isManagedStart(operationId) else { return "Only an administrator can inspect a start attempt for a managed VM." }
            capability = "inspect_operation"
        case .recoverOperation(let value):
            guard value.kind == .acknowledgeFailedStart, isManagedStart(value.operationId) else {
                return "Only acknowledgment of a proven failed managed start is available on this controller."
            }
            capability = "recover_operation"
        default: return "This action is unavailable on the managed controller."
        }
        guard supports(capability) else { return "This server has not enabled this action." }
        if request.isMutation && !Permissions(identity: identity).isAdmin { return "Administrator access is required for managed changes." }
        if case .createVm(let value) = request {
            guard let guest = value.guest, let image = catalog?.images.first(where: { $0.imageSha256 == guest.imageSha256 }) else {
                return "Load this server's approved image catalog and choose a bootable image before creating a VM."
            }
            guard GuestPublicKey.canonicalEd25519(guest.sshPublicKey) == guest.sshPublicKey else {
                return "A canonical Ed25519 SSH public key is required."
            }
            if value.diskMib < image.minDiskMib { return "The selected image requires at least \(image.minDiskMib) MiB of disk." }
        }
        return nil
    }
}
