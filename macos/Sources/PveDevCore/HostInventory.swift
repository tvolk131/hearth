import Foundation

public struct StorageAllocation: Codable, Hashable, Sendable, Identifiable {
    public let id: String
    public let owner: String
    public let dataset: String
    public let storage: String
    public let budgetMib: UInt64
    public let vmIds: [UInt16]
    public let acceptsNewVms: Bool
    public let minMemoryBudgetMib: UInt64
    public let maxMemoryBudgetMib: UInt64
    public let maxCores: UInt16
    public let maxVms: UInt16
    enum CodingKeys: String, CodingKey {
        case id, owner, dataset, storage
        case budgetMib = "budget_mib", vmIds = "vm_ids", acceptsNewVms = "accepts_new_vms"
        case minMemoryBudgetMib = "min_memory_budget_mib", maxMemoryBudgetMib = "max_memory_budget_mib"
        case maxCores = "max_cores", maxVms = "max_vms"
    }
}

public struct BlockDevice: Codable, Hashable, Sendable, Identifiable {
    public var id: String { path }
    public let name: String
    public let path: String
    public let type: String
    public let size: UInt64
    public let model: String?
    public let serial: String?
    public let rota: Bool?
    public let fstype: String?
    public let mountpoints: [String?]?
    public let children: [BlockDevice]?
}

public struct HostInventory: Codable, Hashable, Sendable {
    public let schema: Int
    public let node: String
    public let capturedAt: String
    public let cpuModel: String
    public let logicalCpus: UInt64
    public let cpuSockets: UInt64
    public let memoryTotalBytes: UInt64
    public let memoryUsedBytes: UInt64
    public let uptimeSeconds: UInt64
    public let pveVersion: String
    public let poolSizeBytes: UInt64
    public let poolAllocatedBytes: UInt64
    public let poolFreeBytes: UInt64
    public let blockDevices: [BlockDevice]
    public let zpoolTopology: String
    public let datasets: [[String: String]]
    public let storageAllocations: [StorageAllocation]
    enum CodingKeys: String, CodingKey {
        case schema, node, datasets
        case capturedAt = "captured_at", cpuModel = "cpu_model", logicalCpus = "logical_cpus", cpuSockets = "cpu_sockets"
        case memoryTotalBytes = "memory_total_bytes", memoryUsedBytes = "memory_used_bytes", uptimeSeconds = "uptime_seconds"
        case pveVersion = "pve_version", blockDevices = "block_devices", zpoolTopology = "zpool_topology"
        case poolSizeBytes = "pool_size_bytes", poolAllocatedBytes = "pool_allocated_bytes", poolFreeBytes = "pool_free_bytes"
        case storageAllocations = "storage_allocations"
    }
}

extension DashboardResult {
    public var namespaceVms: [Vm] {
        let allocated = Set(storageAllocations.filter { !$0.acceptsNewVms }.flatMap(\.vmIds))
        return vms.filter { !allocated.contains($0.vmId) }
    }
    public func totalDiskBudget(for user: User) -> UInt64 {
        let allocations = storageAllocations.filter { $0.owner == user.userId }
        return allocations.isEmpty ? user.budgetMib : allocations.reduce(0) { $0 + $1.budgetMib }
    }
    public func totalComputeBudget(for user: User) -> (minimum: UInt64, maximum: UInt64, cores: UInt64, vms: UInt64) {
        let allocations = storageAllocations.filter { $0.owner == user.userId }
        guard !allocations.isEmpty else { return (user.minMemoryBudgetMib, user.maxMemoryBudgetMib, UInt64(user.maxCores), UInt64(user.maxVms)) }
        return allocations.reduce((0, 0, 0, 0)) { ($0.0 + $1.minMemoryBudgetMib, $0.1 + $1.maxMemoryBudgetMib,
            $0.2 + UInt64($1.maxCores), $0.3 + UInt64($1.maxVms)) }
    }
}
