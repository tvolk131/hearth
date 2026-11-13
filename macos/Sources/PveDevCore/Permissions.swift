import Foundation

public struct Permissions: Hashable, Sendable {
    public let userId: String?
    public let isAdmin: Bool

    public init(identity: User?) {
        userId = identity?.state == .active ? identity?.userId : nil
        isAdmin = identity?.state == .active && (identity?.role.isAdmin ?? false)
    }

    public var canCreateVm: Bool { userId != nil }
    public var canChooseVmOwner: Bool { isAdmin }
    public var canManageUsers: Bool { isAdmin }
    public var canManageDevices: Bool { isAdmin }

    public func canManage(_ vm: Vm) -> Bool {
        guard let userId else { return false }
        return isAdmin || vm.owner == userId
    }

    public func availableActions(for vm: Vm) -> [VmActionKind] {
        guard canManage(vm) else { return [] }
        switch vm.state {
        case .stopped: return [.start, .delete]
        case .running: return [.shutdown, .stop, .delete]
        case .blocked: return [.start, .stop, .delete]
        default: return []
        }
    }
}

public struct OwnedUsage: Hashable, Sendable {
    public let activeVmCount: Int

    public init(vms: [Vm], owner: String) {
        activeVmCount = vms.filter { $0.owner == owner && $0.state != .deleted }.count
    }
}
