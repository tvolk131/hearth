import Foundation
import Testing
@testable import PveDevCore

@Suite("Empty account deletion")
struct UserDeletionTests {
    func user(state: UserState = .active, role: UserRole = .member, ready: Bool = true) -> User {
        User(userId: "trial", role: role, budgetMib: 128, minMemoryBudgetMib: 256,
            maxMemoryBudgetMib: 512, maxCores: 2, maxVms: 2, ready: ready,
            policyRevision: 7, state: state)
    }

    @Test func emptyAccountDeletionBindsRevisionAndRetainsIntent() throws {
        let request = try DeleteUserDraft(user: user()).build(requestId: UUID(), vms: [], operations: []).get()
        let data = try WireCoding.encodeRequest(request)
        let json = try JSONSerialization.jsonObject(with: data) as! [String: Any]
        #expect(json["action"] as? String == "delete_user")
        #expect(json["user_id"] as? String == "trial")
        #expect(json["expected_revision"] as? Int == 7)
        #expect(json["cascade"] == nil)
        #expect(request.isDestructive && request.requiresAdministrator && request.expectsOperation)
        #expect(try JSONDecoder().decode(RpcRequest.self, from: data) == request)
        let retry = RpcRequest.deleteUser(DeleteUserRequest(requestId: UUID(), userId: "trial", expectedRevision: 7))
        #expect(request.hasSameIntent(as: retry))
        #expect(!request.hasSameIntent(as: .deleteUser(DeleteUserRequest(requestId: UUID(), userId: "trial", expectedRevision: 8))))
    }

    @Test(arguments: [VmState.stopped, .running, .creating, .deleting, .blocked, VmState(rawValue: "unknown")])
    func anyUndeletedVmPreventsAccountDeletion(state: VmState) {
        let vm = Vm(vmId: 9001, owner: "trial", name: "trial", diskMib: 32,
            memoryMib: 128, minMemoryMib: 128, cores: 1, state: state)
        guard case .failure(let failure) = DeleteUserDraft(user: user()).build(requestId: UUID(), vms: [vm], operations: []) else {
            Issue.record("Account with VM was allowed"); return
        }
        #expect(failure.messages.contains { $0.contains("never deletes VMs") })
    }

    @Test(arguments: [OperationStatus.queued, .running, .blocked, OperationStatus(rawValue: "unknown")])
    func unsettledWorkPreventsDeletion(status: OperationStatus) {
        let operation = OperationRecord(operationId: "one", owner: "trial", requestId: "one", kind: "delete",
            vmId: 9001, status: status, step: "release", error: nil, createdAt: 0)
        guard case .failure = DeleteUserDraft(user: user()).build(requestId: UUID(), vms: [], operations: [operation]) else {
            Issue.record("Unsettled work was ignored"); return
        }
    }

    @Test func deletedVmHistoryDoesNotPreventRetirement() throws {
        let vm = Vm(vmId: 9001, owner: "trial", name: "old", diskMib: 32,
            memoryMib: 128, minMemoryMib: 128, cores: 1, state: .deleted)
        _ = try DeleteUserDraft(user: user()).build(requestId: UUID(), vms: [vm], operations: []).get()
    }

    @Test(arguments: [OperationStatus.verified, .cancelled])
    func terminalHistoryAllowsOnlyAnActuallyEmptyAccount(status: OperationStatus) throws {
        var vm = Vm(vmId: 9201, owner: "trial", name: "old", diskMib: 4096,
            memoryMib: 1024, minMemoryMib: 512, cores: 1, state: .deleted)
        let operation = OperationRecord(operationId: "historical", owner: "trial", requestId: "historical",
            kind: "start", vmId: 9201, status: status, step: "failed_start_acknowledged",
            error: "Historical failed task retained", createdAt: 0)
        #expect(status.isTerminal)
        _ = try DeleteUserDraft(user: user()).build(requestId: UUID(), vms: [vm], operations: [operation]).get()
        vm.state = .stopped
        guard case .failure = DeleteUserDraft(user: user()).build(requestId: UUID(), vms: [vm], operations: [operation]) else {
            Issue.record("Terminal operation history must not hide an existing stopped VM"); return
        }
    }

    @Test func inactiveAndAdministratorAccountsCannotBeDeleted() {
        for account in [user(state: .deleting), user(state: .deleted), user(state: UserState(rawValue: "unknown")), user(role: .admin), user(ready: false)] {
            guard case .failure = DeleteUserDraft(user: account).build(requestId: UUID(), vms: [], operations: []) else {
                Issue.record("Ineligible account deletion allowed"); continue
            }
        }
        #expect(!Permissions(identity: user(state: .deleted, role: .admin)).isAdmin)
        #expect(!Permissions(identity: user(state: .deleting)).canCreateVm)
    }

    @Test func legacyUserIsActiveButUnknownStateHasNoAuthority() throws {
        let bytes = Data(#"{"user_id":"trial","role":"member","budget_mib":128,"min_memory_budget_mib":256,"max_memory_budget_mib":512,"max_cores":2,"max_vms":2,"ready":true}"#.utf8)
        #expect(try JSONDecoder().decode(User.self, from: bytes).state == .active)
        #expect(Permissions(identity: user(state: UserState(rawValue: "unknown"))).userId == nil)
    }
}
