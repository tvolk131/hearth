import Foundation
import Testing
@testable import PveDevCore

private let digest = "04e8f2461a16a44e78a370284471f29616cdd9baf1893df25807b71f985b6c94"
private let publicKey = "ssh-ed25519 " + Data([0,0,0,11] + Array("ssh-ed25519".utf8) + [0,0,0,32] + Array(repeating: UInt8(7), count: 32)).base64EncodedString()
private let approved = ApprovedImage(imageSha256: digest, label: "Ubuntu24.04LTS", osVersion: "24.04", defaultUser: "ubuntu", minDiskMib: 4096)
private func managedDashboard(imported: [ImportedVm] = [], admin: Bool = true, capabilities: [String]? = nil) -> DashboardResult {
    let user = User(userId: "erin", role: admin ? .admin : .member, budgetMib: 8192, minMemoryBudgetMib: 1024, maxMemoryBudgetMib: 8192, maxCores: 8, maxVms: 4, ready: true)
    return DashboardResult(identity: user, users: [user], budget: Budget(budgetMib: 8192, allocatedMib: 0, remainingMib: 8192,
        minMemoryAllocatedMib: 0, memoryAllocatedMib: 0, coresAllocated: 0), vms: [], operations: [], environment: "managed",
        backend: .managed, capabilities: capabilities ?? Array(DashboardResult.managedCapabilities), importedVms: imported)
}

