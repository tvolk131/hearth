import Foundation
import Testing
@testable import PveDevCore

private func migrationFixture() throws -> (Vm, ServerIdentity, ServerIdentity, SignedMigrationGrant, SignedMigrationGrant, MigrationDescriptor) {
    let source = ServerIdentity(serverId: String(repeating: "a", count: 64), environment: String(repeating: "b", count: 32), deviceId: String(repeating: "c", count: 64))
    let destination = ServerIdentity(serverId: String(repeating: "d", count: 64), environment: String(repeating: "e", count: 32), deviceId: String(repeating: "f", count: 64))
    let vm = Vm(vmId: 9001, owner: "alice", name: "dev-alice", diskMib: 4096, memoryMib: 1024, minMemoryMib: 512, cores: 2, state: .stopped)
    let a = MigrationPeer(serverId: source.serverId, environment: source.environment)
    let b = MigrationPeer(serverId: destination.serverId, environment: destination.environment)
    let plan = MigrationPlan(schema: 1, migrationId: String(repeating: "1", count: 64), source: a, destination: b,
        sourceVmId: vm.vmId, targetVmId: 9002, sourceOwner: vm.owner, targetOwner: "bob",
        sourceMachineSha256: String(repeating: "2", count: 64), sourceConfigSha256: String(repeating: "3", count: 64),
        name: vm.name, diskMib: vm.diskMib, memoryMib: vm.memoryMib, minMemoryMib: vm.minMemoryMib,
        cores: vm.cores, guest: nil, byteCeiling: 4300000000, targetBridge: "vmbr77")
    let sourceGrant = SignedMigrationGrant(value: MigrationGrant(schema: 1, plan: plan, role: "source", issuer: a,
        recipient: b, actor: source.deviceId, nonce: String(repeating: "4", count: 64), sourceGrantSha256: nil, issuedAt: 100, expiresAt: 4000), signature: "source-signature")
    let targetGrant = SignedMigrationGrant(value: MigrationGrant(schema: 1, plan: plan, role: "destination", issuer: b,
        recipient: a, actor: destination.deviceId, nonce: String(repeating: "5", count: 64), sourceGrantSha256: String(repeating: "6", count: 64), issuedAt: 200, expiresAt: 4000), signature: "target-signature")
    let descriptor = MigrationDescriptor(schema: 1, endpoint: .object(["id":.string(destination.serverId),
        "addrs":.array([.object(["Ip":.string("127.0.0.1:1234")])])]), environment: destination.environment, network: "loopback")
    return (vm, source, destination, sourceGrant, targetGrant, descriptor)
}

@Suite("Migration UI authority binding")
struct MigrationTests {
    @Test func grantReviewRejectsChangedPeerVmOwnerAndResourceProfile() throws {
        let (vm, source, target, sourceGrant, targetGrant, descriptor) = try migrationFixture()
        #expect(descriptor.matches(target))
        #expect(!descriptor.matches(source))
        #expect(MigrationReview.sourceGrant(sourceGrant, matches: vm, source: source, destination: target, targetVmId: 9002, targetOwner: "bob"))
        #expect(MigrationReview.destinationGrant(targetGrant, matches: sourceGrant, destination: target))
        #expect(!MigrationReview.sourceGrant(sourceGrant, matches: vm, source: source, destination: target, targetVmId: 9001, targetOwner: "bob"))
        #expect(!MigrationReview.sourceGrant(sourceGrant, matches: vm, source: source, destination: target, targetVmId: 9002, targetOwner: "alice"))
        var changed = vm; changed.diskMib = 8192
        #expect(!MigrationReview.sourceGrant(sourceGrant, matches: changed, source: source, destination: target, targetVmId: 9002, targetOwner: "bob"))
        var wrong = targetGrant; wrong.value.plan.sourceConfigSha256 = String(repeating: "4", count: 64)
        #expect(!MigrationReview.destinationGrant(wrong, matches: sourceGrant, destination: target))
        wrong = targetGrant; wrong.value.actor = source.deviceId
        #expect(!MigrationReview.destinationGrant(wrong, matches: sourceGrant, destination: target))
        wrong = targetGrant; wrong.value.sourceGrantSha256 = nil
        #expect(!MigrationReview.destinationGrant(wrong, matches: sourceGrant, destination: target))
        wrong = targetGrant; wrong.value.nonce = String(repeating: "z", count: 64)
        #expect(!MigrationReview.destinationGrant(wrong, matches: sourceGrant, destination: target))
    }

