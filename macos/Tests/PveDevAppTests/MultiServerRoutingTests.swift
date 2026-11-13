import Foundation
import PveDevCore
import Testing

@testable import PveDevApp

private actor ControlledRunner: ProcessRunning {
  private(set) var invocations: [ProcessInvocation] = []
  private var waiters:
    [String: CheckedContinuation<Result<ProcessResult, ProcessRunFailure>, Never>] = [:]
  func run(_ invocation: ProcessInvocation) async throws(ProcessRunFailure) -> ProcessResult {
    invocations.append(invocation)
    let path = invocation.arguments[1]
    let body = String(data: invocation.standardInput, encoding: .utf8) ?? ""
    let kind =
      invocation.arguments.last == "profile-info"
      ? "info"
      : (body.contains("dashboard")
        ? "dashboard" : (body.contains("host_inventory") ? "inventory" : "mutation"))
    let result = await withCheckedContinuation { waiters[path + "|" + kind] = $0 }
    return try result.get()
  }
  func pending(_ path: String, _ kind: String) -> Bool { waiters[path + "|" + kind] != nil }
  func finish(_ path: String, _ kind: String, _ result: Result<ProcessResult, ProcessRunFailure>) {
    waiters.removeValue(forKey: path + "|" + kind)?.resume(returning: result)
  }
}

private func response(_ json: String) -> ProcessResult {
  ProcessResult(
    terminationStatus: 0, terminatedBySignal: false, stdout: Data(json.utf8), stderr: Data())
}
private func dashboard(_ user: String, admin: Bool = true, capabilities: [String] = [])
  -> ProcessResult
{
  let encodedCapabilities = String(decoding: try! JSONEncoder().encode(capabilities), as: UTF8.self)
  return response(
    """
    {"ok":true,"result":{"identity":{"user_id":"\(user)","role":"\(admin ? "admin" : "member")","budget_mib":4096,"min_memory_budget_mib":256,"max_memory_budget_mib":8192,"max_cores":8,"max_vms":4,"ready":true},"users":[],"vms":[],"operations":[],"budget":{"budget_mib":4096,"allocated_mib":0,"remaining_mib":4096,"min_memory_allocated_mib":0,"memory_allocated_mib":0,"cores_allocated":0},"environment":"test","backend":"lab","capabilities":\(encodedCapabilities)}}
    """)
}

@MainActor
private final class Fixture {
  let directory: URL
  let suite = "PveDevApp.routing.\(UUID().uuidString)"
  let a: ServerProfile
  let b: ServerProfile
  let runner = ControlledRunner()
  let model: AppModel
  init(sharedDevice: Bool = false) throws {
    directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false)
    let pa = directory.appendingPathComponent("a.json")
    let pb = directory.appendingPathComponent("b.json")
    try Data("{}".utf8).write(to: pa)
    try Data("{}".utf8).write(to: pb)
    a = ServerProfile(
      name: "HDD", profilePath: pa.path,
      identity: ServerIdentity(
        serverId: String(repeating: "a", count: 64), environment: String(repeating: "b", count: 32),
        deviceId: String(repeating: "c", count: 64)))
    b = ServerProfile(
      name: "SSD", profilePath: pb.path,
      identity: ServerIdentity(
        serverId: String(repeating: "d", count: 64), environment: String(repeating: "e", count: 32),
        deviceId: String(repeating: sharedDevice ? "c" : "f", count: 64)))
    var settings = AppSettings(executablePath: "/usr/bin/true")
    settings.servers = [a, b]
    settings.selectedServerId = a.id
    let store = SettingsStore(suiteName: suite)
    store.save(settings)
    model = AppModel(
      client: RpcClient(runner: runner), settingsStore: store,
      ledgerStore: LedgerStore(fileURL: directory.appendingPathComponent("pending.json")))
  }
  func clean() {
    model.setPolling(false)
    UserDefaults(suiteName: suite)?.removePersistentDomain(forName: suite)
    try? FileManager.default.removeItem(at: directory)
  }
  func waitFor(_ path: String, _ kind: String) async throws {
    for _ in 0..<200 {
      if await runner.pending(path, kind) { return }
      try await Task.sleep(for: .milliseconds(5))
    }
    throw NSError(domain: "Missing CLI invocation: \(kind)", code: 1)
  }
  func settle() async throws { try await Task.sleep(for: .milliseconds(25)) }
}

