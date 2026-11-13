import Foundation
import Testing
@testable import PveDevCore

@Suite("Safe policy editing contract")
struct QuotaEditingTests {
    func user(ready: Bool = true) -> User {
        User(userId: "alice", role: .member, budgetMib: 128, minMemoryBudgetMib: 256,
            maxMemoryBudgetMib: 1024, maxCores: 4, maxVms: 4, ready: ready, policyRevision: 7)
    }
    func vm(id: UInt16 = 9001, disk: UInt64 = 32) -> Vm {
        Vm(vmId: id, owner: "alice", name: "one", diskMib: disk, memoryMib: 128, minMemoryMib: 128, cores: 1, state: .stopped)
    }
    @Test func unusedContractionCarriesExactExpectedRevisionAndPersists() throws {
        var draft = UpdateUserPolicyDraft(user: user()); draft.fields.budgetMib = "64"
        let request = try draft.build(requestId: UUID(), vms: [vm()]).get()
        guard case .updateUserPolicy(let policy) = request else { Issue.record("Wrong request"); return }
        #expect(policy.expectedRevision == 7 && policy.policy.budgetMib == 64)
        #expect(request.expectsOperation && request.isMutation)
        let data = try WireCoding.encodeRequest(request)
        let json = try JSONSerialization.jsonObject(with: data) as! [String: Any]
        #expect(json["action"] as? String == "update_user_policy")
        #expect(json["expected_revision"] as? Int == 7)
        #expect((json["policy"] as? [String: Any])?["budget_mib"] as? Int == 64)
        #expect(try JSONDecoder().decode(RpcRequest.self, from: data) == request)
        var same = policy; same.requestId = UUID()
        #expect(request.hasSameIntent(as: .updateUserPolicy(same)))
        same.expectedRevision += 1
        #expect(!request.hasSameIntent(as: .updateUserPolicy(same)))
    }
    @Test func stoppedThickAllocationsRejectContractionWithActionableExplanation() {
        var draft = UpdateUserPolicyDraft(user: user()); draft.fields.budgetMib = "64"
        guard case .failure(let issues) = draft.build(requestId: UUID(), vms: [vm(), vm(id: 9002)]) else {
            Issue.record("Two thick allocations must not fit a64MiB budget"); return
        }
        #expect(issues.messages.contains { $0.contains("88 MiB") && $0.contains("Guest free space") })
    }
    @Test func zeroUnchangedAndUnreadyPoliciesCannotSubmit() {
        for ready in [false,true] {
            var draft = UpdateUserPolicyDraft(user: user(ready: ready)); draft.fields.budgetMib = "0"
            guard case .failure = draft.build(requestId: UUID(), vms: []) else { Issue.record("Zero or unready policy accepted"); continue }
        }
        guard case .failure = UpdateUserPolicyDraft(user: user()).build(requestId: UUID(), vms: []) else { Issue.record("Unchanged policy accepted"); return }
    }
    @Test func malformedLargeAllocationsFailClosedInsteadOfOverflowing() {
        var draft = UpdateUserPolicyDraft(user: user()); draft.fields.budgetMib = "64"
        guard case .failure = draft.build(requestId: UUID(), vms: [vm(disk: UInt64.max)]) else { Issue.record("Accounting overflow accepted"); return }
    }
    @Test func legacyUsersDecodeWithRevisionZero() throws {
        let bytes = Data(#"{"user_id":"alice","role":"member","budget_mib":128,"min_memory_budget_mib":256,"max_memory_budget_mib":1024,"max_cores":4,"max_vms":4,"ready":true}"#.utf8)
        #expect(try JSONDecoder().decode(User.self, from: bytes).policyRevision == 0)
    }
}
