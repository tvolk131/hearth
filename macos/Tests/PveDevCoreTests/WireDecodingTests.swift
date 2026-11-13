import Foundation
import Testing

@testable import PveDevCore

let dashboardJSON = """
    {
      "ok": true,
      "server_version": "0.1.0",
      "result": {
        "identity": {"user_id": "erin", "role": "admin", "budget_mib": 4096, "min_memory_budget_mib": 256,
                     "max_memory_budget_mib": 8192, "max_cores": 8, "max_vms": 4, "ready": true, "extra": [1, 2]},
        "users": [
          {"user_id": "erin", "role": "admin", "budget_mib": 4096, "min_memory_budget_mib": 256,
           "max_memory_budget_mib": 8192, "max_cores": 8, "max_vms": 4, "ready": true},
          {"user_id": "frank", "role": "member", "budget_mib": 128, "min_memory_budget_mib": 256,
           "max_memory_budget_mib": 1024, "max_cores": 8, "max_vms": 4, "ready": false, "note": "x"}
        ],
        "budget": {"budget_mib": 128, "allocated_mib": 32, "remaining_mib": 96, "min_memory_allocated_mib": 128,
                   "memory_allocated_mib": 128, "cores_allocated": 1, "future": {"nested": true}},
        "vms": [
          {"vm_id": 9001, "owner": "frank", "name": "dev-frank", "disk_mib": 32, "memory_mib": 128,
           "min_memory_mib": 128, "cores": 1, "state": "running", "ip": "10.0.0.2"},
          {"vm_id": 9002, "owner": "erin", "name": "dev-erin", "disk_mib": 64, "memory_mib": 256,
           "min_memory_mib": 128, "cores": 2, "state": "hibernating"}
        ],
        "operations": [
          {"operation_id": "op-1", "owner": "frank", "request_id": "6f9619ff-8b86-d011-b42d-00c04fc964ff",
           "kind": "create_vm", "vm_id": 9001, "status": "verified", "step": "done", "created_at": 1762700000},
          {"operation_id": "op-2", "owner": "frank", "request_id": "r2", "kind": "create_user",
           "vm_id": null, "status": "blocked", "step": "storage", "error": "zfs quota", "created_at": 1762700100,
           "attempt": 3}
        ],
        "environment": "loopback-lab",
        "backend": "lab"
      }
    }
    """

@Suite("Response decoding")
struct WireDecodingTests {
    private func result(stdout: String, status: Int32 = 0, signaled: Bool = false, stderr: String = "") -> ProcessResult {
        ProcessResult(
            terminationStatus: status, terminatedBySignal: signaled, stdout: Data(stdout.utf8),
            stderr: Data(stderr.utf8))
    }

    @Test func dashboardDecodesAndToleratesUnknownFields() throws {
        let dashboard = try ResponseInterpreter.interpret(result(stdout: dashboardJSON), as: DashboardResult.self)
        #expect(dashboard.identity.userId == "erin")
        #expect(dashboard.identity.role.isAdmin)
        #expect(dashboard.users.count == 2)
        #expect(dashboard.users[1].role == .member)
        #expect(dashboard.users[1].ready == false)
        #expect(dashboard.budget.remainingMib == 96)
        #expect(dashboard.budget.coresAllocated == 1)
        #expect(dashboard.vms[0].vmId == 9001)
        #expect(dashboard.vms[0].state == .running)
        #expect(dashboard.vms[1].state.rawValue == "hibernating")
        #expect(!dashboard.vms[1].state.isRecognized)
        #expect(dashboard.operations[0].vmId == 9001)
        #expect(dashboard.operations[0].error == nil)
        #expect(dashboard.operations[1].vmId == nil)
        #expect(dashboard.operations[1].error == "zfs quota")
        #expect(dashboard.operations[1].status == .blocked)
        #expect(dashboard.operations[0].createdDate == Date(timeIntervalSince1970: 1_762_700_000))
        #expect(dashboard.backend == .lab)
        #expect(dashboard.environment == "loopback-lab")
    }

    @Test func operationWithoutOptionalFieldsDecodes() throws {
        let json = #"{"ok":true,"result":{"operation_id":"op","owner":"frank","request_id":"r","kind":"vm_action","status":"queued","step":"queued","created_at":0}}"#
        let ack = try ResponseInterpreter.acknowledgement(
            for: .vmAction(VmActionRequest(requestId: UUID(), vmId: 1, kind: .start)), from: result(stdout: json))
        guard case .operation(let record) = ack else {
            Issue.record("expected operation")
            return
        }
        #expect(record.vmId == nil)
        #expect(record.error == nil)
        #expect(record.status.isActive)
    }