@Suite("Independent server sessions")
@MainActor
struct MultiServerRoutingTests {
  private func inventory(_ node: String) -> ProcessResult {
    response(
      """
      {"ok":true,"result":{"schema":1,"node":"\(node)","captured_at":"2026-11-12T12:00:00Z","cpu_model":"test CPU","logical_cpus":16,"cpu_sockets":1,"memory_total_bytes":137438953472,"memory_used_bytes":1073741824,"uptime_seconds":100,"pve_version":"test","pool_size_bytes":1099511627776,"pool_allocated_bytes":1073741824,"pool_free_bytes":1098437885952,"block_devices":[],"zpool_topology":"mirror-0","datasets":[],"storage_allocations":[]}}
      """)
  }

  @Test func fleetHardwareReplyStaysWithItsOriginAndDoesNotCreatePendingWork() async throws {
    let f = try Fixture()
    defer { f.clean() }
    let origin = try #require(f.model.origin(forServer: f.a.id))
    let loading = Task { await f.model.loadHost(on: origin) }
    try await f.waitFor(f.a.profilePath, "dashboard")
    await f.runner.finish(f.a.profilePath, "dashboard", .success(dashboard("erin")))
    try await f.waitFor(f.a.profilePath, "inventory")
    f.model.selectServer(f.b.id)
    await f.runner.finish(f.a.profilePath, "inventory", .success(inventory("pve1")))
    await loading.value
    #expect(f.model.inventory(forServer: f.a.id)?.node == "pve1")
    #expect(f.model.inventory(forServer: f.b.id) == nil)
    #expect(f.model.selectedServerId == f.b.id)
    #expect(f.model.unresolvedCount == 0)
  }

  @Test func replacedProfileCannotReceiveLateHardwareInventory() async throws {
    let f = try Fixture()
    defer { f.clean() }
    let origin = try #require(f.model.origin(forServer: f.a.id))
    let loading = Task { await f.model.loadHost(on: origin) }
    try await f.waitFor(f.a.profilePath, "dashboard")
    await f.runner.finish(f.a.profilePath, "dashboard", .success(dashboard("erin")))
    try await f.waitFor(f.a.profilePath, "inventory")
    var settings = f.model.settings
    settings.servers[0].identity = ServerIdentity(
      serverId: String(repeating: "1", count: 64),
      environment: String(repeating: "2", count: 32), deviceId: String(repeating: "3", count: 64))
    f.model.applySettings(settings)
    await f.runner.finish(f.a.profilePath, "inventory", .success(inventory("wrong-old-host")))
    await loading.value
    #expect(f.model.inventory(forServer: f.a.id) == nil)
    #expect(f.model.dashboard(forServer: f.a.id) == nil)
    #expect(f.model.inventoryFailure(forServer: f.a.id) == nil)
  }
  @Test func refreshFinishesOnOriginAfterSelectionChanges() async throws {
    let f = try Fixture()
    defer { f.clean() }
    f.model.refresh()
    try await f.waitFor(f.a.profilePath, "dashboard")
    f.model.selectServer(f.b.id)
    #expect(f.model.dashboard == nil)
    #expect(!f.model.isRefreshing)
    f.model.refresh()
    try await f.waitFor(f.b.profilePath, "dashboard")
    await f.runner.finish(f.b.profilePath, "dashboard", .success(dashboard("frank", admin: false)))
    try await f.settle()
    #expect(f.model.dashboard?.identity.userId == "frank")
    #expect(!f.model.permissions.isAdmin)
    await f.runner.finish(f.a.profilePath, "dashboard", .success(dashboard("erin")))
    try await f.settle()
    #expect(f.model.dashboard?.identity.userId == "frank")
    #expect(f.model.refreshFailure == nil)
    f.model.selectServer(f.a.id)
    #expect(f.model.dashboard?.identity.userId == "erin")
    #expect(f.model.permissions.isAdmin)
  }

  @Test func hostFailureAndPollingDoNotLeakIntoOtherHost() async throws {
    let f = try Fixture()
    defer { f.clean() }
    f.model.setPolling(true)
    try await f.waitFor(f.a.profilePath, "dashboard")
    f.model.selectServer(f.b.id)
    #expect(!f.model.settings.pollingEnabled)
    await f.runner.finish(f.a.profilePath, "dashboard", .failure(.launchFailed("A failed")))
    try await f.settle()
    #expect(f.model.refreshFailure == nil)
    #expect(f.model.dashboard == nil)
    f.model.selectServer(f.a.id)
    #expect(f.model.settings.pollingEnabled)
    #expect(f.model.refreshFailure == .launchFailed("A failed"))
    f.model.setPolling(false)
  }

