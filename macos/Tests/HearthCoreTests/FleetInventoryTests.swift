import Foundation
import Testing
@testable import HearthCore

@Suite("Fleet allocations and existing VM authority")
struct FleetInventoryTests {
    private func dashboard() -> DashboardResult {
        let user = User(userId: "erin", role: .admin, budgetMib: 65536, minMemoryBudgetMib: 1024,
            maxMemoryBudgetMib: 8192, maxCores: 8, maxVms: 4, ready: true)
        let vm = Vm(vmId: 101, owner: "erin", name: "dev-erin", diskMib: 1310720,
            memoryMib: 32768, minMemoryMib: 12288, cores: 8, state: .running)
        let allocations = [
            StorageAllocation(id: "new", owner: "erin", dataset: "rpool/control/erin", storage: "new", budgetMib: 65536,
                vmIds: [], acceptsNewVms: true, minMemoryBudgetMib: 1024, maxMemoryBudgetMib: 8192, maxCores: 8, maxVms: 4),
            StorageAllocation(id: "dev-erin", owner: "erin", dataset: "rpool/data/dev-erin", storage: "dev-erin", budgetMib: 1572864,
                vmIds: [101], acceptsNewVms: false, minMemoryBudgetMib: 12288, maxMemoryBudgetMib: 32768, maxCores: 8, maxVms: 1)
        ]
        return DashboardResult(identity: user, users: [user], budget: Budget(budgetMib: 65536, allocatedMib: 0,
            remainingMib: 65536, minMemoryAllocatedMib: 0, memoryAllocatedMib: 0, coresAllocated: 0),
            vms: [vm], operations: [], environment: "test", backend: .managed,
            capabilities: Array(DashboardResult.managedCapabilities), storageAllocations: allocations)
    }

    @Test func anExistingVmNeedsBothAnAllocationAndServerCapability() {
        var d = dashboard()
        let action = RpcRequest.vmAction(VmActionRequest(requestId: UUID(), vmId: 101, kind: .shutdown))
        #expect(d.availableActions(for: d.vms[0]).contains(.shutdown))
        #expect(d.requestIssue(action, catalog: nil) == nil)
        d.capabilities.removeAll { $0 == "manage_existing_vm" }
        #expect(d.availableActions(for: d.vms[0]).isEmpty)
        #expect(d.requestIssue(action, catalog: nil) != nil)
        d = dashboard(); d.storageAllocations = []
        #expect(d.requestIssue(action, catalog: nil) != nil)
    }

    @Test func totalsIncludeEveryAllocationWhileProvisioningUsesOnlyItsOwnBudget() {
        let d = dashboard()
        #expect(OwnedUsage(vms: d.vms, owner: "erin").activeVmCount == 1)
        #expect(d.namespaceVms.isEmpty)
        #expect(d.totalDiskBudget(for: d.identity) == 1638400)
        let compute = d.totalComputeBudget(for: d.identity)
        #expect(compute.minimum == 13312 && compute.maximum == 40960 && compute.cores == 16 && compute.vms == 5)
        #expect(d.budget.allocatedMib == 0 && d.budget.remainingMib == 65536)
    }

    @Test func aCompletedDeleteCanStillRetryItsOriginalBoundRequest() {
        var d = dashboard(); d.vms[0].state = .deleted
        let request = RpcRequest.vmAction(VmActionRequest(requestId: UUID(), vmId: 101, kind: .delete))
        #expect(d.availableActions(for: d.vms[0]).isEmpty)
        #expect(d.requestIssue(request, catalog: nil) == nil)
        #expect(d.namespaceVms.isEmpty)
    }

    @Test func onlyADeletedAdoptedVmsAllocationCanBeRetired() throws {
        var d = dashboard()
        let retire = { (dataset: String, confirmed: Bool) in
            RpcRequest.retireStorage(RetireStorageRequest(
                requestId: UUID(), vmId: 101, dataset: dataset, confirmRetireStorage: confirmed))
        }
        // A live adopted VM keeps its allocation; retirement is never part of Delete.
        #expect(d.retirableAllocation(forVm: 101) == nil)
        #expect(d.requestIssue(retire("rpool/data/dev-erin", true), catalog: nil) != nil)
        d.vms[0].state = .deleted
        #expect(d.retirableAllocation(forVm: 101)?.dataset == "rpool/data/dev-erin")
        #expect(d.requestIssue(retire("rpool/data/dev-erin", true), catalog: nil) == nil)
        #expect(d.requestIssue(retire("rpool/data/other", true), catalog: nil) != nil)
        #expect(d.requestIssue(retire("rpool/data/dev-erin", false), catalog: nil) != nil)
        var missing = d; missing.capabilities.removeAll { $0 == "retire_storage" }
        #expect(missing.retirableAllocation(forVm: 101) == nil)
        var member = d; member.identity.role = .member
        #expect(member.retirableAllocation(forVm: 101) == nil)
        // Once retired, the controller stops listing the allocation.
        var retired = d; retired.storageAllocations.removeAll { !$0.acceptsNewVms }
        #expect(retired.retirableAllocation(forVm: 101) == nil)
        #expect(retired.requestIssue(retire("rpool/data/dev-erin", true), catalog: nil) != nil)
        // A namespace allocation is never retirable.
        #expect(d.retirableAllocation(forVm: 9201) == nil)
    }

    @Test func retireStorageIsADestructiveAdministratorOperationOnTheWire() throws {
        let id = UUID(uuidString: "00000000-0000-4000-8000-000000000001")!
        let request = RpcRequest.retireStorage(RetireStorageRequest(
            requestId: id, vmId: 103, dataset: "rpool/data/dev-daniel", confirmRetireStorage: true))
        #expect(request.isMutation && request.isDestructive && request.requiresAdministrator && request.expectsOperation)
        #expect(request.requestId == id)
        #expect(request.requiredCapability == "retire_storage")
        let bytes = try WireCoding.encodeRequest(request)
        #expect(String(decoding: bytes, as: UTF8.self)
            == #"{"action":"retire_storage","confirm_retire_storage":true,"dataset":"rpool/data/dev-daniel","request_id":"00000000-0000-4000-8000-000000000001","vm_id":103}"#)
        #expect(try JSONDecoder().decode(RpcRequest.self, from: bytes) == request)
        let other = RpcRequest.retireStorage(RetireStorageRequest(
            requestId: UUID(), vmId: 103, dataset: "rpool/data/dev-daniel", confirmRetireStorage: true))
        #expect(request.hasSameIntent(as: other))
        #expect(OperationPresentation.title(for: "retire_allocation") == "Retire storage")
    }

    @Test func hardwareInventoryIsAnAdministratorReadAndNeverAMutation() throws {
        #expect(!RpcRequest.hostInventory.isMutation)
        #expect(RpcRequest.hostInventory.requiresAdministrator)
        #expect(RpcRequest.hostInventory.requestId == nil)
        let bytes = try WireCoding.encodeRequest(.hostInventory)
        #expect(String(decoding: bytes, as: UTF8.self) == #"{"action":"host_inventory"}"#)
        #expect(try JSONDecoder().decode(RpcRequest.self, from: bytes) == .hostInventory)
    }
}