    @Test func updatedResultDecodesForDeviceRequests() throws {
        let ack = try ResponseInterpreter.acknowledgement(
            for: .revokeDevice(RevokeDeviceRequest(endpointId: "x")),
            from: result(stdout: #"{"ok":true,"result":{"updated":true,"extra":1}}"#))
        #expect(ack == .updated(true))
    }

    @Test func serverErrorIsDistinguishedEvenWithNonzeroExit() {
        let json = #"{"ok":false,"error":{"code":"conflict","message":"name taken","hint":"x"}}"#
        #expect(throws: RpcFailure.server(ServerError(code: .conflict, message: "name taken"))) {
            try ResponseInterpreter.interpret(result(stdout: json, status: 2), as: DashboardResult.self)
        }
    }

    @Test func unknownServerErrorCodeIsTolerated() throws {
        let json = #"{"ok":false,"error":{"code":"rate_limited","message":"slow down"}}"#
        do {
            _ = try ResponseInterpreter.interpret(result(stdout: json), as: UpdatedResult.self)
            Issue.record("expected failure")
        } catch {
            guard case .server(let serverError) = error else {
                Issue.record("expected server error")
                return
            }
            #expect(serverError.code.rawValue == "rate_limited")
            #expect(!error.isDefinitiveRejection)
            #expect(error.isServerAcknowledged)
        }
    }

    @Test(arguments: ["forbidden", "invalid", "conflict"])
    func definitiveCodes(code: String) {
        #expect(RpcFailure.server(ServerError(code: ServerErrorCode(rawValue: code), message: "")).isDefinitiveRejection)
    }

    @Test(arguments: ["unavailable", "internal"])
    func indeterminateCodes(code: String) {
        #expect(!RpcFailure.server(ServerError(code: ServerErrorCode(rawValue: code), message: "")).isDefinitiveRejection)
    }

    @Test func nonzeroExitWithGarbageIsProcessFailure() {
        do {
            _ = try ResponseInterpreter.interpret(
                result(stdout: "panicked", status: 101, stderr: "thread main panicked"), as: UpdatedResult.self)
            Issue.record("expected failure")
        } catch {
            #expect(error == .processFailed(status: 101, signaled: false, stderr: "thread main panicked"))
            #expect(!error.isServerAcknowledged)
        }
    }

    @Test func signalTerminationIsProcessFailure() {
        do {
            _ = try ResponseInterpreter.interpret(result(stdout: "", status: 9, signaled: true), as: UpdatedResult.self)
            Issue.record("expected failure")
        } catch {
            #expect(error == .processFailed(status: 9, signaled: true, stderr: ""))
        }
    }

    @Test func successEnvelopeWithNonzeroExitIsNotTrusted() {
        do {
            _ = try ResponseInterpreter.interpret(
                result(stdout: #"{"ok":true,"result":{"updated":true}}"#, status: 1), as: UpdatedResult.self)
            Issue.record("expected failure")
        } catch {
            guard case .processFailed = error else {
                Issue.record("expected process failure")
                return
            }
        }
    }

    @Test func cleanExitWithWrongShapeIsMalformed() {
        do {
            _ = try ResponseInterpreter.interpret(
                result(stdout: #"{"ok":true,"result":{"identity":{}}}"#), as: DashboardResult.self)
            Issue.record("expected failure")
        } catch {
            guard case .malformedResponse(let message) = error else {
                Issue.record("expected malformed response")
                return
            }
            #expect(message.contains("missing field"))
            #expect(message.contains("result.identity"))
        }
    }

    @Test func emptyStdoutWithCleanExitIsMalformed() {
        #expect(throws: RpcFailure.malformedResponse("The backend response did not match the expected format (not valid JSON).")) {
            try ResponseInterpreter.interpret(result(stdout: ""), as: UpdatedResult.self)
        }
    }

    @Test func overflowedOutputIsRejected() {
        var overflowed = result(stdout: #"{"ok":true,"result":{"updated":true}}"#)
        overflowed.stdoutOverflowed = true
        #expect(throws: RpcFailure.outputTooLarge) {
            try ResponseInterpreter.interpret(overflowed, as: UpdatedResult.self)
        }
    }

    @Test func serverMessageIsSanitizedForDisplay() {
        let secret = String(repeating: "a1", count: 32)
        let error = ServerError(code: .internal, message: "key \(secret)\u{1B}[31m failed")
        #expect(!error.displayMessage.contains(secret))
        #expect(!error.displayMessage.contains("\u{1B}"))
        #expect(error.displayMessage.contains(SafeText.redactionMarker))
    }
}