  @Test func replacementIdentityDropsLateDashboardAndOldRetry() async throws {
    let f = try Fixture()
    defer { f.clean() }
    let request = RpcRequest.vmAction(VmActionRequest(requestId: UUID(), vmId: 9001, kind: .stop))
    let pending = try #require(f.model.submit(request, origin: f.model.requestOrigin))
    try await f.waitFor(f.a.profilePath, "mutation")
    f.model.refresh()
    await f.runner.finish(f.a.profilePath, "mutation", .failure(.timedOut))
    try await f.waitFor(f.a.profilePath, "dashboard")
    try await f.settle()
    var changed = f.model.settings
    changed.servers[0].identity = f.b.identity
    // Avoid a deliberately duplicate profile binding.
    changed.servers.removeLast()
    f.model.applySettings(changed)
    #expect(f.model.pendingForCurrentProfile.isEmpty)
    #expect(f.model.pendingForOtherProfiles.count == 1)
    #expect(!f.model.isVmBusy(9001))
    let count = await f.runner.invocations.count
    f.model.retry(pending)
    try await f.settle()
    #expect(await f.runner.invocations.count == count)
    await f.runner.finish(f.a.profilePath, "dashboard", .success(dashboard("old-admin")))
    try await f.settle()
    #expect(f.model.dashboard == nil)
    #expect(f.model.refreshFailure == nil)
  }

  @Test func mutationCapturesOriginAndSameVmOnOtherServerStaysAvailable() async throws {
    let f = try Fixture()
    defer { f.clean() }
    let request = RpcRequest.vmAction(VmActionRequest(requestId: UUID(), vmId: 9001, kind: .stop))
    let id = try #require(f.model.submit(request, origin: f.model.requestOrigin))
    try await f.waitFor(f.a.profilePath, "mutation")
    #expect(f.model.isVmBusy(9001))
    f.model.selectServer(f.b.id)
    #expect(!f.model.isVmBusy(9001))
    #expect(f.model.pendingForCurrentProfile.isEmpty)
    #expect(f.model.pendingForOtherProfiles.map(\.id) == [id])
    let invocation = try #require(await f.runner.invocations.first)
    #expect(invocation.arguments.contains(try #require(f.a.identity).serverId))
    #expect(!invocation.arguments.contains(try #require(f.b.identity).serverId))
    await f.runner.finish(f.a.profilePath, "mutation", .failure(.timedOut))
    try await f.waitFor(f.a.profilePath, "dashboard")
    #expect(await f.runner.pending(f.b.profilePath, "dashboard") == false)
    #expect(f.model.notices.contains { $0.serverId == f.a.id })
    #expect(!f.model.isRefreshing)
    let count = await f.runner.invocations.count
    f.model.retry(id)  // Selected B cannot retry A.
    try await f.settle()
    #expect(await f.runner.invocations.count == count)
    await f.runner.finish(f.a.profilePath, "dashboard", .success(dashboard("erin")))
    try await f.settle()
    f.model.selectServer(f.a.id)
    #expect(f.model.canRetry(try #require(f.model.ledger.entry(id))))
    f.model.retry(id)
    try await f.waitFor(f.a.profilePath, "mutation")
    let repeated = try #require(await f.runner.invocations.last)
    #expect(repeated.standardInput == invocation.standardInput)
    await f.runner.finish(f.a.profilePath, "mutation", .failure(.timedOut))
    try await f.waitFor(f.a.profilePath, "dashboard")
    await f.runner.finish(f.a.profilePath, "dashboard", .success(dashboard("erin")))
    try await f.settle()
  }

  @Test(arguments: ["identity", "profile", "executable"])
  func staleConfirmationCannotSubmitToReplacementRoute(change: String) async throws {
    let f = try Fixture()
    defer { f.clean() }
    let capturedOrigin = try #require(f.model.requestOrigin)
    let delete = RpcRequest.vmAction(VmActionRequest(requestId: UUID(), vmId: 9001, kind: .delete))
    var replacement = f.model.settings
    switch change {
    case "identity":
      replacement.servers[0].identity = f.b.identity
      replacement.servers.removeLast()
    case "profile": replacement.servers[0].profilePath = f.b.profilePath
    case "executable": replacement.executablePath = "/bin/true"
    default: replacement.selectedServerId = f.b.id
    }
    f.model.applySettings(replacement)
    #expect(f.model.requestOrigin != capturedOrigin)
    #expect(f.model.submit(delete, origin: capturedOrigin) == nil)
    #expect(f.model.ledger.entries.isEmpty)
    try await f.settle()
    #expect(await f.runner.invocations.isEmpty)
  }

