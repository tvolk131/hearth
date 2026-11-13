import Foundation
import PveDevCore
import Testing

@testable import PveDevApp

private func observedData(_ label: String, host: String) throws -> Data {
  let hostId = String(repeating: host, count: 64)
  return try JSONSerialization.data(withJSONObject: [
    "schema": "pve-dev-observation-result/1",
    "identity": [
      "host_id": hostId, "binding": hostId, "node": label,
      "host_key_sha256": "SHA256:" + String(repeating: "A", count: 43),
      "machine_id_sha256": String(repeating: "c", count: 64), "zpool_guid": "42",
      "zfs_root": "rpool/data",
      "known_hosts_sha256": String(repeating: "d", count: 64),
    ],
    "captured_at": "2026-11-11T00:00:00Z", "received_at_unix_ms": 100,
    "snapshot_digest": String(repeating: "e", count: 64),
    "collector_sha256": String(repeating: "f", count: 64),
    "pve_version": "pve-manager/9.2", "pool_name": "rpool", "pool_health": "ONLINE",
    "root_used_bytes": 4096, "root_available_bytes": 8192,
    "vms": [
      [
        "id": hostId + "/103", "vmid": 103, "name": label, "status": "stopped",
        "cores": 8, "memory_mib": 32768, "balloon_mib": 12288, "disk_bytes": 4096,
        "note": "Recovery copy", "pending_changes": false,
      ]
    ], "datasets": [],
    "container_count": 0, "warnings": [], "managed": false, "authorizes_management": false,
    "creates_controller_state": false,
  ])
}

private actor ReadOnlyRunner: ProcessRunning {
  private(set) var calls: [ProcessInvocation] = []
  let outputs: [String: Data]
  init(outputs: [String: Data]) { self.outputs = outputs }
  func run(_ invocation: ProcessInvocation) async throws(ProcessRunFailure) -> ProcessResult {
    calls.append(invocation)
    guard invocation.arguments.first == "observe", let bytes = outputs[invocation.arguments[2]]
    else {
      return ProcessResult(
        terminationStatus: 70, terminatedBySignal: false, stdout: Data(),
        stderr: Data("Unexpected non-observation call".utf8))
    }
    return ProcessResult(
      terminationStatus: 0, terminatedBySignal: false, stdout: bytes, stderr: Data())
  }
}

@MainActor
private final class ObservationFixture {
  let directory: URL
  let suite = "PveDevApp.observation-routing.\(UUID().uuidString)"
  let runner: ReadOnlyRunner
  let model: AppModel
  let a: ServerProfile
  let b: ServerProfile
  let controller: ServerProfile
  init() throws {
    directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false)
    let pa = directory.appendingPathComponent("a.json")
    let pb = directory.appendingPathComponent("b.json")
    let pc = directory.appendingPathComponent("controller.json")
    for path in [pa, pb, pc] { try Data("{}".utf8).write(to: path) }
    let da = try observedData("HDD recovery", host: "a")
    let db = try observedData("SSD active", host: "b")
    a = ServerProfile(
      name: "HDD", profilePath: pa.path, mode: .observation,
      observationIdentity: try JSONDecoder().decode(ObservationResult.self, from: da).identity)
    b = ServerProfile(
      name: "SSD", profilePath: pb.path, mode: .observation,
      observationIdentity: try JSONDecoder().decode(ObservationResult.self, from: db).identity)
    controller = ServerProfile(
      name: "Lab", profilePath: pc.path,
      identity: ServerIdentity(
        serverId: String(repeating: "1", count: 64), environment: String(repeating: "2", count: 32),
        deviceId: String(repeating: "3", count: 64)))
    runner = ReadOnlyRunner(outputs: [pa.path: da, pb.path: db])
    var settings = AppSettings(executablePath: "/usr/bin/true")
    settings.inventoryExecutablePath = "/usr/bin/true"
    settings.servers = [a, b, controller]
    settings.selectedServerId = a.id
    let store = SettingsStore(suiteName: suite)
    store.save(settings)
    model = AppModel(
      client: RpcClient(runner: runner), settingsStore: store,
      ledgerStore: LedgerStore(fileURL: directory.appendingPathComponent("pending.json")),
      observationClient: ObservationClient(runner: runner))
  }
  func settleRefresh() async throws {
    for _ in 0..<200 {
      if !model.isRefreshing { return }
      try await Task.sleep(for: .milliseconds(5))
    }
    throw NSError(domain: "Observation refresh did not settle", code: 1)
  }
  func clean() {
    model.setPolling(false)
    UserDefaults(suiteName: suite)?.removePersistentDomain(forName: suite)
    try? FileManager.default.removeItem(at: directory)
  }
}

