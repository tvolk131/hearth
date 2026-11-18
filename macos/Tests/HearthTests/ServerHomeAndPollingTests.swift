import Foundation
import Testing

@testable import Hearth
@testable import HearthCore

/// Answers immediately: dashboards per profile path with a configurable role,
/// optional failures, and a record of every invocation.
private actor HomeRunner: ProcessRunning {
  private(set) var calls: [(path: String, action: String)] = []
  private var roles: [String: UserRole] = [:]
  private var failing: Set<String> = []
  /// Dashboard reads per path that still report an operation in this status.
  private var pending: [String: (status: OperationStatus, reads: Int)] = [:]
  func setPending(_ status: OperationStatus, reads: Int, for path: String) {
    pending[path] = (status, reads)
  }
  func setRole(_ role: UserRole, for path: String) { roles[path] = role }
  func setFailing(_ fails: Bool, for path: String) {
    if fails { failing.insert(path) } else { failing.remove(path) }
  }
  func count(_ path: String, _ action: String) -> Int {
    calls.filter { $0.path == path && $0.action == action }.count
  }
  func count(action: String) -> Int { calls.filter { $0.action == action }.count }

  func run(_ invocation: ProcessInvocation) async throws(ProcessRunFailure) -> ProcessResult {
    guard invocation.arguments.first == "--profile" else {
      calls.append((invocation.arguments.dropFirst(2).first ?? "", "observe"))
      throw .launchFailed("Unexpected observation")
    }
    let path = invocation.arguments[1]
    let request = try! JSONDecoder().decode(RpcRequest.self, from: invocation.standardInput)
    switch request {
    case .dashboard:
      calls.append((path, "dashboard"))
      if failing.contains(path) { throw .timedOut }
      var dashboard = Self.dashboard(role: roles[path] ?? .member)
      if let (status, reads) = pending[path] {
        let still = reads > 0
        if still { pending[path] = (status, reads - 1) }
        dashboard.operations = [
          OperationRecord(
            operationId: String(repeating: "f", count: 64), owner: "erin",
            requestId: UUID().uuidString.lowercased(), kind: "delete", vmId: 103,
            status: still ? status : .verified, step: still ? "destroy" : "complete",
            error: nil, createdAt: 1)
        ]
      }
      return Self.result(dashboard)
    case .hostInventory:
      calls.append((path, "host_inventory"))
      throw .timedOut
    default:
      calls.append((path, "other"))
      throw .launchFailed("Unexpected test RPC")
    }
  }

  static func dashboard(role: UserRole) -> DashboardResult {
    let user = User(
      userId: role == .admin ? "erin" : "frank", role: role, budgetMib: 4096,
      minMemoryBudgetMib: 512, maxMemoryBudgetMib: 2048, maxCores: 4, maxVms: 2, ready: true)
    return DashboardResult(
      identity: user, users: [user],
      budget: Budget(
        budgetMib: 4096, allocatedMib: 1024, remainingMib: 3072, minMemoryAllocatedMib: 128,
        memoryAllocatedMib: 256, coresAllocated: 1),
      vms: [], operations: [], environment: "lab-env", backend: .lab)
  }

  static func result<T: Encodable>(_ value: T) -> ProcessResult {
    let object = try! JSONSerialization.jsonObject(with: JSONEncoder().encode(value))
    return ProcessResult(
      terminationStatus: 0, terminatedBySignal: false,
      stdout: try! JSONSerialization.data(withJSONObject: ["ok": true, "result": object]),
      stderr: Data())
  }
}

@MainActor
private final class HomeFixture {
  let dir: URL
  let suite = "PveDevApp.server-home.\(UUID())"
  let a: ServerProfile, b: ServerProfile, host: ServerProfile
  let runner = HomeRunner()
  let model: AppModel

