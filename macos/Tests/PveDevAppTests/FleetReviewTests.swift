import Foundation
import Testing

@testable import PveDevApp
@testable import PveDevCore

@Suite("Fleet action safety and allocation clarity")
struct FleetReviewTests {
  func dashboard(_ state: VmState) -> DashboardResult {
    let user = User(
      userId: "erin", role: .admin, budgetMib: 65536, minMemoryBudgetMib: 1024,
      maxMemoryBudgetMib: 8192, maxCores: 8, maxVms: 4, ready: true)
    let vm = Vm(
      vmId: 103, owner: "erin", name: "dev-heidi", diskMib: 768000, memoryMib: 32768,
      minMemoryMib: 12288, cores: 8, state: state)
    let allocation = StorageAllocation(
      id: "existing", owner: "erin", dataset: "rpool/data/dev-heidi", storage: "dev-heidi",
      budgetMib: 844800, vmIds: [103], acceptsNewVms: false, minMemoryBudgetMib: 12288,
      maxMemoryBudgetMib: 32768, maxCores: 8, maxVms: 1)
    return DashboardResult(
      identity: user, users: [user],
      budget: Budget(
        budgetMib: 65536, allocatedMib: 0, remainingMib: 65536, minMemoryAllocatedMib: 0,
        memoryAllocatedMib: 0, coresAllocated: 0), vms: [vm], operations: [], environment: "test",
      backend: .managed, capabilities: Array(DashboardResult.managedCapabilities),
      storageAllocations: [allocation])
  }
  @Test func duplicateRecoveryRowsHaveScopedIdentityAndQualifiedDestructiveConfirmation() throws {
    let a = ServerProfile(name: "HDD", profilePath: "/a")
    let b = ServerProfile(name: "SSD", profilePath: "/b")
    let rows = FleetVm.build(
      servers: [b, a], dashboards: [a.id: dashboard(.stopped), b.id: dashboard(.running)])
    #expect(rows.count == 2 && Set(rows.map(\.id)).count == 2)
    #expect(rows[0].serverId == a.id && rows[0].runningDuplicates == ["SSD"])
    #expect(rows[0].existing)
    #expect(rows[0].confirmationRequired(for: .stop) == "HDD/dev-heidi")
    #expect(rows[0].confirmationRequired(for: .delete) != "dev-heidi")
    #expect(rows[0].confirmationRequired(for: .delete) != rows[1].qualifiedName)
  }
  @Test func singleIecFormatterCoversBytesAndLargeVolumes() {
    #expect(Formatting.bytes(0) == "0 B")
    #expect(Formatting.bytes(1024) == "1 KiB")
    #expect(
      Formatting.bytes(1_649_267_441_664, locale: Locale(identifier: "en_US_POSIX")) == "1.5 TiB")
  }
}

private actor AdoptedRunner: ProcessRunning {
  let a: String, b: String
  private var peerState: VmState = .running
  private var peerFails = false
  func peer(_ state: VmState, fails: Bool = false) {
    peerState = state
    peerFails = fails
  }
  private(set) var mutations: [ProcessInvocation] = []
  init(a: String, b: String) {
    self.a = a
    self.b = b
  }
  func result<T: Encodable>(_ value: T) -> ProcessResult {
    let object = try! JSONSerialization.jsonObject(with: JSONEncoder().encode(value))
    return ProcessResult(
      terminationStatus: 0, terminatedBySignal: false,
      stdout: try! JSONSerialization.data(withJSONObject: ["ok": true, "result": object]),
      stderr: Data())
  }
  func run(_ invocation: ProcessInvocation) async throws(ProcessRunFailure) -> ProcessResult {
    let request = try! JSONDecoder().decode(RpcRequest.self, from: invocation.standardInput)
    switch request {
    case .dashboard:
      if invocation.arguments[1] == b && peerFails { throw .timedOut }
      return result(
        FleetReviewTests().dashboard(invocation.arguments[1] == a ? .stopped : peerState))
    case .vmAction(let action):
      mutations.append(invocation)
      return result(
        OperationRecord(
          operationId: "accepted", owner: "erin", requestId: WireUUID.string(action.requestId),
          kind: action.kind.rawValue, vmId: action.vmId, status: .queued, step: "queued",
          error: nil, createdAt: 1))
    default: throw .launchFailed("Unexpected test RPC")
    }
  }
}

extension FleetReviewTests {
  @Test @MainActor func adoptedActionsUseOriginalHostAndDuplicateStartIsRefused() async throws {
    let dir = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: false)
    let suite = "PveDevApp.adopted-review.\(UUID())"
    defer {
      UserDefaults(suiteName: suite)?.removePersistentDomain(forName: suite)
      try? FileManager.default.removeItem(at: dir)
    }
    let pa = dir.appendingPathComponent("a.json")
    let pb = dir.appendingPathComponent("b.json")
    try Data("{}".utf8).write(to: pa)
    try Data("{}".utf8).write(to: pb)
    let a = ServerProfile(
      name: "HDD", profilePath: pa.path,
      identity: ServerIdentity(
        serverId: String(repeating: "a", count: 64), environment: String(repeating: "b", count: 32),
        deviceId: String(repeating: "c", count: 64)), expectedBackend: .managed)
    let b = ServerProfile(
      name: "SSD", profilePath: pb.path,
      identity: ServerIdentity(
        serverId: String(repeating: "d", count: 64), environment: String(repeating: "e", count: 32),
        deviceId: String(repeating: "c", count: 64)), expectedBackend: .managed)
    var settings = AppSettings(executablePath: "/usr/bin/true")
    settings.servers = [a, b]
    settings.selectedServerId = a.id
    let store = SettingsStore(suiteName: suite)
    store.save(settings)
    let runner = AdoptedRunner(a: pa.path, b: pb.path)
    let model = AppModel(
      client: RpcClient(runner: runner), settingsStore: store,
      ledgerStore: LedgerStore(fileURL: dir.appendingPathComponent("ledger.json")))
    let oa = try #require(model.origin(forServer: a.id))
    let ob = try #require(model.origin(forServer: b.id))
    _ = try await model.loadDashboard(on: oa)
    _ = try await model.loadDashboard(on: ob)
    let row = try #require(model.fleetRows.first { $0.serverId == a.id })
    #expect(model.actionIssue(for: row, kind: .start, origin: oa) != nil)
    #expect(
      model.submit(
        .vmAction(VmActionRequest(requestId: UUID(), vmId: 103, kind: .start)), origin: oa) == nil)
    #expect(model.ledger.entries.isEmpty)
    _ = try await model.performMutation(
      .vmAction(VmActionRequest(requestId: UUID(), vmId: 103, kind: .shutdown)), origin: ob)
    #expect(model.selectedServerId == a.id)
    #expect(model.ledger.entries.isEmpty)
    #expect(model.notices.contains { $0.serverId == b.id && $0.kind == .success })
    let call = try #require(await runner.mutations.first)
    #expect(call.arguments[1] == b.profilePath)
    #expect(call.arguments.contains(try #require(b.identity).serverId))
  }
}