  @Test func staleSettingsDraftCannotRestoreAnOldSelectionOrProfile() throws {
    let f = try Fixture()
    defer { f.clean() }
    var draft = f.model.settings
    let baseline = f.model.settingsRevision
    draft.servers[0].name = "Old draft renamed"
    f.model.selectServer(f.b.id)
    #expect(!f.model.applySettings(draft, expectedRevision: baseline))
    #expect(f.model.selectedServerId == f.b.id)
    #expect(f.model.settings.servers[0].name == "HDD")
    let refreshedBaseline = f.model.settingsRevision
    draft = f.model.settings
    draft.servers[1].name = "New SSD name"
    #expect(f.model.applySettings(draft, expectedRevision: refreshedBaseline))
    #expect(f.model.selectedServer?.name == "New SSD name")
  }

  @Test func profileChangeDuringInspectionDoesNotInstallStaleTrust() async throws {
    let f = try Fixture()
    defer { f.clean() }
    let inspect = Task { try await f.model.pinServer(f.a.id) }
    try await f.waitFor(f.a.profilePath, "info")
    var settings = f.model.settings
    settings.servers[0].profilePath = f.b.profilePath
    f.model.applySettings(settings)
    await f.runner.finish(
      f.a.profilePath, "info",
      .success(
        ProcessResult(
          terminationStatus: 0, terminatedBySignal: false,
          stdout: try JSONEncoder().encode(f.a.identity), stderr: Data())))
    do {
      try await inspect.value
      Issue.record("Stale inspection must fail")
    } catch { #expect(f.model.selectedServer?.identity == nil) }
    #expect(f.model.selectedServer?.profilePath == f.b.profilePath)
  }

  @Test func unpinnedProfileCannotSendUntilExplicitLocalInspection() async throws {
    let f = try Fixture()
    defer { f.clean() }
    var settings = f.model.settings
    settings.servers[0].identity = nil
    f.model.applySettings(settings)
    f.model.refresh()
    #expect(!f.model.isConfigured)
    #expect(await f.runner.invocations.isEmpty)
    let inspection = Task { try await f.model.pinServer(f.a.id) }
    try await f.waitFor(f.a.profilePath, "info")
    let identity = try #require(f.a.identity)
    await f.runner.finish(
      f.a.profilePath, "info",
      .success(
        ProcessResult(
          terminationStatus: 0, terminatedBySignal: false,
          stdout: try JSONEncoder().encode(identity), stderr: Data())))
    try await inspection.value
    #expect(f.model.isConfigured)
    #expect(await f.runner.invocations.count == 1)
    #expect(f.model.selectedServer?.identity == identity)
    f.model.refresh()
    try await f.waitFor(f.a.profilePath, "dashboard")
    await f.runner.finish(f.a.profilePath, "dashboard", .success(dashboard("erin")))
    try await f.settle()
  }
  @Test func readOnlyQueryDropsResultAfterServerSelectionChanges() async throws {
    let f = try Fixture()
    defer { f.clean() }
    let origin = f.model.requestOrigin
    let query = Task {
      try await f.model.query(.dashboard, origin: origin, as: DashboardResult.self)
    }
    try await f.waitFor(f.a.profilePath, "dashboard")
    f.model.selectServer(f.b.id)
    await f.runner.finish(f.a.profilePath, "dashboard", .success(dashboard("old-admin")))
    do {
      _ = try await query.value
      Issue.record("Stale planning response must fail")
    } catch let error as RpcFailure {
      guard case .configuration = error else {
        Issue.record("Expected stale-origin rejection")
        return
      }
    }
    #expect(f.model.dashboard == nil)
    #expect(f.model.ledger.entries.isEmpty)
  }

  @Test func readOnlyQueryCannotBypassMutationLedger() async throws {
    let f = try Fixture()
    defer { f.clean() }
    let request = RpcRequest.vmAction(VmActionRequest(requestId: UUID(), vmId: 9001, kind: .delete))
    do {
      let _: OperationRecord = try await f.model.query(
        request, origin: f.model.requestOrigin, as: OperationRecord.self)
      Issue.record("Mutation query must fail before sending")
    } catch let error as RpcFailure {
      guard case .configuration = error else {
        Issue.record("Expected query-mode rejection")
        return
      }
    }
    #expect(await f.runner.invocations.isEmpty)
    #expect(f.model.ledger.entries.isEmpty)
  }

  @Test func newMutationRequiresAdvertisedCapability() async throws {
    let f = try Fixture()
    defer { f.clean() }
    f.model.refresh()
    try await f.waitFor(f.a.profilePath, "dashboard")
    await f.runner.finish(f.a.profilePath, "dashboard", .success(dashboard("erin")))
    try await f.settle()
    #expect(f.model.permissions.isAdmin)
    let before = await f.runner.invocations.count
    let request = RpcRequest.growVm(
      GrowVmRequest(
        requestId: UUID(), vmId: 9001,
        expectedDiskMib: 4096, targetDiskMib: 8192, confirmDowntime: true))
    #expect(f.model.submit(request, origin: f.model.requestOrigin) == nil)
    try await f.settle()
    #expect(await f.runner.invocations.count == before)
    #expect(f.model.ledger.entries.isEmpty)
  }

  @Test func awaitedMutationUsesReviewedDestinationWithoutChangingSelection() async throws {
    let f = try Fixture()
    defer { f.clean() }
    let origin = try #require(f.model.origin(forServer: f.b.id))
    let load = Task { try await f.model.loadDashboard(on: origin) }
    try await f.waitFor(f.b.profilePath, "dashboard")
    await f.runner.finish(
      f.b.profilePath, "dashboard", .success(dashboard("target-admin", capabilities: ["grow_vm"])))
    _ = try await load.value
    let request = RpcRequest.growVm(
      GrowVmRequest(
        requestId: UUID(), vmId: 9001, expectedDiskMib: 4096, targetDiskMib: 8192,
        confirmDowntime: true))
    let send = Task { try await f.model.performMutation(request, origin: origin) }
    try await f.waitFor(f.b.profilePath, "mutation")
    #expect(f.model.selectedServerId == f.a.id)
    let entry = try #require(f.model.ledger.entries.first)
    #expect(entry.serverId == f.b.id && entry.phase == .inFlight)
    #expect(f.model.isMutationInFlight(entry.id))
    let invocation = try #require(await f.runner.invocations.last)
    #expect(invocation.arguments.contains(try #require(f.b.identity).serverId))
    await f.runner.finish(f.b.profilePath, "mutation", .failure(.timedOut))
    do {
      _ = try await send.value
      Issue.record("Expected uncertain acknowledgement")
    } catch let error as RpcFailure { #expect(error == .timedOut(seconds: 60)) }
    #expect(f.model.ledger.entries.first?.phase == .indeterminate)
    #expect(f.model.ledger.entries.first?.request == request)
    #expect(f.model.selectedServerId == f.a.id)
    try await f.waitFor(f.b.profilePath, "dashboard")
    await f.runner.finish(
      f.b.profilePath, "dashboard", .success(dashboard("target-admin", capabilities: ["grow_vm"])))
    try await f.settle()
  }

  @Test func awaitedAcknowledgementCannotContinueAfterDestinationIdentityReplacement() async throws
  {
    let f = try Fixture()
    defer { f.clean() }
    let origin = try #require(f.model.origin(forServer: f.b.id))
    let load = Task { try await f.model.loadDashboard(on: origin) }
    try await f.waitFor(f.b.profilePath, "dashboard")
    await f.runner.finish(
      f.b.profilePath, "dashboard", .success(dashboard("target-admin", capabilities: ["grow_vm"])))
    _ = try await load.value
    let request = RpcRequest.growVm(
      GrowVmRequest(
        requestId: UUID(), vmId: 9001, expectedDiskMib: 4096, targetDiskMib: 8192,
        confirmDowntime: true))
    let send = Task { try await f.model.performMutation(request, origin: origin) }
    try await f.waitFor(f.b.profilePath, "mutation")
    var changed = f.model.settings
    changed.servers[1].identity = ServerIdentity(
      serverId: String(repeating: "9", count: 64), environment: String(repeating: "8", count: 32),
      deviceId: String(repeating: "7", count: 64))
    f.model.applySettings(changed)
    await f.runner.finish(
      f.b.profilePath, "mutation",
      .success(
        response(
          #"{"ok":true,"result":{"operation_id":"old-operation","owner":"target-admin","request_id":"one","kind":"grow_vm","vm_id":9001,"status":"queued","step":"queued","error":null,"created_at":1}}"#
        )))
    do {
      _ = try await send.value
      Issue.record("Acknowledged old route must not continue a workflow")
    } catch let error as RpcFailure {
      guard case .configuration = error else {
        Issue.record("Expected changed-origin rejection")
        return
      }
    }
    #expect(f.model.ledger.entries.isEmpty)
    #expect(f.model.dashboard(forServer: f.b.id) == nil)
    #expect(f.model.selectedServerId == f.a.id)
  }

  @Test func selectedHostDoesNotRetargetOriginSubmitAndFleetNoticesAreVisible() async throws {
    let f = try Fixture()
    defer { f.clean() }
    let origin = try #require(f.model.origin(forServer: f.b.id))
    let id = try #require(
      f.model.submit(
        .vmAction(VmActionRequest(requestId: UUID(), vmId: 9001, kind: .stop)), origin: origin))
    try await f.waitFor(f.b.profilePath, "mutation")
    #expect(f.model.selectedServerId == f.a.id)
    #expect(f.model.ledger.entry(id)?.serverId == f.b.id)
    await f.runner.finish(f.b.profilePath, "mutation", .failure(.timedOut))
    try await f.waitFor(f.b.profilePath, "dashboard")
    #expect(f.model.notices.contains { $0.serverId == f.b.id })
    f.model.fleetPageActive = false
    #expect(!f.model.notices.contains { $0.serverId == f.b.id })
    await f.runner.finish(f.b.profilePath, "dashboard", .success(dashboard("erin")))
  }
  @Test func cancelledRefreshIsSilentAndDashboardSuccessClearsFailure() async throws {
    let f = try Fixture()
    defer { f.clean() }
    f.model.refresh()
    try await f.waitFor(f.a.profilePath, "dashboard")
    f.model.cancelRefresh()
    await f.runner.finish(f.a.profilePath, "dashboard", .failure(.cancelled))
    try await f.settle()
    #expect(f.model.refreshFailure == nil)
    f.model.refresh()
    try await f.waitFor(f.a.profilePath, "dashboard")
    await f.runner.finish(f.a.profilePath, "dashboard", .failure(.timedOut))
    try await f.settle()
    #expect(f.model.refreshFailure != nil)
    let read = Task { try await f.model.loadDashboard(on: #require(f.model.requestOrigin)) }
    try await f.waitFor(f.a.profilePath, "dashboard")
    await f.runner.finish(f.a.profilePath, "dashboard", .success(dashboard("erin")))
    _ = try await read.value
    #expect(f.model.refreshFailure == nil)
  }
  @Test func sameDeviceOtherHostQueuedMutationCancellationNeverLaunches() async throws {
    let f = try Fixture(sharedDevice: true)
    defer { f.clean() }
    let a = try #require(f.model.requestOrigin)
    let b = try #require(f.model.origin(forServer: f.b.id))
    let read = Task { try await f.model.loadDashboard(on: a) }
    try await f.waitFor(f.a.profilePath, "dashboard")
    let send = Task {
      try await f.model.performMutation(
        .vmAction(VmActionRequest(requestId: UUID(), vmId: 9001, kind: .stop)), origin: b)
    }
    try await f.settle()
    let entry = try #require(f.model.ledger.entries.first)
    #expect(f.model.mutationSendStates[entry.id] == .queued)
    #expect(!(await f.runner.pending(f.b.profilePath, "mutation")))
    f.model.cancelMutation(entry.id)
    do {
      _ = try await send.value
      Issue.record("Queued cancellation succeeded")
    } catch { #expect(error as? RpcFailure == .cancelled) }
    #expect(f.model.ledger.entry(entry.id)?.attempts == 0)
    #expect(f.model.ledger.entry(entry.id)?.phase == .indeterminate)
    #expect(!(await f.runner.pending(f.b.profilePath, "mutation")))
    await f.runner.finish(f.a.profilePath, "dashboard", .success(dashboard("erin")))
    _ = try await read.value
  }

  @Test func failedHostBackgroundReadsPauseUntilExplicitRefresh() async throws {
    let f = try Fixture(sharedDevice: true)
    defer { f.clean() }
    f.model.refreshAll()
    // Dictionary/task scheduling does not guarantee which host starts first.
    for _ in 0..<200 {
      if await f.runner.pending(f.a.profilePath, "dashboard") {
        await f.runner.finish(f.a.profilePath, "dashboard", .failure(.timedOut))
      }
      if await f.runner.pending(f.b.profilePath, "dashboard") {
        await f.runner.finish(f.b.profilePath, "dashboard", .success(dashboard("erin")))
      }
      if !f.model.fleetIsRefreshing { break }
      try await Task.sleep(for: .milliseconds(5))
    }
    #expect(f.model.failure(forServer: f.a.id) != nil)
    let count = await f.runner.invocations.count
    f.model.refreshAll(onlyStale: true)
    try await f.settle()
    #expect(await f.runner.invocations.count == count)
    f.model.refresh()
    try await f.waitFor(f.a.profilePath, "dashboard")
    await f.runner.finish(f.a.profilePath, "dashboard", .success(dashboard("erin")))
    try await f.settle()
    #expect(f.model.failure(forServer: f.a.id) == nil)
  }

  @Test(arguments: ["identity", "profile", "remove"])
  func queuedMutationRechecksRouteBeforeLaunching(change: String) async throws {
    let f = try Fixture(sharedDevice: true)
    defer { f.clean() }
    let a = try #require(f.model.requestOrigin)
    let b = try #require(f.model.origin(forServer: f.b.id))
    let hold = Task { try await f.model.loadDashboard(on: a) }
    try await f.waitFor(f.a.profilePath, "dashboard")
    let send = Task {
      try await f.model.performMutation(
        .vmAction(VmActionRequest(requestId: UUID(), vmId: 9001, kind: .stop)), origin: b)
    }
    try await f.settle()
    let entry = try #require(f.model.inFlightEntry(vmId: 9001, on: f.b.id))
    #expect(f.model.mutationSendStates[entry.id] == .queued)
    var settings = f.model.settings
    if change == "remove" {
      settings.servers.removeLast()
    } else if change == "profile" {
      settings.servers[1].profilePath = f.a.profilePath
    } else {
      settings.servers[1].identity = ServerIdentity(
        serverId: String(repeating: "8", count: 64), environment: String(repeating: "9", count: 32),
        deviceId: String(repeating: "c", count: 64))
    }
    f.model.applySettings(settings)
    await f.runner.finish(f.a.profilePath, "dashboard", .success(dashboard("erin")))
    _ = try await hold.value
    do {
      _ = try await send.value
      Issue.record("Changed queued route must not send")
    } catch { #expect(error as? RpcFailure == .cancelled) }
    #expect(f.model.ledger.entry(entry.id)?.attempts == 0)
    #expect(f.model.ledger.entry(entry.id)?.request == entry.request)
    #expect(!(await f.runner.pending(f.b.profilePath, "mutation")))
    #expect(f.model.notices.contains { $0.serverId == nil && $0.title.contains("Not sent") })
  }

  @Test func postMutationRefreshSurvivesConcurrentHardwareLoad() async throws {
    let f = try Fixture(sharedDevice: true)
    defer { f.clean() }
    let b = try #require(f.model.origin(forServer: f.b.id))
    f.model.refreshServer(f.b.id)
    try await f.waitFor(f.b.profilePath, "dashboard")
    await f.runner.finish(f.b.profilePath, "dashboard", .success(dashboard("erin")))
    try await f.waitFor(f.b.profilePath, "inventory")
    let send = Task {
      try await f.model.performMutation(
        .vmAction(VmActionRequest(requestId: UUID(), vmId: 9001, kind: .stop)), origin: b)
    }
    try await f.settle()
    await f.runner.finish(f.b.profilePath, "inventory", .success(inventory("pve2")))
    try await f.waitFor(f.b.profilePath, "mutation")
    // A second hardware load waits in the same device queue behind the mutation.
    f.model.refreshServer(f.b.id)
    try await f.settle()
    await f.runner.finish(
      f.b.profilePath, "mutation",
      .success(
        response(
          "{\"ok\":true,\"result\":{\"operation_id\":\"op\",\"owner\":\"erin\",\"request_id\":\"r\",\"kind\":\"stop\",\"vm_id\":9001,\"status\":\"queued\",\"step\":\"queued\",\"created_at\":1}}"
        )))
    _ = try await send.value
    try await f.waitFor(f.b.profilePath, "inventory")
    await f.runner.finish(f.b.profilePath, "inventory", .success(inventory("pve2")))
    try await f.waitFor(f.b.profilePath, "dashboard")
    await f.runner.finish(f.b.profilePath, "dashboard", .success(dashboard("erin")))
    try await f.settle()
    #expect(!f.model.refreshing(f.b.id))
    #expect(f.model.failure(forServer: f.b.id) == nil)
  }

  @Test func legacySheetTargetNeverRebindsItsVmToNewSelection() throws {
    let f = try Fixture()
    defer { f.clean() }
    let a = try #require(f.model.requestOrigin)
    let vm = Vm(
      vmId: 9001, owner: "erin", name: "same-name", diskMib: 4096, memoryMib: 1024,
      minMemoryMib: 512, cores: 1, state: .stopped)
    let target = LegacyVmTarget(vm: vm, origin: a)
    f.model.selectServer(f.b.id)
    #expect(target.originForReview(current: f.model.requestOrigin) == nil)
    #expect(target.vm == vm && target.origin == a)
    #expect(
      f.model.submit(
        .vmAction(VmActionRequest(requestId: UUID(), vmId: 9001, kind: .delete)),
        origin: target.originForReview(current: f.model.requestOrigin)) == nil)
  }

  @Test func queuedStartRechecksPeersAtLaunch() async throws {
    let f = try Fixture(sharedDevice: true)
    defer { f.clean() }
    func managed(_ state: VmState) throws -> ProcessResult {
      let value = try JSONSerialization.jsonObject(
        with: JSONEncoder().encode(FleetReviewTests().dashboard(state)))
      return ProcessResult(
        terminationStatus: 0, terminatedBySignal: false,
        stdout: try JSONSerialization.data(withJSONObject: ["ok": true, "result": value]),
        stderr: Data())
    }
    let a = try #require(f.model.origin(forServer: f.a.id))
    let b = try #require(f.model.origin(forServer: f.b.id))
    for (origin, path) in [(a, f.a.profilePath), (b, f.b.profilePath)] {
      let load = Task { try await f.model.loadDashboard(on: origin) }
      try await f.waitFor(path, "dashboard")
      await f.runner.finish(path, "dashboard", .success(try managed(.stopped)))
      _ = try await load.value
    }
    let hold = Task {
      try await f.model.queryOnRoute(.hostInventory, origin: a, as: HostInventory.self)
    }
    try await f.waitFor(f.a.profilePath, "inventory")
    let start = Task {
      try await f.model.performMutation(
        .vmAction(VmActionRequest(requestId: UUID(), vmId: 103, kind: .start)), origin: a)
    }
    try await f.settle()
    let entry = try #require(f.model.inFlightEntry(vmId: 103, on: f.a.id))
    #expect(f.model.mutationSendStates[entry.id] == .queued)
    f.model.sessions[f.b.id]?.dashboard = FleetReviewTests().dashboard(.running)
    await f.runner.finish(f.a.profilePath, "inventory", .success(inventory("pve1")))
    _ = try await hold.value
    do {
      _ = try await start.value
      Issue.record("Duplicate appeared while queued")
    } catch { #expect(error as? RpcFailure == .cancelled) }
    #expect(f.model.ledger.entry(entry.id)?.attempts == 0)
    #expect(!(await f.runner.pending(f.a.profilePath, "mutation")))
    #expect(
      f.model.notices.contains { $0.title.contains("Not sent") && $0.detail.contains("same-named") }
    )
  }

  @Test func refusedOriginNoticeBelongsToItsOwnServer() throws {
    let f = try Fixture()
    defer { f.clean() }
    let b = try #require(f.model.origin(forServer: f.b.id))
    let request = RpcRequest.deleteUser(
      DeleteUserRequest(requestId: UUID(), userId: "ivan", expectedRevision: 0))
    #expect(f.model.submit(request, origin: b) == nil)
    #expect(f.model.notices.contains { $0.serverId == f.b.id && $0.title.contains("SSD") })
    #expect(f.model.ledger.entries.isEmpty)
  }
  @Test func hardwareFailureDoesNotPauseHealthyDashboardPolling() async throws {
    let f = try Fixture()
    defer { f.clean() }
    f.model.refreshServer(f.b.id)
    try await f.waitFor(f.b.profilePath, "dashboard")
    await f.runner.finish(f.b.profilePath, "dashboard", .success(dashboard("erin")))
    try await f.waitFor(f.b.profilePath, "inventory")
    await f.runner.finish(f.b.profilePath, "inventory", .failure(.timedOut))
    try await f.settle()
    #expect(f.model.sessions[f.b.id]?.inventoryBackgroundPaused == true)
    #expect(f.model.sessions[f.b.id]?.backgroundPaused == false)
    f.model.selectServer(f.b.id)
    f.model.fleetPageActive = false
    f.model.setPolling(true)
    try await f.waitFor(f.b.profilePath, "dashboard")
    await f.runner.finish(f.b.profilePath, "dashboard", .success(dashboard("erin")))
    f.model.setPolling(false)
  }

}
