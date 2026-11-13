import Foundation
import PveDevCore
import Testing

@testable import PveDevApp

private let imageDigest = String(repeating: "a", count: 64)
private let guestKey =
  "ssh-ed25519 "
  + Data(
    [0, 0, 0, 11] + Array("ssh-ed25519".utf8) + [0, 0, 0, 32]
      + Array(repeating: UInt8(9), count: 32)
  ).base64EncodedString()
private let imageCatalog = ImageCatalog(images: [
  ApprovedImage(
    imageSha256: imageDigest, label: "Ubuntu24.04LTS", osVersion: "24.04", defaultUser: "ubuntu",
    minDiskMib: 4096)
])
private func managedResult(_ backend: BackendKind = .managed, ssd: Bool = false) -> DashboardResult
{
  let admin = User(
    userId: "erin", role: .admin, budgetMib: 8192, minMemoryBudgetMib: 512,
    maxMemoryBudgetMib: 4096, maxCores: 8, maxVms: 4, ready: true)
  let member = User(
    userId: "frank", role: .member, budgetMib: 8192, minMemoryBudgetMib: 512,
    maxMemoryBudgetMib: 4096, maxCores: 4, maxVms: 2, ready: true)
  let vm = Vm(
    vmId: 103, owner: "heidi", name: ssd ? "Active SSD Heidi" : "Stopped HDD recovery",
    diskMib: 1_310_720, memoryMib: 32768, minMemoryMib: 12288, cores: 8,
    state: ssd ? .running : .stopped)
  return DashboardResult(
    identity: admin, users: [admin, member],
    budget: Budget(
      budgetMib: 8192, allocatedMib: 0, remainingMib: 8192, minMemoryAllocatedMib: 0,
      memoryAllocatedMib: 0, coresAllocated: 0), vms: [], operations: [], environment: "managed",
    backend: backend,
    capabilities: Array(DashboardResult.managedCapabilities) + [
      "migration", "grow_vm", "shrink_vm",
    ],
    importedVms: [
      ImportedVm(
        vm: vm, label: ssd ? "Active SSD Heidi" : "Stopped HDD recovery",
        observedAt: 1_794_500_000, configurationSha256: imageDigest,
        reason: "Import only; mutations not qualified")
    ])
}
private func envelope<T: Encodable>(_ value: T) throws -> ProcessResult {
  let data = try JSONEncoder().encode(value)
  let object = try JSONSerialization.jsonObject(with: data)
  return ProcessResult(
    terminationStatus: 0, terminatedBySignal: false,
    stdout: try JSONSerialization.data(withJSONObject: ["ok": true, "result": object]),
    stderr: Data())
}
private actor ManagedRunner: ProcessRunning {
  private(set) var calls: [ProcessInvocation] = []
  private var waiter: CheckedContinuation<ProcessResult, Never>?
  var delayCatalog: Bool
  var backend: BackendKind
  init(delayCatalog: Bool = false, backend: BackendKind = .managed) {
    self.delayCatalog = delayCatalog
    self.backend = backend
  }
  func run(_ invocation: ProcessInvocation) async throws(ProcessRunFailure) -> ProcessResult {
    calls.append(invocation)
    let request = try! JSONDecoder().decode(RpcRequest.self, from: invocation.standardInput)
    switch request {
    case .dashboard:
      return try! envelope(
        managedResult(backend, ssd: invocation.arguments[1].hasSuffix("/b.json")))
    case .listImageCatalog:
      if delayCatalog { return await withCheckedContinuation { waiter = $0 } }
      return try! envelope(imageCatalog)
    default: throw .timedOut
    }
  }
  func catalogIsWaiting() -> Bool { waiter != nil }
  func finishCatalog(_ response: ProcessResult? = nil) {
    waiter?.resume(returning: response ?? (try! envelope(imageCatalog)))
    waiter = nil
  }
}
@MainActor
private final class ManagedFixture {
  let directory: URL
  let suite = "PveDevApp.managed-tests.\(UUID().uuidString)"
  let runner: ManagedRunner
  let model: AppModel
  let a: ServerProfile
  let b: ServerProfile
  init(delayCatalog: Bool = false, backend: BackendKind = .managed) throws {
    directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false)
    let pa = directory.appendingPathComponent("a.json")
    let pb = directory.appendingPathComponent("b.json")
    for p in [pa, pb] { try Data("{}".utf8).write(to: p) }
    a = ServerProfile(
      name: "Persistent HDD", profilePath: pa.path,
      identity: ServerIdentity(
        serverId: String(repeating: "a", count: 64), environment: String(repeating: "b", count: 32),
        deviceId: String(repeating: "c", count: 64)), expectedBackend: .managed)
    b = ServerProfile(
      name: "Persistent SSD", profilePath: pb.path,
      identity: ServerIdentity(
        serverId: String(repeating: "d", count: 64), environment: String(repeating: "e", count: 32),
        deviceId: String(repeating: "f", count: 64)), expectedBackend: .managed)
    var settings = AppSettings(executablePath: "/usr/bin/true")
    settings.servers = [a, b]
    settings.selectedServerId = a.id
    let store = SettingsStore(suiteName: suite)
    store.save(settings)
    runner = ManagedRunner(delayCatalog: delayCatalog, backend: backend)
    model = AppModel(
      client: RpcClient(runner: runner), settingsStore: store,
      ledgerStore: LedgerStore(fileURL: directory.appendingPathComponent("pending.json")))
  }
  func load() async throws { _ = try await model.loadDashboard(on: #require(model.requestOrigin)) }
  func waitForCatalog() async throws {
    for _ in 0..<200 {
      if await runner.catalogIsWaiting() { return }
      try await Task.sleep(for: .milliseconds(5))
    }
    throw NSError(domain: "Catalog invocation missing", code: 1)
  }
  func clean() {
    model.setPolling(false)
    UserDefaults(suiteName: suite)?.removePersistentDomain(forName: suite)
    try? FileManager.default.removeItem(at: directory)
  }
  func request() -> RpcRequest {
    .createVm(
      CreateVmRequest(
        requestId: UUID(), name: "dev-frank", diskMib: 8192, memoryMib: 1024, minMemoryMib: 512,
        cores: 1, owner: "frank", guest: GuestSpec(imageSha256: imageDigest, sshPublicKey: guestKey)
      ))
  }
}
@Suite("Persistent managed controller routing")
@MainActor
struct ManagedRoutingTests {
  @Test func catalogReadUsesPinnedCurrentControllerAndDoesNotWriteLedger() async throws {
    let f = try ManagedFixture()
    defer { f.clean() }
    try await f.load()
    #expect(try await f.model.loadImageCatalog(origin: f.model.requestOrigin) == imageCatalog)
    let call = try #require(await f.runner.calls.last)
    #expect(call.arguments.contains(try #require(f.a.identity).serverId))
    #expect(call.arguments.contains(try #require(f.a.identity).deviceId))
    #expect(
      try JSONDecoder().decode(RpcRequest.self, from: call.standardInput) == .listImageCatalog)
    #expect(f.model.ledger.entries.isEmpty)
    #expect(
      !FileManager.default.fileExists(
        atPath: f.directory.appendingPathComponent("pending.json").path))
  }
  @Test func selectionChangePreservesCatalogOnOriginalRouteAndDoesNotRetargetCreate() async throws {
    let f = try ManagedFixture(delayCatalog: true)
    defer { f.clean() }
    try await f.load()
    let origin = f.model.requestOrigin
    let read = Task { try await f.model.loadImageCatalog(origin: origin) }
    try await f.waitForCatalog()
    f.model.selectServer(f.b.id)
    await f.runner.finishCatalog()
    #expect(try await read.value == imageCatalog)
    let id = try #require(f.model.submit(f.request(), origin: origin))
    #expect(f.model.ledger.entry(id)?.serverId == f.a.id)
    #expect(f.model.selectedServerId == f.b.id)
    #expect(f.model.dashboard == nil)
  }
  @Test(arguments: ["profile", "identity", "executable", "mode"])
  func sourceRouteChangeInvalidatesCatalogAndForm(field: String) async throws {
    let f = try ManagedFixture(delayCatalog: true)
    defer { f.clean() }
    try await f.load()
    let origin = f.model.requestOrigin
    let read = Task { try await f.model.loadImageCatalog(origin: origin) }
    try await f.waitForCatalog()
    var settings = f.model.settings
    switch field {
    case "profile": settings.servers[0].profilePath = f.b.profilePath
    case "identity": settings.servers[0].identity = f.b.identity
    case "executable": settings.executablePath = "/bin/echo"
    default:
      settings.servers[0].mode = .observation
      settings.servers[0].identity = nil
    }
    f.model.applySettings(settings)
    await f.runner.finishCatalog()
    do {
      _ = try await read.value
      Issue.record("Changed route must fail")
    } catch let error as RpcFailure {
      guard case .configuration = error else {
        Issue.record("Expected changed route")
        return
      }
    }
    #expect(f.model.submit(f.request(), origin: origin) == nil)
    #expect(f.model.ledger.entries.isEmpty)
  }
  @Test func normalAdminCreatesForMemberWithExactDurableGuestPayload() async throws {
    let f = try ManagedFixture()
    defer { f.clean() }
    try await f.load()
    _ = try await f.model.loadImageCatalog(origin: f.model.requestOrigin)
    #expect(f.model.permissions.canChooseVmOwner)
    let request = f.request()
    let id = try #require(f.model.submit(request, origin: f.model.requestOrigin))
    for _ in 0..<200 {
      if !f.model.isMutationInFlight(id) { break }
      try await Task.sleep(for: .milliseconds(5))
    }
    let entry = try #require(f.model.ledger.entry(id))
    #expect(entry.request == request && entry.phase == .indeterminate && entry.serverId == f.a.id)
    let saved = try LedgerStore(fileURL: f.directory.appendingPathComponent("pending.json")).load()
    #expect(saved.entry(id)?.request == request)
    let calls = await f.runner.calls
    let sent = calls.compactMap {
      try? JSONDecoder().decode(RpcRequest.self, from: $0.standardInput)
    }.filter(\.isMutation)
    #expect(sent == [request])
  }
  @Test func unsupportedManagedAndImportedActionsNeverEnterLedgerOrSend() async throws {
    let f = try ManagedFixture()
    defer { f.clean() }
    try await f.load()
    let before = await f.runner.calls.count
    let origin = f.model.requestOrigin
    let requests: [RpcRequest] = [
      f.request(),
      .vmAction(VmActionRequest(requestId: UUID(), vmId: 103, kind: .start)),
      .vmAction(VmActionRequest(requestId: UUID(), vmId: 103, kind: .delete)),
      .growVm(
        GrowVmRequest(
          requestId: UUID(), vmId: 9201, expectedDiskMib: 4096, targetDiskMib: 8192,
          confirmDowntime: true)),
      .migration(.abort(migrationId: "test", confirmCleanup: true)),
    ]
    for r in requests { #expect(f.model.submit(r, origin: origin) == nil) }
    do {
      let _: ImageCatalog = try await f.model.query(
        .planVmGrowth(vmId: 103, targetDiskMib: 8192), origin: origin, as: ImageCatalog.self)
      Issue.record("Imported plan must be blocked")
    } catch {}
    #expect(f.model.ledger.entries.isEmpty)
    #expect(await f.runner.calls.count == before)
  }
  @Test func importedVm103RemainsDistinctAcrossPersistentControllerSessions() async throws {
    let f = try ManagedFixture()
    defer { f.clean() }
    try await f.load()
    let hdd = try #require(f.model.dashboard?.importedVms.first)
    #expect(hdd.vm.vmId == 103 && hdd.vm.state == .stopped && hdd.label == "Stopped HDD recovery")
    f.model.selectServer(f.b.id)
    try await f.load()
    let ssd = try #require(f.model.dashboard?.importedVms.first)
    #expect(ssd.vm.vmId == 103 && ssd.vm.state == .running && ssd.label == "Active SSD Heidi")
    #expect(hdd.scopedId(serverId: f.a.id) != ssd.scopedId(serverId: f.b.id))
    #expect(f.model.dashboard?.budget.allocatedMib == 0)
    f.model.selectServer(f.a.id)
    #expect(f.model.dashboard?.importedVms.first == hdd)
    #expect(f.model.dashboard?.vms.isEmpty == true)
  }
  @Test func wrongBackendCannotUseManagedProfileAsLegacyRoute() async throws {
    let f = try ManagedFixture(backend: .lab)
    defer { f.clean() }
    // Read-only dashboard exposes the mismatch for diagnosis, but no action follows it.
    try await f.load()
    #expect(f.model.submit(f.request(), origin: f.model.requestOrigin) == nil)
    do {
      _ = try await f.model.loadImageCatalog(origin: f.model.requestOrigin)
      Issue.record("Legacy route cannot serve managed catalog")
    } catch {}
    #expect(f.model.ledger.entries.isEmpty)
  }
}
