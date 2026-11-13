import Foundation
import Testing
@testable import PveDevCore

@Suite("Two-stage disk shrink contract")
struct DiskShrinkTests {
    @Test func shrinkWireBindsExpectedSizeDowntimeAndRetryIdentity() throws {
        let request = RpcRequest.shrinkVm(ShrinkVmRequest(requestId: UUID(), vmId: 9001, expectedDiskMib: 4096,
            targetDiskMib: 3072, confirmDowntime: true))
        let bytes = try WireCoding.encodeRequest(request)
        let value = try JSONSerialization.jsonObject(with: bytes) as! [String: Any]
        #expect(value["action"] as? String == "shrink_vm")
        #expect(value["expected_disk_mib"] as? Int == 4096)
        #expect(value["target_disk_mib"] as? Int == 3072)
        #expect(value["confirm_downtime"] as? Bool == true)
        #expect(request.requiresAdministrator && request.isDestructive && request.expectsOperation)
        #expect(try JSONDecoder().decode(RpcRequest.self, from: bytes) == request)
        let retry = RpcRequest.shrinkVm(ShrinkVmRequest(requestId: UUID(), vmId: 9001, expectedDiskMib: 4096,
            targetDiskMib: 3072, confirmDowntime: true))
        #expect(request.hasSameIntent(as: retry))
        let changed = RpcRequest.shrinkVm(ShrinkVmRequest(requestId: UUID(), vmId: 9001, expectedDiskMib: 4096,
            targetDiskMib: 2048, confirmDowntime: true))
        #expect(!request.hasSameIntent(as: changed))
    }

    @Test func finishWireRequiresExactPreparationAndBothAcknowledgements() throws {
        let request = RpcRequest.finishVmShrink(FinishVmShrinkRequest(requestId: UUID(), vmId: 9001, preparationId: String(repeating: "a", count: 64),
            confirmGuestVerified: true, confirmDeleteRecovery: true))
        let bytes = try WireCoding.encodeRequest(request)
        let value = try JSONSerialization.jsonObject(with: bytes) as! [String: Any]
        #expect(value["preparation_id"] as? String == String(repeating: "a", count: 64))
        #expect(value["confirm_guest_verified"] as? Bool == true)
        #expect(value["confirm_delete_recovery"] as? Bool == true)
        #expect(request.requiresAdministrator && request.isDestructive && request.expectsOperation)
        #expect(try JSONDecoder().decode(RpcRequest.self, from: bytes) == request)
    }

    @Test func pendingShrinkRequestsKeepOnlyTheirOwnServerVmBusy() {
        let server = UUID()
        let identity = ServerIdentity(serverId: String(repeating: "a", count: 64), environment: String(repeating: "b", count: 32), deviceId: String(repeating: "c", count: 64))
        for request in [RpcRequest.shrinkVm(ShrinkVmRequest(requestId: UUID(), vmId: 9001, expectedDiskMib: 4096, targetDiskMib: 3072, confirmDowntime: true)),
                        .finishVmShrink(FinishVmShrinkRequest(requestId: UUID(), vmId: 9001, preparationId: "operation", confirmGuestVerified: true, confirmDeleteRecovery: true))] {
            var ledger = MutationLedger()
            ledger.prepare(request, profilePath: "/tmp/profile.json", serverId: server, identity: identity)
            #expect(ledger.isBusy(vmId: 9001, serverId: server, identity: identity))
            #expect(!ledger.isBusy(vmId: 9001, serverId: UUID(), identity: identity))
            #expect(!ledger.isBusy(vmId: 9002, serverId: server, identity: identity))
        }
    }

    @Test func recoveryBudgetMetadataDecodesWithoutOverflow() throws {
        let bytes = Data(#"{"operation_id":"one","vm_id":9001,"owner":"alice","spec":{"previous_disk_mib":4096,"target_disk_mib":3072},"prepared":true}"#.utf8)
        let hold = try JSONDecoder().decode(PendingDiskShrink.self, from: bytes)
        #expect(hold.prepared && hold.spec.workspaceMib == 7591)
        let malformed = Data(#"{"previous_disk_mib":18446744073709551615,"target_disk_mib":3072}"#.utf8)
        #expect(try JSONDecoder().decode(DiskShrinkSpec.self, from: malformed).workspaceMib == 0)
    }

    @Test func gracefulShutdownHasDistinctWireAndDestructiveAcknowledgement() throws {
        let request = RpcRequest.vmAction(VmActionRequest(requestId: UUID(), vmId: 9001, kind: .shutdown))
        #expect(request.isDestructive)
        let bytes = try WireCoding.encodeRequest(request)
        let value = try JSONSerialization.jsonObject(with: bytes) as! [String: Any]
        #expect(value["kind"] as? String == "shutdown")
        #expect(try JSONDecoder().decode(RpcRequest.self, from: bytes) == request)
    }
}
