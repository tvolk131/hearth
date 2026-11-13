import Foundation
import Testing
@testable import PveDevCore

@Suite("Maintenance plans and operation recovery")
struct MaintenanceTests {
    @Test func plansAndInspectionAreReadOnlyAndNeverAcquireRequestIds() throws {
        for request in [RpcRequest.inspectOperation(operationId: "operation"),
                        .planVmGrowth(vmId: 9001, targetDiskMib: 16384),
                        .planVmShrink(vmId: 9001, targetDiskMib: 12288)] {
            #expect(!request.isMutation && !request.expectsOperation && request.requestId == nil)
            #expect(request.requiresAdministrator)
            let bytes = try WireCoding.encodeRequest(request)
            let json = try JSONSerialization.jsonObject(with: bytes) as! [String: Any]
            #expect(json["request_id"] == nil)
            #expect(try JSONDecoder().decode(RpcRequest.self, from: bytes) == request)
        }
    }

    @Test func recoveryRetryBindsInspectionOperationAndAction() throws {
        let request = RpcRequest.recoverOperation(RecoverOperationRequest(requestId: UUID(), operationId: "one",
            inspectionId: "proof-a", kind: .resumeVerifiedEffect, confirmRecovery: true))
        let same = RpcRequest.recoverOperation(RecoverOperationRequest(requestId: UUID(), operationId: "one",
            inspectionId: "proof-a", kind: .resumeVerifiedEffect, confirmRecovery: true))
        #expect(request.hasSameIntent(as: same))
        for different in [RecoverOperationRequest(requestId: UUID(), operationId: "two", inspectionId: "proof-a", kind: .resumeVerifiedEffect, confirmRecovery: true),
                          RecoverOperationRequest(requestId: UUID(), operationId: "one", inspectionId: "proof-b", kind: .resumeVerifiedEffect, confirmRecovery: true),
                          RecoverOperationRequest(requestId: UUID(), operationId: "one", inspectionId: "proof-a", kind: .abortPreparation, confirmRecovery: true)] {
            #expect(!request.hasSameIntent(as: .recoverOperation(different)))
        }
        let bytes = try WireCoding.encodeRequest(request)
        let json = try JSONSerialization.jsonObject(with: bytes) as! [String: Any]
        #expect(json["kind"] as? String == "resume_verified_effect")
        #expect(json["confirm_recovery"] as? Bool == true)
        #expect(request.requiresAdministrator && request.expectsOperation && request.isDestructive)
        #expect(try JSONDecoder().decode(RpcRequest.self, from: bytes) == request)
    }

    @Test func growthBindsExpectedSizeAndKeepsOnlyItsServerVmBusy() throws {
        let request = RpcRequest.growVm(GrowVmRequest(requestId: UUID(), vmId: 9001, expectedDiskMib: 8192,
            targetDiskMib: 16384, confirmDowntime: true))
        #expect(try JSONDecoder().decode(RpcRequest.self, from: WireCoding.encodeRequest(request)) == request)
        let server = UUID(), identity = ServerIdentity(serverId: String(repeating: "a", count: 64), environment: String(repeating: "b", count: 32), deviceId: String(repeating: "c", count: 64))
        var ledger = MutationLedger()
        ledger.prepare(request, profilePath: "/tmp/a.json", serverId: server, identity: identity)
        #expect(ledger.isBusy(vmId: 9001, serverId: server, identity: identity))
        #expect(!ledger.isBusy(vmId: 9001, serverId: UUID(), identity: identity))
        let changed = RpcRequest.growVm(GrowVmRequest(requestId: UUID(), vmId: 9001, expectedDiskMib: 12288,
            targetDiskMib: 16384, confirmDowntime: true))
        #expect(!request.hasSameIntent(as: changed))
    }