    @Test func migrationWirePreservesSignedGrantAndPublicRouteForRetry() throws {
        let (_, _, _, sourceGrant, targetGrant, descriptor) = try migrationFixture()
        let requests: [MigrationAdminRequest] = [.describe, .list, .status(migrationId: "one"),
            .plan(requestId: UUID(), vmId: 9001, destination: descriptor, targetOwner: "bob", targetVmId: 9002, expiresInSeconds: 3600),
            .authorizeDestination(sourceGrant: sourceGrant, expiresInSeconds: 3600),
            .renew(requestId: UUID(), migrationId: "one", expiresInSeconds: 3600),
            .renew(requestId: UUID(), migrationId: "one", expiresInSeconds: 3600, sourceGrant: sourceGrant),
            .start(migrationId: "one", destinationGrant: targetGrant), .abort(migrationId: "one", confirmCleanup: true),
            .finish(migrationId: "one", confirmGuestVerified: true, confirmDeleteRecovery: true)]
        for action in requests {
            let request = RpcRequest.migration(action), bytes = try WireCoding.encodeRequest(request)
            #expect(try JSONDecoder().decode(RpcRequest.self, from: bytes) == request)
            #expect(request.isMutation == action.isMutation)
            #expect(request.requiredCapability == "migration" && request.requiresAdministrator)
            let json = try JSONSerialization.jsonObject(with: bytes) as! [String: Any]
            #expect(json["action"] as? String == "migration")
            #expect(json["request"] is [String: Any])
        }
        let first = RpcRequest.migration(.plan(requestId: UUID(), vmId: 9001, destination: descriptor, targetOwner: "bob", targetVmId: 9002, expiresInSeconds: 3600))
        let retry = RpcRequest.migration(.plan(requestId: UUID(), vmId: 9001, destination: descriptor, targetOwner: "bob", targetVmId: 9002, expiresInSeconds: 3600))
        #expect(first.hasSameIntent(as: retry))
        #expect(!first.hasSameIntent(as: .migration(.plan(requestId: UUID(), vmId: 9001, destination: descriptor, targetOwner: "bob", targetVmId: 9003, expiresInSeconds: 3600))))
        var alteredGrant = targetGrant; alteredGrant.signature = "changed"
        #expect(!RpcRequest.migration(.start(migrationId: "one", destinationGrant: targetGrant)).hasSameIntent(as: .migration(.start(migrationId: "one", destinationGrant: alteredGrant))))
    }

    @Test func mutationAcknowledgementDecodesGrantWithoutExposingItInSummary() throws {
        let (_, _, _, grant, _, _) = try migrationFixture()
        let body = try JSONSerialization.data(withJSONObject: ["ok":true,"result":JSONSerialization.jsonObject(with: JSONEncoder().encode(grant))])
        let result = ProcessResult(terminationStatus: 0, terminatedBySignal: false, stdout: body, stderr: Data())
        let request = RpcRequest.migration(.authorizeDestination(sourceGrant: grant, expiresInSeconds: 3600))
        let ack = try ResponseInterpreter.acknowledgement(for: request, from: result)
        guard case .migrationGrant(let decoded) = ack else { Issue.record("Expected typed authorization"); return }
        #expect(decoded == grant)
        #expect(!ack.summary.contains(grant.signature))
    }