  init() throws {
    dir = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: false)
    let base = dir
    let paths = ["a", "b", "host"].map { base.appendingPathComponent("\($0).json") }
    for path in paths { try Data("{}".utf8).write(to: path) }
    a = ServerProfile(
      name: "Lab A", profilePath: paths[0].path,
      identity: ServerIdentity(
        serverId: String(repeating: "a", count: 64), environment: String(repeating: "b", count: 32),
        deviceId: String(repeating: "c", count: 64)))
    b = ServerProfile(
      name: "Lab B", profilePath: paths[1].path,
      identity: ServerIdentity(
        serverId: String(repeating: "d", count: 64), environment: String(repeating: "e", count: 32),
        deviceId: String(repeating: "c", count: 64)))
    // An untrusted read-only host: touching it at all would record a refresh failure.
    host = ServerProfile(name: "Observed", profilePath: paths[2].path, mode: .observation)
    var settings = AppSettings(executablePath: "/usr/bin/true")
    settings.inventoryExecutablePath = "/usr/bin/true"
    settings.servers = [a, b, host]
    settings.selectedServerId = a.id
    let store = SettingsStore(suiteName: suite)
    store.save(settings)
    model = AppModel(
      client: RpcClient(runner: runner), settingsStore: store,
      ledgerStore: LedgerStore(fileURL: dir.appendingPathComponent("ledger.json")),
      observationClient: ObservationClient(runner: runner))
  }

  func waitUntil(_ condition: () async -> Bool) async throws {
    for _ in 0..<400 {
      if await condition() { return }
      try await Task.sleep(for: .milliseconds(5))
    }
    Issue.record("Condition was not met in time")
  }

  /// Lets any follow-up request that would wrongly be sent arrive.
  func quiesce() async throws {
    try await waitUntil { !model.fleetIsRefreshing }
    try await Task.sleep(for: .milliseconds(50))
    try await waitUntil { !model.fleetIsRefreshing }
  }

  func hostUntouched() async -> Bool {
    let observed = await runner.count(action: "observe")
    let session = model.sessions[host.id]
    return observed == 0 && session?.refreshFailure == nil && session?.isRefreshing == false
      && session?.observation == nil
  }

  func clean() {
    model.setPolling(false)
    UserDefaults(suiteName: suite)?.removePersistentDomain(forName: suite)
    try? FileManager.default.removeItem(at: dir)
  }
}

@Suite("Server home and polling") @MainActor
struct ServerHomeAndPollingTests {
  @Test func memberHomeNeverRequestsHostInventory() async throws {
    let f = try HomeFixture()
    defer { f.clean() }
    await f.runner.setRole(.member, for: f.a.profilePath)
    await f.runner.setRole(.admin, for: f.b.profilePath)
    // What the home page does when it appears.
    f.model.refreshServers(onlyStale: true)
    try await f.waitUntil {
      f.model.dashboard(forServer: f.a.id) != nil && f.model.dashboard(forServer: f.b.id) != nil
    }
    try await f.quiesce()
    #expect(await f.runner.count(f.a.profilePath, "host_inventory") == 0)
    #expect(await f.runner.count(f.b.profilePath, "host_inventory") == 1)
    // Explicit per-server and page refreshes still skip the member's server.
    f.model.refreshServer(f.a.id)
    try await f.quiesce()
    f.model.refreshServers()
    try await f.quiesce()
    #expect(await f.runner.count(f.a.profilePath, "host_inventory") == 0)
    #expect(f.model.inventory(forServer: f.a.id) == nil)
    #expect(f.model.inventoryFailure(forServer: f.a.id) == nil)
    #expect(f.model.failure(forServer: f.a.id) == nil)
    #expect(!f.model.isAdmin(onServer: f.a.id))
    #expect(f.model.isAdmin(onServer: f.b.id))
    #expect(await f.hostUntouched())
  }

  @Test func homeAppearanceDoesNotRefreshLoadedServers() async throws {
    let f = try HomeFixture()
    defer { f.clean() }
    f.model.refreshServers(onlyStale: true)
    try await f.waitUntil {
      f.model.dashboard(forServer: f.a.id) != nil && f.model.dashboard(forServer: f.b.id) != nil
    }
    try await f.quiesce()
    let before = await f.runner.count(action: "dashboard")
    f.model.refreshServers(onlyStale: true)
    f.model.loadFleetIfNeeded()
    try await f.quiesce()
    let after = await f.runner.count(action: "dashboard")
    #expect(after == before)
  }

  @Test func pollingRefreshesOnlyServersWithPollingOn() async throws {
    let f = try HomeFixture()
    defer { f.clean() }
    #expect(f.model.fleetPageActive)
    f.model.selectServer(f.a.id)
    f.model.setPolling(true)
    #expect(f.model.shouldPoll(f.a.id))
    #expect(!f.model.shouldPoll(f.b.id))
    #expect(!f.model.shouldPoll(f.host.id))
    try await f.waitUntil { await f.runner.count(f.a.profilePath, "dashboard") >= 1 }
    // A background tick from the fleet page.
    f.model.refreshAll(onlyStale: true)
    try await f.quiesce()
    #expect(await f.runner.count(f.a.profilePath, "dashboard") >= 1)
    #expect(await f.runner.count(f.b.profilePath, "dashboard") == 0)
    #expect(await f.hostUntouched())
    // A manual fleet refresh reads every controller but never the read-only host.
    f.model.refreshAll()
    try await f.waitUntil { f.model.dashboard(forServer: f.b.id) != nil }
    try await f.quiesce()
    #expect(await f.hostUntouched())
    #expect(!f.model.fleetServers.contains { $0.id == f.host.id })
  }