    @Test func unknownServerRecoveryActionsAreNotExposed() throws {
        let bytes = Data(#"{"operation":{"operation_id":"o","owner":"alice","request_id":"r","kind":"shrink_vm","vm_id":9001,"status":"blocked","step":"shrink_stage","error":"interrupted","created_at":1},"inspection_id":"proof","observed_at":2,"classification":"verified_copy","evidence":[{"name":"original","status":"verified","detail":"unchanged"}],"allowed_actions":["force_destroy_everything","resume_verified_effect"]}"#.utf8)
        let result = try JSONDecoder().decode(OperationInspection.self, from: bytes)
        #expect(result.offeredActions == [.resumeVerifiedEffect])
        #expect(result.evidence.count == 1)
        var completed = result; completed.operation.status = .cancelled
        #expect(completed.offeredActions.isEmpty)
    }

    @Test func failedStartAcknowledgmentRequiresExplicitServerOfferAndKeepsReviewedIntent() throws {
        let bytes = Data(#"{"operation":{"operation_id":"failed-start","owner":"setup-check","request_id":"original","kind":"start","vm_id":9201,"status":"blocked","step":"start","error":"QEMU exited","created_at":1},"inspection_id":"exact-proof","observed_at":2,"classification":"known_failed_start","evidence":[],"allowed_actions":["acknowledge_failed_start","invented_force_start"]}"#.utf8)
        let inspection = try JSONDecoder().decode(OperationInspection.self, from: bytes)
        #expect(inspection.offeredActions == [.acknowledgeFailedStart])
        let request = RpcRequest.recoverOperation(RecoverOperationRequest(requestId: UUID(), operationId: inspection.operation.operationId,
            inspectionId: inspection.inspectionId, kind: .acknowledgeFailedStart, confirmRecovery: true))
        let wire = try WireCoding.encodeRequest(request)
        let object = try JSONSerialization.jsonObject(with: wire) as! [String: Any]
        #expect(object["kind"] as? String == "acknowledge_failed_start")
        #expect(request.requiredCapability == "recover_operation" && request.requiresAdministrator)
        #expect(try JSONDecoder().decode(RpcRequest.self, from: wire) == request)
        var unoffered = inspection; unoffered.allowedActions = []
        #expect(unoffered.offeredActions.isEmpty)
    }

    @Test func oldOperationsDecodeWithoutTimingAndNewTimingIsBounded() throws {
        let old = Data(#"{"operation_id":"o","owner":"alice","request_id":"r","kind":"shrink_vm","vm_id":9001,"status":"running","step":"shrink_stage","error":null,"created_at":1}"#.utf8)
        #expect(try JSONDecoder().decode(OperationRecord.self, from: old).progress == nil)
        let progress = OperationProgress(startedAt: 100, updatedAt: 115, finishedAt: 120, completedSteps: 3, totalSteps: 4, detail: nil)
        #expect(progress.fractionCompleted == 0.75)
        #expect(progress.elapsedSeconds(now: Date(timeIntervalSince1970: 1000)) == 20)
        let invalid = OperationProgress(startedAt: UInt64.max, updatedAt: UInt64.max, finishedAt: 1, completedSteps: 4, totalSteps: 3, detail: nil)
        #expect(invalid.fractionCompleted == nil && invalid.elapsedSeconds() == nil)
        let empty = OperationProgress(startedAt: nil, updatedAt: 0, finishedAt: nil, completedSteps: 0, totalSteps: 0, detail: nil)
        #expect(empty.fractionCompleted == nil && empty.elapsedSeconds() == nil)
    }

    @Test func physicalCapabilitiesNeverUseLegacyLabFallback() throws {
        let json = #"{"identity":{"user_id":"erin","role":"admin","budget_mib":8192,"min_memory_budget_mib":128,"max_memory_budget_mib":1024,"max_cores":8,"max_vms":4,"ready":true},"users":[],"vms":[],"operations":[],"budget":{"budget_mib":8192,"allocated_mib":0,"remaining_mib":8192,"min_memory_allocated_mib":0,"memory_allocated_mib":0,"cores_allocated":0},"environment":"test","backend":"physical_trial"}"#
        var dashboard = try JSONDecoder().decode(DashboardResult.self, from: Data(json.utf8))
        #expect(!dashboard.supports("shrink_vm", legacyLabFallback: true))
        #expect(!dashboard.supports("grow_vm"))
        #expect(!dashboard.supports("shutdown", legacyLabFallback: true))
        dashboard.capabilities = ["grow_vm", "future_action"]
        #expect(dashboard.supports("grow_vm"))
        #expect(!dashboard.supports("shrink_vm", legacyLabFallback: true))
        dashboard.backend = .lab; dashboard.capabilities = []
        #expect(dashboard.supports("shrink_vm", legacyLabFallback: true))
        #expect(!dashboard.supports("grow_vm"))
    }

    @Test func largeWorkspaceEstimatesUseCheckedArithmetic() {
        let spec = DiskShrinkSpec(previousDiskMib: 750 * 1024, targetDiskMib: 500 * 1024)
        #expect(spec.workspaceMib == 1_344_064)
        #expect(DiskShrinkSpec(previousDiskMib: UInt64.max, targetDiskMib: 2048).workspaceMib == 0)
        #expect(DiskShrinkSpec(previousDiskMib: 4096, targetDiskMib: 4096).workspaceMib == 0)
        #expect(DiskShrinkSpec(previousDiskMib: 4096, targetDiskMib: 1).workspaceMib == 0)
    }
}