    @Test func renewalRetriesRetainIntentAndRoundTripAllSignedFields() throws {
        let (_, _, _, source, destination, _) = try migrationFixture()
        let id = UUID()
        let request = RpcRequest.migration(.renew(requestId: id, migrationId: source.value.plan.migrationId,
            expiresInSeconds: 3600, sourceGrant: source))
        #expect(request.requestId == id)
        #expect(request.hasSameIntent(as: .migration(.renew(requestId: UUID(), migrationId: source.value.plan.migrationId,
            expiresInSeconds: 3600, sourceGrant: source))))
        var changed = source; changed.value.nonce = String(repeating: "7", count: 64)
        #expect(!request.hasSameIntent(as: .migration(.renew(requestId: id, migrationId: source.value.plan.migrationId,
            expiresInSeconds: 3600, sourceGrant: changed))))
        #expect(!request.hasSameIntent(as: .migration(.renew(requestId: id, migrationId: source.value.plan.migrationId,
            expiresInSeconds: 7200, sourceGrant: source))))
        let encoded = try WireCoding.encodeRequest(.migration(.start(migrationId: source.value.plan.migrationId, destinationGrant: destination)))
        let json = try JSONSerialization.jsonObject(with: encoded) as! [String: Any]
        let nested = json["request"] as! [String: Any]
        let grant = nested["destination_grant"] as! [String: Any]
        let value = grant["value"] as! [String: Any]
        #expect(value["nonce"] as? String == destination.value.nonce)
        #expect(value["source_grant_sha256"] as? String == destination.value.sourceGrantSha256)
        #expect(grant["signature"] as? String == destination.signature)
        #expect(try JSONDecoder().decode(RpcRequest.self, from: encoded) == .migration(.start(migrationId: source.value.plan.migrationId, destinationGrant: destination)))
    }

    @Test func destinationAbortForwardsExactSourceProofAndCleanupWaitsForAcceptance() throws {
        let (_, _, _, source, destination, _) = try migrationFixture()
        let proof = SignedMigrationAbort(value: MigrationAbortReceipt(migrationId: source.value.plan.migrationId,
            planSha256: String(repeating: "8", count: 64), source: source.value.plan.source, abortedAt: 300), signature: "source-abort")
        #expect(proof.matches(source.value.plan))
        var wrong = proof; wrong.value.migrationId = "another"
        #expect(!wrong.matches(source.value.plan))
        let request = RpcRequest.migration(.abort(migrationId: proof.value.migrationId, confirmCleanup: true, sourceAbort: proof))
        #expect(try JSONDecoder().decode(RpcRequest.self, from: WireCoding.encodeRequest(request)) == request)
        var changed = proof; changed.signature = "changed"
        #expect(!request.hasSameIntent(as: .migration(.abort(migrationId: proof.value.migrationId, confirmCleanup: true, sourceAbort: changed))))
        var record = MigrationTransferRecord(plan: source.value.plan, role: "source", phase: "relinquished",
            sourceGrant: source, destinationGrant: destination, offsets: [:], handoff: .object(["signature":.string("source")]),
            createdAt: 100, updatedAt: 300)
        #expect(!record.mayFinish && !record.mayRenew && !record.mayAbort)
        record.acceptance = .object(["signature":.string("destination")])
        #expect(record.mayFinish)
        record.role = "destination"
        #expect(!record.mayFinish && !record.mayRenew)
        record.handoff = nil; record.phase = "future_phase"
        #expect(!record.mayFinish && !record.mayRenew && !record.mayAbort)
    }

    @Test func queuedDestinationCleanupCannotBePresentedAsVerified() throws {
        let (_, _, _, source, destination, _) = try migrationFixture()
        let proof = SignedMigrationAbort(value: MigrationAbortReceipt(migrationId: source.value.plan.migrationId,
            planSha256: String(repeating: "8", count: 64), source: source.value.plan.source, abortedAt: 300), signature: "source-abort")
        var record = MigrationTransferRecord(plan: source.value.plan, role: "destination", phase: "cleaning",
            sourceGrant: source, destinationGrant: destination, offsets: [:], abortProof: proof, createdAt: 100, updatedAt: 300)
        #expect(DestinationCleanupState.review(record, plan: record.plan, sourceProof: proof) == .pending)
        record.phase = "aborted"
        #expect(DestinationCleanupState.review(record, plan: record.plan, sourceProof: proof) == .verified)
        record.abortProof = nil
        #expect(DestinationCleanupState.review(record, plan: record.plan, sourceProof: proof) == nil)
        record.abortProof = proof; record.abortProof?.signature = "other"
        #expect(DestinationCleanupState.review(record, plan: record.plan, sourceProof: proof) == nil)
        record.abortProof = proof; record.role = "source"
        #expect(DestinationCleanupState.review(record, plan: record.plan, sourceProof: proof) == nil)
        record.role = "destination"; record.phase = "future_cleanup"
        #expect(DestinationCleanupState.review(record, plan: record.plan, sourceProof: proof) == nil)
    }

    @Test func committedBlockedTransferCannotOfferAbort() throws {
        let (_, _, _, sourceGrant, targetGrant, _) = try migrationFixture()
        var record = MigrationTransferRecord(plan: sourceGrant.value.plan, role: "source", phase: "blocked",
            sourceGrant: sourceGrant, destinationGrant: targetGrant, offsets: ["root":UInt64.max,"cloudinit":4], handoff: .object(["value":.string("committed")]),
            createdAt: 100, updatedAt: 300, error: "uncertain reply")
        #expect(!record.mayAbort)
        #expect(record.transferredBytes == nil)
        record.handoff = nil
        #expect(record.mayAbort)
        record.phase = "future_phase"
        #expect(!record.mayAbort && !record.mayFinish)
    }

    @Test func enrollmentAndPartitionSelectionKeepTheirExactReviewedIntent() throws {
        let request = RpcRequest.enrollVm(EnrollVmRequest(requestId: UUID(), planId: "one", vmId: 9001, owner: "alice", confirmEnrollment: true))
        #expect(try JSONDecoder().decode(RpcRequest.self, from: WireCoding.encodeRequest(request)) == request)
        #expect(!request.hasSameIntent(as: .enrollVm(EnrollVmRequest(requestId: UUID(), planId: "two", vmId: 9001, owner: "alice", confirmEnrollment: true))))
        let uuid = "0D17BC71-00F4-4E70-8DC2-3E1214773333"
        let selection = RootPartitionSelection(text: "  " + uuid + "\n")
        #expect(selection.isValid && selection.uuid == uuid.lowercased())
        #expect(RootPartitionSelection().isValid && RootPartitionSelection().uuid == nil)
        #expect(!RootPartitionSelection(text: "root").isValid)
        let grow = RpcRequest.growVm(GrowVmRequest(requestId: UUID(), vmId: 9001, expectedDiskMib: 4096, targetDiskMib: 8192, confirmDowntime: true, rootPartitionUuid: selection.uuid))
        let decoded = try JSONDecoder().decode(RpcRequest.self, from: WireCoding.encodeRequest(grow))
        #expect(decoded == grow)
        #expect(!grow.hasSameIntent(as: .growVm(GrowVmRequest(requestId: UUID(), vmId: 9001, expectedDiskMib: 4096, targetDiskMib: 8192, confirmDowntime: true))))
        #expect(OperationStatus.cancelled.isRecognized && !OperationStatus.cancelled.isActive)
    }
}