@MainActor
private final class SafetyFixture {
  let dir: URL
  let suite = "PveDevApp.partial-fleet.\(UUID())"
  let a: ServerProfile, b: ServerProfile
  let runner: AdoptedRunner
  let model: AppModel
  init() throws {
    dir = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: false)
    let pa = dir.appendingPathComponent("a.json")
    let pb = dir.appendingPathComponent("b.json")
    try Data("{}".utf8).write(to: pa)
    try Data("{}".utf8).write(to: pb)
    a = ServerProfile(
      name: "HDD", profilePath: pa.path,
      identity: ServerIdentity(
        serverId: String(repeating: "a", count: 64), environment: String(repeating: "b", count: 32),
        deviceId: String(repeating: "c", count: 64)), expectedBackend: .managed)
    b = ServerProfile(
      name: "SSD", profilePath: pb.path,
      identity: ServerIdentity(
        serverId: String(repeating: "d", count: 64), environment: String(repeating: "e", count: 32),
        deviceId: String(repeating: "c", count: 64)), expectedBackend: .managed)
    var settings = AppSettings(executablePath: "/usr/bin/true")
    settings.servers = [a, b]
    settings.selectedServerId = a.id
    let store = SettingsStore(suiteName: suite)
    store.save(settings)
    runner = AdoptedRunner(a: pa.path, b: pb.path)
    model = AppModel(
      client: RpcClient(runner: runner), settingsStore: store,
      ledgerStore: LedgerStore(fileURL: dir.appendingPathComponent("ledger.json")))
  }
  func clean() {
    UserDefaults(suiteName: suite)?.removePersistentDomain(forName: suite)
    try? FileManager.default.removeItem(at: dir)
  }
}

extension FleetReviewTests {
  @Test(arguments: ["missing", "failed", "stale", "paused"])
  @MainActor func startRefusesUnverifiedOrActivePeers(caseName: String) async throws {
    let f = try SafetyFixture()
    defer { f.clean() }
    let a = try #require(f.model.origin(forServer: f.a.id))
    let b = try #require(f.model.origin(forServer: f.b.id))
    _ = try await f.model.loadDashboard(on: a)
    if caseName != "missing" {
      await f.runner.peer(caseName == "paused" ? VmState(rawValue: "paused") : .stopped)
      _ = try await f.model.loadDashboard(on: b)
      if caseName == "failed" {
        await f.runner.peer(.stopped, fails: true)
        do {
          _ = try await f.model.loadDashboard(on: b)
          Issue.record("Expected peer failure")
        } catch {}
      } else if caseName == "stale" {
        f.model.sessions[f.b.id]?.lastRefreshedAt = Date().addingTimeInterval(-61)
      }
    }
    let row = try #require(f.model.fleetRows.first { $0.serverId == f.a.id })
    let issue = try #require(f.model.actionIssue(for: row, kind: .start, origin: a))
    #expect(issue.contains("SSD"))
    #expect(issue.contains(caseName == "paused" ? "same-named" : "Cannot verify"))
    #expect(
      f.model.submit(
        .vmAction(VmActionRequest(requestId: UUID(), vmId: 103, kind: .start)), origin: a) == nil)
    #expect(f.model.ledger.entries.isEmpty)
    #expect(await f.runner.mutations.isEmpty)
  }
  @Test func iecUnitsUseOneLocaleConvention() {
    let locale = Locale(identifier: "de_DE")
    #expect(Formatting.bytes(1_649_267_441_664, locale: locale) == "1,5 TiB")
    #expect(Formatting.mib(1_572_864, locale: locale) == "1,5 TiB")
  }
}


extension FleetReviewTests {
  @Test @MainActor func startIsAllowedWhenEveryPeerIsFreshAndStopped() async throws {
    let f = try SafetyFixture(); defer { f.clean() }
    let a = try #require(f.model.origin(forServer: f.a.id)), b = try #require(f.model.origin(forServer: f.b.id))
    await f.runner.peer(.stopped)
    _ = try await f.model.loadDashboard(on: a)
    _ = try await f.model.loadDashboard(on: b)
    let row = try #require(f.model.fleetRows.first { $0.serverId == f.a.id })
    #expect(f.model.actionIssue(for: row, kind: .start, origin: a) == nil)
    _ = try await f.model.performMutation(.vmAction(VmActionRequest(requestId: UUID(), vmId: 103, kind: .start)), origin: a)
    #expect(await f.runner.mutations.count == 1)
  }
}