  @Test func failedBackgroundReadShowsPausedUntilRetry() async throws {
    let f = try HomeFixture()
    defer { f.clean() }
    await f.runner.setFailing(true, for: f.a.profilePath)
    f.model.setPolling(true)
    try await f.waitUntil { f.model.failure(forServer: f.a.id) != nil }
    #expect(f.model.autoRefreshPaused(f.a.id))
    #expect(!f.model.shouldPoll(f.a.id))
    #expect(!f.model.autoRefreshPaused(f.b.id))
    let attempts = await f.runner.count(f.a.profilePath, "dashboard")
    f.model.refreshAll(onlyStale: true)
    try await f.quiesce()
    #expect(await f.runner.count(f.a.profilePath, "dashboard") == attempts)
    // Retry reads once and resumes auto-refresh on success.
    await f.runner.setFailing(false, for: f.a.profilePath)
    f.model.retryAutoRefresh(f.a.id)
    try await f.waitUntil { !f.model.autoRefreshPaused(f.a.id) }
    #expect(f.model.failure(forServer: f.a.id) == nil)
    #expect(f.model.shouldPoll(f.a.id))
  }

  @Test func runningOperationsAreFollowedUntilTheyFinishWithAutoRefreshOff() async throws {
    let f = try HomeFixture()
    defer { f.clean() }
    f.model.refresher.followInterval = .milliseconds(10)
    await f.runner.setPending(.running, reads: 3, for: f.a.profilePath)
    #expect(!f.model.shouldPoll(f.a.id))
    f.model.refresh(f.a.id)
    try await f.waitUntil {
      f.model.dashboard(forServer: f.a.id)?.operations.first?.status == .verified
    }
    try await f.waitUntil { !f.model.refresher.isFollowing(f.a.id) }
    try await f.quiesce()
    let reads = await f.runner.count(f.a.profilePath, "dashboard")
    // Three reads showed it running and one showed it finished; then reading stops.
    #expect(reads == 4)
    try await Task.sleep(for: .milliseconds(60))
    #expect(await f.runner.count(f.a.profilePath, "dashboard") == reads)
    #expect(await f.runner.count(f.b.profilePath, "dashboard") == 0)
    #expect(await f.hostUntouched())
  }

  @Test func blockedOperationsAndFailedReadsStopFollowing() async throws {
    let f = try HomeFixture()
    defer { f.clean() }
    f.model.refresher.followInterval = .milliseconds(10)
    // Blocked work waits for an administrator; it is never followed.
    await f.runner.setPending(.blocked, reads: 100, for: f.a.profilePath)
    f.model.refresh(f.a.id)
    try await f.waitUntil { f.model.dashboard(forServer: f.a.id) != nil }
    try await f.quiesce()
    #expect(!f.model.refresher.isFollowing(f.a.id))
    #expect(await f.runner.count(f.a.profilePath, "dashboard") == 1)
    // A failed read stops following until the next successful read.
    await f.runner.setPending(.queued, reads: 100, for: f.b.profilePath)
    f.model.refresh(f.b.id)
    try await f.waitUntil { f.model.refresher.isFollowing(f.b.id) }
    await f.runner.setFailing(true, for: f.b.profilePath)
    try await f.waitUntil { f.model.failure(forServer: f.b.id) != nil }
    try await f.waitUntil { !f.model.refresher.isFollowing(f.b.id) }
    let attempts = await f.runner.count(f.b.profilePath, "dashboard")
    try await Task.sleep(for: .milliseconds(60))
    #expect(await f.runner.count(f.b.profilePath, "dashboard") == attempts)
  }

  @Test func pausedIsOnlyReportedWhileAutoRefreshIsOn() async throws {
    let f = try HomeFixture()
    defer { f.clean() }
    await f.runner.setFailing(true, for: f.b.profilePath)
    f.model.refreshAll()
    try await f.waitUntil { f.model.failure(forServer: f.b.id) != nil }
    #expect(f.model.sessions[f.b.id]?.backgroundPaused == true)
    #expect(!f.model.autoRefreshPaused(f.b.id))
  }

  @Test func newVmMenuExplainsEachUnavailableServer() async throws {
    let f = try HomeFixture()
    defer { f.clean() }
    #expect(f.model.createVmIssue(forServer: f.a.id) == "Load this server's dashboard first.")
    await f.runner.setFailing(true, for: f.b.profilePath)
    f.model.refreshAll()
    try await f.waitUntil {
      f.model.dashboard(forServer: f.a.id) != nil && f.model.failure(forServer: f.b.id) != nil
    }
    #expect(f.model.createVmIssue(forServer: f.a.id) == nil)
    #expect(f.model.createVmIssue(forServer: f.b.id)?.contains("failed") == true)
    #expect(f.model.enrollVmIssue(forServer: f.a.id) != nil)
  }
}