@Suite("Read-only host routing and mutation refusal")
@MainActor
struct ObservationRoutingTests {
  @Test func sameVmIdOnTwoObservedHostsKeepsIndependentCache() async throws {
    let f = try ObservationFixture()
    defer { f.clean() }
    f.model.refresh()
    try await f.settleRefresh()
    let first = try #require(f.model.observation?.vms.first)
    #expect(first.vmid == 103)
    #expect(first.name == "HDD recovery")
    f.model.selectServer(f.b.id)
    #expect(f.model.observation == nil)
    f.model.refresh()
    try await f.settleRefresh()
    let second = try #require(f.model.observation?.vms.first)
    #expect(second.vmid == 103 && second.id != first.id)
    #expect(second.name == "SSD active")
    f.model.selectServer(f.a.id)
    #expect(f.model.observation?.vms.first?.name == "HDD recovery")
    #expect(f.model.dashboard == nil)
    #expect(!f.model.permissions.isAdmin)
  }

  @Test func replacedControllerFormAndMatchingVmCannotMutateObservedHost() async throws {
    let f = try ObservationFixture()
    defer { f.clean() }
    f.model.selectServer(f.controller.id)
    let oldOrigin = f.model.requestOrigin
    var settings = f.model.settings
    settings.servers[2].identity = nil
    f.model.applySettings(settings)
    f.model.selectServer(f.a.id)
    let request = RpcRequest.vmAction(VmActionRequest(requestId: UUID(), vmId: 103, kind: .delete))
    #expect(f.model.submit(request, origin: oldOrigin) == nil)
    #expect(f.model.submit(request, origin: f.model.requestOrigin) == nil)
    #expect(f.model.ledger.entries.isEmpty)
    #expect(
      !FileManager.default.fileExists(
        atPath: f.directory.appendingPathComponent("pending.json").path))
    f.model.retry(UUID())
    let calls = await f.runner.calls
    #expect(calls.isEmpty)
    #expect(f.model.isConfigured)
  }

  @Test func policyEditsCannotSendOrCreateLedgerOnReadOnlyOrUnprivilegedRoute() async throws {
    let f = try ObservationFixture()
    defer { f.clean() }
    let policy = ResourcePolicy(
      budgetMib: 64, minMemoryBudgetMib: 256, maxMemoryBudgetMib: 1024, maxCores: 4, maxVms: 4)
    let request = RpcRequest.updateUserPolicy(
      UpdateUserPolicyRequest(
        requestId: UUID(), userId: "erin", expectedRevision: 0, policy: policy))
    #expect(f.model.submit(request, origin: f.model.requestOrigin) == nil)
    f.model.selectServer(f.controller.id)
    #expect(f.model.submit(request, origin: f.model.requestOrigin) == nil)
    #expect(f.model.ledger.entries.isEmpty)
    #expect(
      !FileManager.default.fileExists(
        atPath: f.directory.appendingPathComponent("pending.json").path))
    #expect(await f.runner.calls.isEmpty)
  }

  @Test func accountDeletionCannotSendFromReadOnlyOrUnprivilegedRoute() async throws {
    let f = try ObservationFixture()
    defer { f.clean() }
    let request = RpcRequest.deleteUser(
      DeleteUserRequest(requestId: UUID(), userId: "trial", expectedRevision: 0))
    #expect(f.model.submit(request, origin: f.model.requestOrigin) == nil)
    f.model.selectServer(f.controller.id)
    #expect(f.model.submit(request, origin: f.model.requestOrigin) == nil)
    #expect(f.model.ledger.entries.isEmpty)
    #expect(
      !FileManager.default.fileExists(
        atPath: f.directory.appendingPathComponent("pending.json").path))
    #expect(await f.runner.calls.isEmpty)
  }

  @Test func diskShrinkCannotCreateLedgerOrSendFromReadOnlyOrUnprivilegedRoute() async throws {
    for request in [
      RpcRequest.shrinkVm(
        ShrinkVmRequest(
          requestId: UUID(), vmId: 9001, expectedDiskMib: 4096, targetDiskMib: 3072,
          confirmDowntime: true)),
      .finishVmShrink(
        FinishVmShrinkRequest(
          requestId: UUID(), vmId: 9001, preparationId: "operation", confirmGuestVerified: true,
          confirmDeleteRecovery: true)),
      .growVm(
        GrowVmRequest(
          requestId: UUID(), vmId: 9001, expectedDiskMib: 4096, targetDiskMib: 8192,
          confirmDowntime: true)),
      .recoverOperation(
        RecoverOperationRequest(
          requestId: UUID(), operationId: "one", inspectionId: "proof", kind: .abortPreparation,
          confirmRecovery: true)),
      .enrollVm(
        EnrollVmRequest(
          requestId: UUID(), planId: "one", vmId: 9001, owner: "alice", confirmEnrollment: true)),
      .migration(.abort(migrationId: "one", confirmCleanup: true)),
    ] {
      let f = try ObservationFixture()
      defer { f.clean() }
      #expect(f.model.submit(request, origin: f.model.requestOrigin) == nil)
      f.model.selectServer(f.controller.id)
      #expect(f.model.submit(request, origin: f.model.requestOrigin) == nil)
      #expect(f.model.ledger.entries.isEmpty)
      #expect(
        !FileManager.default.fileExists(
          atPath: f.directory.appendingPathComponent("pending.json").path))
      #expect(await f.runner.calls.isEmpty)
    }
  }

  @Test func replacingObservedTrustInvalidatesCacheAndOrigin() async throws {
    let f = try ObservationFixture()
    defer { f.clean() }
    f.model.refresh()
    try await f.settleRefresh()
    let origin = f.model.requestOrigin
    var updated = f.model.settings
    updated.servers[0].observationIdentity = f.b.observationIdentity
    f.model.applySettings(updated)
    #expect(f.model.observation == nil)
    #expect(f.model.requestOrigin != origin)
    f.model.selectServer(f.controller.id)
    #expect(!f.model.isReadOnly)
    #expect(f.model.observation == nil)
  }

  @Test func controllerPendingRetryCannotCrossIntoReadOnlyHost() async throws {
    let f = try ObservationFixture()
    defer { f.clean() }
    f.model.selectServer(f.controller.id)
    let request = RpcRequest.vmAction(VmActionRequest(requestId: UUID(), vmId: 103, kind: .delete))
    let pendingId = try #require(f.model.submit(request, origin: f.model.requestOrigin))
    for _ in 0..<200 {
      if !f.model.isMutationInFlight(pendingId) { break }
      try await Task.sleep(for: .milliseconds(5))
    }
    // Completion also refreshes the origin server; settle it before counting
    // calls so a permitted background dashboard read is not blamed on retry.
    try await f.settleRefresh()
    let pending = try #require(f.model.ledger.entry(pendingId))
    let ledgerPath = f.directory.appendingPathComponent("pending.json")
    let before = try Data(contentsOf: ledgerPath)
    let previousCalls = await f.runner.calls.count
    f.model.selectServer(f.a.id)
    #expect(!f.model.canRetry(pending))
    f.model.retry(pendingId)
    let calls = await f.runner.calls.count
    #expect(calls == previousCalls)
    #expect(try Data(contentsOf: ledgerPath) == before)
    #expect(f.model.ledger.entry(pendingId)?.serverId == f.controller.id)
    #expect(f.model.pendingForCurrentProfile.isEmpty)
  }
}