@Suite("Managed boot image and imported metadata contract")
struct ManagedCreationTests {
    func draft() -> CreateVmDraft {
        var d = CreateVmDraft(); d.name = "dev-frank"; d.diskMib = "8192"; d.memoryMib = "1024"; d.minMemoryMib = "512"
        d.owner = "frank"; d.imageSha256 = digest; d.sshPublicKey = publicKey + " frank@laptop"
        return d
    }
    @Test func approvedSelectionCanonicalKeyAndAdminOwnerSurviveWireAndLedger() throws {
        let request = try draft().build(requestId: UUID(), allowOwner: true, requireGuest: true, catalog: ImageCatalog(images: [approved])).get()
        guard case .createVm(let value) = request else { Issue.record("Expected create"); return }
        #expect(value.owner == "frank")
        #expect(value.guest == GuestSpec(imageSha256: digest, sshPublicKey: publicKey))
        let wire = try WireCoding.encodeRequest(request)
        #expect(try JSONDecoder().decode(RpcRequest.self, from: wire) == request)
        let json = try #require(JSONSerialization.jsonObject(with: wire) as? [String: Any])
        #expect((json["guest"] as? [String: String]) == ["image_sha256": digest, "ssh_public_key": publicKey])
        #expect(json["images"] == nil)
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: dir) }
        let store = LedgerStore(fileURL: dir.appendingPathComponent("pending.json"))
        let identity = ServerIdentity(serverId: String(repeating: "a", count: 64), environment: String(repeating: "b", count: 32), deviceId: String(repeating: "c", count: 64))
        let server = UUID(); var ledger = MutationLedger()
        let entry = ledger.prepare(request, profilePath: "/tmp/profile.json", serverId: server, identity: identity)
        _ = try ledger.beginAttempt(entry.id, profilePath: "/tmp/profile.json", serverId: server, identity: identity)
        try store.save(ledger)
        var restored = try store.load()
        #expect(try restored.beginAttempt(entry.id, profilePath: "/tmp/profile.json", serverId: server, identity: identity) == request)
        var changed = value; changed.guest?.sshPublicKey = publicKey + "different"
        #expect(!request.hasSameIntent(as: .createVm(changed)))
    }
    @Test func legacyCreateOmitsGuestAndDecodesWithoutChangingIntent() throws {
        let request = RpcRequest.createVm(CreateVmRequest(requestId: UUID(), name: "legacy", diskMib: 32, memoryMib: 128, minMemoryMib: 128, cores: 1, owner: nil))
        let bytes = try WireCoding.encodeRequest(request)
        #expect(!String(decoding: bytes, as: UTF8.self).contains("guest"))
        #expect(try JSONDecoder().decode(RpcRequest.self, from: bytes) == request)
        #expect(managedDashboard().requestIssue(request, catalog: ImageCatalog(images: [approved])) != nil)
        #expect(!RpcRequest.listImageCatalog.isMutation && RpcRequest.listImageCatalog.requestId == nil)
        #expect(String(decoding: try WireCoding.encodeRequest(.listImageCatalog), as: UTF8.self) == #"{"action":"list_image_catalog"}"#)
    }
    @Test func defaultUserLabelAndMinimumDiskComeFromCatalog() throws {
        let bytes = try JSONEncoder().encode(ImageCatalog(images: [approved]))
        let catalog = try JSONDecoder().decode(ImageCatalog.self, from: bytes)
        #expect(catalog.images[0].displayLabel == "Ubuntu24.04LTS · 24.04")
        #expect(catalog.images[0].accessLabel == "Default SSH user: ubuntu")
        var d = draft(); d.diskMib = "4095"
        #expect(throws: DraftIssues.self) { try d.build(requestId: UUID(), allowOwner: true, requireGuest: true, catalog: catalog).get() }
        d.diskMib = "4096"
        #expect(try d.build(requestId: UUID(), allowOwner: true, requireGuest: true, catalog: catalog).get().isMutation)
    }
    @Test func unavailableOrUnknownCatalogNeverBecomesAnImageSelection() throws {
        #expect(try JSONDecoder().decode(ImageCatalog.self, from: Data("[]".utf8)).images.isEmpty)
        for json in [#"["ubuntu"]"#, #"{"images":[]}"#, #"[{"image_sha256":"x","label":"Ubuntu","os_version":"24.04","default_user":"ubuntu","min_disk_mib":4096}]"#] {
            #expect(throws: DecodingError.self) { try JSONDecoder().decode(ImageCatalog.self, from: Data(json.utf8)) }
        }
        let data = try JSONEncoder().encode(approved)
        var extra = try #require(JSONSerialization.jsonObject(with: data) as? [String: Any]); extra["url"] = "https://untrusted.example/image"
        #expect(throws: DecodingError.self) { try JSONDecoder().decode(ImageCatalog.self, from: JSONSerialization.data(withJSONObject: [extra])) }
        #expect(throws: DecodingError.self) { try JSONDecoder().decode(ImageCatalog.self, from: JSONEncoder().encode([approved, approved])) }
        for catalog in [nil, ImageCatalog(images: [])] {
            #expect(throws: DraftIssues.self) { try draft().build(requestId: UUID(), allowOwner: true, requireGuest: true, catalog: catalog).get() }
        }
    }
    @Test func keyRejectsOptionsOtherAlgorithmsMalformedBlobsAndMultipleLines() {
        #expect(GuestPublicKey.canonicalEd25519(publicKey + " comment") == publicKey)
        for key in ["command=whoami " + publicKey, publicKey + "\n" + publicKey, "ssh-rsa AAAA", "ssh-ed25519 AAAA", "", "ssh-ed25519 " + Data(repeating: 1, count: 51).base64EncodedString()] {
            #expect(GuestPublicKey.canonicalEd25519(key) == nil)
        }
    }
    @Test func managedCapabilitiesCannotEnableUnqualifiedRoutesOrLegacyFallback() throws {
        let d = managedDashboard(capabilities: Array(DashboardResult.managedCapabilities) + ["migration", "shrink_vm", "grow_vm", "enroll_vm"])
        #expect(!d.supports("migration") && !d.supports("shrink_vm", legacyLabFallback: true))
        #expect(d.requestIssue(.migration(.abort(migrationId: "one", confirmCleanup: true)), catalog: nil) != nil)
        let request = try draft().build(requestId: UUID(), allowOwner: true, requireGuest: true, catalog: ImageCatalog(images: [approved])).get()
        #expect(d.requestIssue(request, catalog: ImageCatalog(images: [approved])) == nil)
        #expect(managedDashboard(admin: false).requestIssue(request, catalog: ImageCatalog(images: [approved])) != nil)
        #expect(!managedDashboard(capabilities: []).canCreateManagedVm)
        var unknown = d; unknown.backend = BackendKind(rawValue: "future_controller")
        #expect(unknown.requestIssue(request, catalog: nil) != nil)
        #expect(!unknown.supports("create_vm") && !unknown.canCreateManagedVm)
    }
    @Test func qualifiedPowerActionsRequireCurrentReservedManagedVmAndMappedCapability() {
        var d = managedDashboard()
        let vm = Vm(vmId: 9201, owner: "frank", name: "new-frank", diskMib: 8192, memoryMib: 1024, minMemoryMib: 512, cores: 1, state: .stopped)
        d.vms = [vm]
        #expect(d.supports("create_vm") && d.capabilities.contains("create_guest"))
        #expect(d.supports("delete_user") && d.capabilities.contains("delete_empty_user"))
        #expect(d.availableActions(for: vm) == [.start, .delete])
        #expect(d.requestIssue(.vmAction(VmActionRequest(requestId: UUID(), vmId: 9201, kind: .start)), catalog: nil) == nil)
        for id: UInt16 in [101, 102, 103, 9202, 9300] {
            #expect(d.requestIssue(.vmAction(VmActionRequest(requestId: UUID(), vmId: id, kind: .delete)), catalog: nil) != nil)
        }
        // Lost start acknowledgements remain retryable after the VM becomes running.
        d.vms[0].state = .running
        #expect(d.requestIssue(.vmAction(VmActionRequest(requestId: UUID(), vmId: 9201, kind: .start)), catalog: nil) == nil)
        d.capabilities.removeAll { $0 == "start_new_vm" }
        #expect(!d.availableActions(for: vm).contains(.start))
        #expect(d.requestIssue(.vmAction(VmActionRequest(requestId: UUID(), vmId: 9201, kind: .start)), catalog: nil) != nil)
    }
    @Test func managedRecoveryIsBoundToKnownNewVmStartAndNarrowAction() throws {
        var d = managedDashboard()
        d.vms = [Vm(vmId: 9201, owner: "setup-check", name: "fresh", diskMib: 4096, memoryMib: 1024, minMemoryMib: 512, cores: 1, state: .blocked)]
        let json = #"{"operation_id":"failed-start","owner":"setup-check","request_id":"r","kind":"start","vm_id":9201,"status":"blocked","step":"start","error":"failed","created_at":1}"#
        d.operations = [try JSONDecoder().decode(OperationRecord.self, from: Data(json.utf8))]
        let inspect = RpcRequest.inspectOperation(operationId: "failed-start")
        let acknowledge = RpcRequest.recoverOperation(RecoverOperationRequest(requestId: UUID(), operationId: "failed-start", inspectionId: "review", kind: .acknowledgeFailedStart, confirmRecovery: true))
        #expect(d.requestIssue(inspect, catalog: nil) == nil)
        #expect(d.requestIssue(acknowledge, catalog: nil) == nil)
        #expect(d.requestIssue(.recoverOperation(RecoverOperationRequest(requestId: UUID(), operationId: "failed-start", inspectionId: "review", kind: .resumeVerifiedEffect, confirmRecovery: true)), catalog: nil) != nil)
        #expect(d.requestIssue(.inspectOperation(operationId: "unknown"), catalog: nil) != nil)
        d.vms[0].owner = "someone-else"
        #expect(d.requestIssue(acknowledge, catalog: nil) != nil)
        d.vms[0].owner = "setup-check"; d.identity.role = .member
        #expect(d.requestIssue(inspect, catalog: nil) != nil)
        d.identity.role = .admin; d.capabilities.removeAll { $0 == "recover_operation" }
        #expect(d.requestIssue(acknowledge, catalog: nil) != nil)
    }
    @Test func importedRowsStaySeparateUnchargedAndNeverAcquireActions() throws {
        let vm = Vm(vmId: 103, owner: "heidi", name: "HDD recovery", diskMib: 262144, memoryMib: 32768, minMemoryMib: 12288, cores: 8, state: .stopped)
        let row = ImportedVm(vm: vm, label: "Stopped HDD recovery", observedAt: 1794500000, configurationSha256: digest, reason: "Imported without changing guest/config/storage; legacy mutation not qualified")
        let d = managedDashboard(imported: [row])
        let decoded = try JSONDecoder().decode(DashboardResult.self, from: JSONEncoder().encode(d))
        #expect(decoded.importedVms == [row] && decoded.vms.isEmpty)
        #expect(decoded.budget.allocatedMib == 0 && OwnedUsage(vms: decoded.vms, owner: "heidi").activeVmCount == 0)
        #expect(decoded.availableActions(for: vm).isEmpty)
        for kind in VmActionKind.allCases {
            #expect(decoded.requestIssue(.vmAction(VmActionRequest(requestId: UUID(), vmId: 103, kind: kind)), catalog: nil) != nil)
        }
        #expect(row.scopedId(serverId: UUID()) != row.scopedId(serverId: UUID()))
        #expect(row.recoveryDescription.contains("recovery"))
        #expect(row.observedAt == 1794500000 && row.vm.state == .stopped)
    }
}
