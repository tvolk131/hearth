import Foundation
import Observation
import PveDevCore

struct Notice: Identifiable, Hashable {
  enum Kind: Hashable { case success, failure }
  let id = UUID()
  let kind: Kind
  let title: String
  let detail: String
  let serverId: UUID?
  let identity: ServerIdentity?
}

@MainActor
@Observable
final class ServerSession {
  var isInventoryRefreshing = false
  var hostInventory: HostInventory?
  var inventoryFailure: RpcFailure?
  var dashboard: DashboardResult?
  var observation: ObservationResult?
  var imageCatalog: ImageCatalog?
  var catalogRevision = UUID()
  var lastRefreshedAt: Date?
  var isRefreshing = false
  var refreshFailure: RpcFailure?
  var pollingEnabled = false
  var backgroundPaused = false
  var inventoryRefreshedAt: Date?
  var inventoryBackgroundPaused = false
  let revision = UUID()
}

/// Captured by a form/confirmation when its page is built; never inferred at send time.
struct MutationOrigin: Hashable {
  let serverId: UUID
  let revision: UUID
}

@MainActor
@Observable
final class AppModel {
  private(set) var settings: AppSettings
  private(set) var settingsRevision = UUID()
  private(set) var sessions: [UUID: ServerSession] = [:]
  private(set) var ledger: MutationLedger
  private(set) var ledgerStorageIssue: String?
  var fleetPageActive = true
  private(set) var mutationSendStates: [UUID: RpcSendState] = [:]
  private var launchRefusals: [UUID: String] = [:]
  private var allNotices: [Notice] = []
  private let emptySession = ServerSession()

  @ObservationIgnored private var refreshTasks: [UUID: Task<Void, Never>] = [:]
  @ObservationIgnored private var pollingTasks: [UUID: Task<Void, Never>] = [:]
  @ObservationIgnored private var mutationTasks: [UUID: Task<Void, Never>] = [:]
  @ObservationIgnored private var awaitedMutationTasks:
    [UUID: Task<Result<MutationAcknowledgement, RpcFailure>, Never>] = [:]
  @ObservationIgnored private let client: RpcClient
  @ObservationIgnored private let observationClient: ObservationClient
  @ObservationIgnored private let settingsStore: SettingsStore
  @ObservationIgnored private let ledgerStore: LedgerStore
  @ObservationIgnored private let ledgerLoadFailed: Bool

  init(
    client: RpcClient = RpcClient(), settingsStore: SettingsStore = .applicationStore(),
    ledgerStore: LedgerStore = .defaultStore(), observationClient: ObservationClient? = nil
  ) {
    self.client = client
    self.observationClient = observationClient ?? ObservationClient(runner: client.runner)
    self.settingsStore = settingsStore
    self.ledgerStore = ledgerStore
    var loaded = settingsStore.load()
    loaded.pollingEnabled = false
    settings = loaded
    do {
      ledger = try ledgerStore.load()
      ledgerLoadFailed = false
    } catch {
      ledger = MutationLedger()
      ledgerLoadFailed = true
      ledgerStorageIssue =
        "Saved pending requests could not be read. They were left untouched on disk at \(ledgerStore.fileURL.path)."
    }
    for server in loaded.servers { sessions[server.id] = ServerSession() }
  }

  var selectedServerId: UUID? { settings.selectedServerId }
  var selectedServer: ServerProfile? { settings.selectedServer }
  var isReadOnly: Bool { selectedServer?.isReadOnly == true }
  var requestOrigin: MutationOrigin? {
    guard let id = selectedServerId, let session = sessions[id] else { return nil }
    return MutationOrigin(serverId: id, revision: session.revision)
  }
  func origin(forServer id: UUID) -> MutationOrigin? {
    guard settings.servers.contains(where: { $0.id == id }), let session = sessions[id] else {
      return nil
    }
    return MutationOrigin(serverId: id, revision: session.revision)
  }
  func dashboard(forServer id: UUID) -> DashboardResult? { sessions[id]?.dashboard }
  func inventory(forServer id: UUID) -> HostInventory? { sessions[id]?.hostInventory }
  func inventoryFailure(forServer id: UUID) -> RpcFailure? { sessions[id]?.inventoryFailure }
  func failure(forServer id: UUID) -> RpcFailure? { sessions[id]?.refreshFailure }
  var fleetIsRefreshing: Bool {
    sessions.values.contains { $0.isRefreshing || $0.isInventoryRefreshing }
  }
  var fleetVmCount: Int {
    sessions.values.reduce(0) {
      $0 + ($1.dashboard?.vms.filter { $0.state != .deleted }.count ?? 0)
    }
  }
  @ObservationIgnored private var inventoryTasks: [UUID: Task<Void, Never>] = [:]
  func serverName(on origin: MutationOrigin?) -> String {
    settings.servers.first { $0.id == origin?.serverId }?.name ?? "Unavailable server"
  }
  func refreshing(_ id: UUID) -> Bool {
    sessions[id]?.isRefreshing == true || sessions[id]?.isInventoryRefreshing == true
  }
  func freshness(_ id: UUID) -> String {
    if refreshing(id) { return "Refreshing…" }
    if failure(forServer: id) != nil {
      return sessions[id]?.dashboard == nil ? "Unavailable" : "Stale · refresh failed"
    }
    return sessions[id]?.lastRefreshedAt.map {
      "Updated " + $0.formatted(date: .omitted, time: .standard)
    } ?? "Not loaded"
  }
  func refreshForPage() {
    if fleetPageActive {
      refreshAll()
      refreshServers(onlyStale: true)
    } else {
      refresh()
    }
  }
  func refreshServer(_ id: UUID) {
    guard inventoryTasks[id] == nil, let origin = origin(forServer: id) else { return }
    inventoryTasks[id] = Task { [weak self] in
      await self?.loadHost(on: origin)
      self?.inventoryTasks[id] = nil
    }
  }
  func refreshServers(onlyStale: Bool = false) {
    for server in settings.servers where !server.isReadOnly {
      if onlyStale
        && (sessions[server.id]?.inventoryBackgroundPaused == true
          || (sessions[server.id]?.inventoryRefreshedAt.map { Date().timeIntervalSince($0) < 60 }
            ?? false))
      {
        continue
      }
      refreshServer(server.id)
    }
  }
  func refreshAll(onlyStale: Bool = false) {
    for server in settings.servers {
      if onlyStale
        && (sessions[server.id]?.backgroundPaused == true
          || (sessions[server.id]?.lastRefreshedAt.map {
            Date().timeIntervalSince($0) < Double(min(30, settings.clampedPollingInterval))
          } ?? false))
      {
        continue
      }
      refresh(server.id)
    }
  }
  func cancelAllRefreshes() {
    for task in refreshTasks.values { task.cancel() }
    for task in inventoryTasks.values { task.cancel() }
  }
  func loadHost(on origin: MutationOrigin) async {
    guard let session = sessions[origin.serverId], session.revision == origin.revision,
      !session.isInventoryRefreshing
    else { return }
    session.isInventoryRefreshing = true
    defer {
      if sessions[origin.serverId]?.revision == origin.revision {
        session.isInventoryRefreshing = false
      }
    }
    do throws(RpcFailure) {
      if let refresh = refreshTasks[origin.serverId] {
        await refresh.value
        if Task.isCancelled { throw RpcFailure.cancelled }
        if let failure = sessions[origin.serverId]?.refreshFailure { throw failure }
      }
      if sessions[origin.serverId]?.dashboard == nil
        || (sessions[origin.serverId]?.lastRefreshedAt.map { Date().timeIntervalSince($0) > 15 }
          ?? true)
      {
        _ = try await loadDashboard(on: origin)
      }
      let inventory = try await queryOnRoute(.hostInventory, origin: origin, as: HostInventory.self)
      guard self.origin(forServer: origin.serverId) == origin else { return }
      sessions[origin.serverId]?.hostInventory = inventory
      sessions[origin.serverId]?.inventoryFailure = nil
      sessions[origin.serverId]?.inventoryBackgroundPaused = false
      sessions[origin.serverId]?.inventoryRefreshedAt = Date()
    } catch {
      guard self.origin(forServer: origin.serverId) == origin else { return }
      if error != .cancelled {
        sessions[origin.serverId]?.inventoryFailure = error
        sessions[origin.serverId]?.inventoryBackgroundPaused = true
      }
    }
  }

  private var selectedSession: ServerSession {
    selectedServerId.flatMap { sessions[$0] } ?? emptySession
  }
  var dashboard: DashboardResult? { selectedSession.dashboard }
  var observation: ObservationResult? { selectedSession.observation }
  var lastRefreshedAt: Date? { selectedSession.lastRefreshedAt }
  var isRefreshing: Bool { selectedSession.isRefreshing }
  var refreshFailure: RpcFailure? { selectedSession.refreshFailure }
  var notices: [Notice] {
    allNotices.filter { notice in
      guard let id = notice.serverId else { return true }
      guard settings.servers.first(where: { $0.id == id })?.identity == notice.identity else {
        return false
      }
      return fleetPageActive || id == selectedServerId
    }
  }
  var permissions: Permissions { Permissions(identity: isReadOnly ? nil : dashboard?.identity) }
  var usesLegacyMachines: Bool { !isReadOnly && dashboard != nil && dashboard?.backend != .managed }
  var canCreateVm: Bool {
    !isReadOnly && dashboard?.canCreateManagedVm == true
      && (selectedServer?.expectedBackend == nil
        || selectedServer?.expectedBackend == dashboard?.backend)
  }

  var configurationIssues: [String] {
    var issues = settings.validationIssues()
    if selectedServer?.isTrusted != true {
      issues.append("Inspect and trust this server profile in Settings before connecting.")
    }
    return issues
  }
  var isConfigured: Bool { configurationIssues.isEmpty }
  var currentProfilePath: String { selectedServer?.canonicalProfilePath ?? "" }
  var pendingForCurrentProfile: [PendingMutation] { ledger.entries.filter(isCurrent) }
  var pendingForOtherProfiles: [PendingMutation] { ledger.entries.filter { !isCurrent($0) } }
  var unresolvedCount: Int { ledger.entries.filter { $0.phase != .rejected }.count }
  var activeOperationCount: Int { dashboard?.operations.filter { $0.status.isActive }.count ?? 0 }
  func identityUser() -> User? { dashboard?.identity }

  func selectServer(_ id: UUID) {
    guard settings.servers.contains(where: { $0.id == id }) else { return }
    settings.selectedServerId = id
    settings.pollingEnabled = sessions[id]?.pollingEnabled ?? false
    settingsRevision = UUID()
    settingsStore.save(settings)
  }

  @discardableResult
  func applySettings(_ newSettings: AppSettings, expectedRevision: UUID? = nil) -> Bool {
    guard expectedRevision == nil || expectedRevision == settingsRevision else { return false }
    var stored = newSettings
    stored.timeoutSeconds = newSettings.clampedTimeout
    stored.pollingIntervalSeconds = newSettings.clampedPollingInterval
    for index in stored.servers.indices {
      let server = stored.servers[index]
      if let old = settings.servers.first(where: { $0.id == server.id }),
        old.profilePath != server.profilePath || old.isReadOnly != server.isReadOnly
      {
        // Changing a path never silently transfers trust to the new file.
        stored.servers[index].identity = nil
        stored.servers[index].observationIdentity = nil
      }
    }
    if stored.selectedServer == nil { stored.selectedServerId = stored.servers.first?.id }
    for (id, session) in sessions {
      let before = settings.servers.first { $0.id == id }
      let after = stored.servers.first { $0.id == id }
      if before?.profilePath != after?.profilePath || before?.identity != after?.identity
        || before?.mode != after?.mode || before?.observationIdentity != after?.observationIdentity
        || before?.expectedBackend != after?.expectedBackend
        || settings.executablePath != stored.executablePath
        || settings.resolvedInventoryExecutablePath != stored.resolvedInventoryExecutablePath
        || after == nil
      {
        refreshTasks.removeValue(forKey: id)?.cancel()
        inventoryTasks.removeValue(forKey: id)?.cancel()
        pollingTasks.removeValue(forKey: id)?.cancel()
        sessions[id] = after == nil ? nil : ServerSession()
      } else if session.pollingEnabled
        && settings.pollingIntervalSeconds != stored.pollingIntervalSeconds
      {
        pollingTasks.removeValue(forKey: id)?.cancel()
      }
    }
    for server in stored.servers where sessions[server.id] == nil {
      sessions[server.id] = ServerSession()
    }
    stored.pollingEnabled =
      stored.selectedServerId.flatMap { sessions[$0]?.pollingEnabled } ?? false
    settings = stored
    settingsRevision = UUID()
    settingsStore.save(settings)
    for (id, session) in sessions where session.pollingEnabled { restartPolling(id) }
    return true
  }

  /// Local inspection only. Rust opens the profile and key; Swift receives public identifiers.
  func pinServer(_ id: UUID) async throws(RpcFailure) {
    if settings.servers.first(where: { $0.id == id })?.isReadOnly == true {
      try await pinObservationServer(id)
      return
    }
    let configuration = try self.configuration(for: id, requireIdentity: false)
    let original = settings.servers.first { $0.id == id }
    let originalExecutable = settings.executablePath
    let identity = try await client.profileIdentity(configuration: configuration)
    guard let index = settings.servers.firstIndex(where: { $0.id == id }),
      settings.servers[index].profilePath == original?.profilePath,
      settings.servers[index].identity == original?.identity,
      settings.executablePath == originalExecutable
    else {
      throw .configuration(
        "The server settings changed during inspection. Inspect the profile again.")
    }
    var updated = settings
    updated.servers[index].identity = identity
    guard updated.serverListIssues.isEmpty else {
      throw .configuration(updated.serverListIssues.joined(separator: "\n"))
    }
    applySettings(updated)
  }

  private func pinObservationServer(_ id: UUID) async throws(RpcFailure) {
    var snapshot = settings
    snapshot.selectedServerId = id
    let configuration = try snapshot.observationConfiguration(requirePinnedIdentity: false)
    let original = snapshot.selectedServer
    let originalExecutable = snapshot.resolvedInventoryExecutablePath
    let identity = try await observationClient.profileIdentity(configuration: configuration)
    guard let index = settings.servers.firstIndex(where: { $0.id == id }),
      settings.servers[index] == original,
      settings.resolvedInventoryExecutablePath == originalExecutable
    else {
      throw .configuration(
        "The read-only host settings changed during inspection. Inspect the profile again.")
    }
    var updated = settings
    updated.servers[index].observationIdentity = identity
    guard updated.serverListIssues.isEmpty else {
      throw .configuration(updated.serverListIssues.joined(separator: "\n"))
    }
    applySettings(updated)
  }

  func refresh() {
    guard let id = selectedServerId else { return }
    refresh(id)
  }

  private func refresh(_ id: UUID) {
    guard refreshTasks[id] == nil, let session = sessions[id] else {
      return
    }
    if settings.servers.first(where: { $0.id == id })?.isReadOnly == true {
      refreshObservation(id, session: session)
      return
    }
    let configuration: RpcConfiguration
    do { configuration = try self.configuration(for: id) } catch {
      session.refreshFailure = error
      return
    }
    session.isRefreshing = true
    let client = self.client
    refreshTasks[id] = Task { [weak self] in
      let result: Result<DashboardResult, RpcFailure>
      do throws(RpcFailure) {
        result = .success(try await client.dashboard(configuration: configuration))
      } catch { result = .failure(error) }
      guard let self, self.sessions[id]?.revision == session.revision else { return }
      self.refreshTasks[id] = nil
      session.isRefreshing = false
      switch result {
      case .success(let dashboard):
        session.dashboard = dashboard
        session.lastRefreshedAt = Date()
        session.refreshFailure = nil
        session.backgroundPaused = false
      case .failure(let error):
        if error != .cancelled {
          session.refreshFailure = error
          session.backgroundPaused = true
        }
      }
    }
  }

  private func refreshObservation(_ id: UUID, session: ServerSession) {
    var snapshot = settings
    snapshot.selectedServerId = id
    let configuration: ObservationConfiguration
    do { configuration = try snapshot.observationConfiguration() } catch {
      session.refreshFailure = error
      return
    }
    session.isRefreshing = true
    let client = observationClient
    refreshTasks[id] = Task { [weak self] in
      let result: Result<ObservationResult, RpcFailure>
      do throws(RpcFailure) {
        result = .success(try await client.observe(configuration: configuration))
      } catch { result = .failure(error) }
      guard let self, self.sessions[id]?.revision == session.revision else { return }
      self.refreshTasks[id] = nil
      session.isRefreshing = false
      switch result {
      case .success(let observation):
        session.observation = observation
        session.lastRefreshedAt = Date()
        session.refreshFailure = nil
        session.backgroundPaused = false
      case .failure(let error):
        if error != .cancelled {
          session.refreshFailure = error
          session.backgroundPaused = true
        }
      }
    }
  }

  func cancelRefresh() {
    guard let id = selectedServerId else { return }
    refreshTasks[id]?.cancel()
  }

  func setPolling(_ enabled: Bool) {
    guard let id = selectedServerId, let session = sessions[id] else { return }
    session.pollingEnabled = enabled
    settings.pollingEnabled = enabled
    if enabled { restartPolling(id) } else { pollingTasks.removeValue(forKey: id)?.cancel() }
  }

  private func restartPolling(_ id: UUID) {
    pollingTasks.removeValue(forKey: id)?.cancel()
    let interval = settings.clampedPollingInterval
    pollingTasks[id] = Task { [weak self] in
      while !Task.isCancelled {
        guard let self, self.sessions[id]?.pollingEnabled == true else { return }
        if self.fleetPageActive {
          self.refreshAll(onlyStale: true)
        } else if self.sessions[id]?.backgroundPaused != true {
          self.refresh(id)
        }
        do { try await Task.sleep(for: .seconds(interval)) } catch { return }
      }
    }
  }

  private func postNotSent(_ detail: String, origin: MutationOrigin?) {
    let server = settings.servers.first { $0.id == origin?.serverId }
    let unchanged =
      origin != nil && origin == origin.flatMap { self.origin(forServer: $0.serverId) }
    post(
      .failure, "\(server?.name ?? "Original server"): Not sent", detail,
      serverId: server?.id, identity: server?.identity, global: !unchanged)
  }

  @discardableResult
  func submit(_ request: RpcRequest, origin: MutationOrigin?) -> UUID? {
    guard request.isMutation else { return nil }
    guard let origin else {
      postNotSent("The original server route is unavailable.", origin: origin)
      return nil
    }
    let configuration: RpcConfiguration
    do { configuration = try checkedRoute(request, origin: origin) } catch {
      postNotSent(error.detail, origin: origin)
      return nil
    }
    guard !ledgerLoadFailed else {
      postNotSent("The pending-request ledger needs investigation.", origin: origin)
      return nil
    }
    guard let server = settings.servers.first(where: { $0.id == origin.serverId }),
      let identity = server.identity
    else { return nil }
    let entry = ledger.prepare(
      request, profilePath: configuration.profilePath, serverId: server.id, identity: identity)
    guard persistLedger() else { return nil }
    if entry.request != request {
      postNotSent(
        "A matching request already exists. Review it in Pending Requests and explicitly retry with the same request ID.",
        origin: origin)
      return nil
    }
    send(entry.id, origin: origin)
    return entry.id
  }

  /// Plans and inspections belong to the exact route captured when their form
  /// opened. A late reply must never authorize a mutation on another server.
  func query<Value: Decodable & Sendable>(
    _ request: RpcRequest, origin: MutationOrigin?, as type: Value.Type
  ) async throws(RpcFailure) -> Value {
    guard let origin, origin == requestOrigin else {
      throw .configuration(
        "The server or its profile changed. Open this action again on its original server.")
    }
    let value = try await queryOnRoute(request, origin: origin, as: type)
    guard origin == requestOrigin else {
      throw .configuration(
        "The server or its profile changed while this request was running. Its result was discarded."
      )
    }
    return value
  }

  /// Used by a reviewed two-server workflow. Both routes carry independently
  /// pinned identities; choosing a destination never changes the selected host.
  func queryOnRoute<Value: Decodable & Sendable>(
    _ request: RpcRequest, origin: MutationOrigin, as type: Value.Type
  ) async throws(RpcFailure) -> Value {
    guard !request.isMutation else {
      throw .configuration(
        "This request changes server state and must use the pending-request ledger.")
    }
    let configuration = try checkedRoute(request, origin: origin)
    let value = try await client.query(request, configuration: configuration, as: type)
    guard origin == self.origin(forServer: origin.serverId) else {
      throw .configuration(
        "The server profile changed while this request was running. Its result was discarded.")
    }
    return value
  }

  func loadDashboard(on origin: MutationOrigin) async throws(RpcFailure) -> DashboardResult {
    do throws(RpcFailure) {
      let value = try await queryOnRoute(.dashboard, origin: origin, as: DashboardResult.self)
      sessions[origin.serverId]?.dashboard = value
      sessions[origin.serverId]?.lastRefreshedAt = Date()
      sessions[origin.serverId]?.refreshFailure = nil
      sessions[origin.serverId]?.backgroundPaused = false
      return value
    } catch {
      if origin == self.origin(forServer: origin.serverId), error != .cancelled {
        sessions[origin.serverId]?.refreshFailure = error
        sessions[origin.serverId]?.backgroundPaused = true
      }
      throw error
    }
  }

  func loadImageCatalog(origin: MutationOrigin?) async throws(RpcFailure) -> ImageCatalog {
    guard let origin, origin == self.origin(forServer: origin.serverId),
      dashboard(forServer: origin.serverId)?.backend == .managed
    else {
      throw .configuration("Choose the current managed controller before loading approved images.")
    }
    // Never retain previously accepted image metadata after a failed reload.
    let revision = UUID()
    sessions[origin.serverId]?.catalogRevision = revision
    sessions[origin.serverId]?.imageCatalog = nil
    let value = try await queryOnRoute(.listImageCatalog, origin: origin, as: ImageCatalog.self)
    guard origin == self.origin(forServer: origin.serverId),
      sessions[origin.serverId]?.catalogRevision == revision,
      dashboard(forServer: origin.serverId)?.backend == .managed,
      dashboard(forServer: origin.serverId)?.supports("list_image_catalog") == true
    else {
      throw .configuration("The catalog's controller route changed. Reload approved images.")
    }
    sessions[origin.serverId]?.imageCatalog = value
    return value
  }

  /// Returns typed migration grants/receipts only after the same durable
  /// request bookkeeping used by ordinary forms. Unknown replies remain
  /// retryable in Pending Requests with their original route and payload.
  func performMutation(_ request: RpcRequest, origin: MutationOrigin) async throws(RpcFailure)
    -> MutationAcknowledgement
  {
    guard request.isMutation else { throw .configuration("This action is read-only.") }
    guard !ledgerLoadFailed else {
      throw .configuration(
        "The saved pending-request ledger needs investigation before sending changes.")
    }
    let configuration = try checkedRoute(request, origin: origin)
    guard let server = settings.servers.first(where: { $0.id == origin.serverId }),
      let identity = server.identity
    else {
      throw .configuration("The server identity is unavailable.")
    }
    let entry = ledger.prepare(
      request, profilePath: configuration.profilePath, serverId: server.id, identity: identity)
    guard entry.request == request else {
      throw .configuration(
        "A matching request is already pending. Use Retry with Same Request ID in Pending Requests on \(server.name)."
      )
    }
    let retry: RpcRequest
    do {
      retry = try ledger.beginAttempt(
        entry.id, profilePath: configuration.profilePath, serverId: server.id, identity: identity)
    } catch { throw .configuration(Self.describe(error)) }
    guard persistLedger() else {
      ledger.recordFailure(
        entry.id,
        failure: .configuration("The pending request could not be saved. Nothing was sent."))
      throw .configuration("The pending request could not be saved. Nothing was sent.")
    }
    let reviewedVm = reviewedMachine(retry, on: origin.serverId)
    mutationSendStates[entry.id] = .queued
    let client = self.client
    let task = Task { () -> Result<MutationAcknowledgement, RpcFailure> in
      do throws(RpcFailure) {
        return .success(
          try await client.submit(
            retry, configuration: configuration,
            onState: { [weak self] state in await self?.setSendState(state, id: entry.id) },
            shouldLaunch: { [weak self] in
              await self?.authorizeLaunch(
                entry.id, request: retry, origin: origin, configuration: configuration,
                reviewedVm: reviewedVm) ?? false
            }))
      } catch { return .failure(error) }
    }
    awaitedMutationTasks[entry.id] = task
    let outcome = await withTaskCancellationHandler {
      await task.value
    } onCancel: {
      task.cancel()
    }
    awaitedMutationTasks[entry.id] = nil
    complete(entry.id, request: retry, server: server, revision: origin.revision, outcome: outcome)
    switch outcome {
    case .success(let acknowledgement):
      guard origin == self.origin(forServer: server.id) else {
        throw .configuration(
          "The original server acknowledged this action, but its profile changed. Refresh that server before continuing."
        )
      }
      return acknowledgement
    case .failure(let failure): throw failure
    }
  }

  private func checkedRoute(_ request: RpcRequest, origin: MutationOrigin) throws(RpcFailure)
    -> RpcConfiguration
  {
    guard origin == self.origin(forServer: origin.serverId),
      let server = settings.servers.first(where: { $0.id == origin.serverId }), !server.isReadOnly
    else {
      throw .configuration("This server route is read-only or changed after review.")
    }
    let dashboard = sessions[server.id]?.dashboard
    if request.requiresAdministrator && !Permissions(identity: dashboard?.identity).isAdmin {
      throw .configuration("Load an administrator dashboard for this server before continuing.")
    }
    if let issue = routeIssue(request, serverId: server.id) { throw .configuration(issue) }
    if case .vmAction(let action) = request, action.kind == .start,
      let vm = dashboard?.vms.first(where: { $0.vmId == action.vmId }),
      let issue = startIssue(vm: vm, on: server.id)
    {
      throw .configuration(issue)
    }
    return try configuration(for: server.id)
  }

  private func routeIssue(_ request: RpcRequest, serverId: UUID?) -> String? {
    guard let serverId, let session = sessions[serverId] else {
      return "The server route is unavailable."
    }
    let expectedBackend = settings.servers.first(where: { $0.id == serverId })?.expectedBackend
    if let dashboard = session.dashboard {
      if let expectedBackend, dashboard.backend != expectedBackend {
        return "The controller backend differs from this profile's expected backend."
      }
      return dashboard.requestIssue(request, catalog: session.imageCatalog)
    }
    if request != .dashboard && expectedBackend != nil {
      return "Load this controller's dashboard before continuing."
    }
    // Preserve legacy lab/test flows; capability-dependent requests still require a dashboard.
    if request == .listImageCatalog || request.requiredCapability != nil {
      return "Load this server's dashboard before continuing."
    }
    return nil
  }

  private func supports(_ request: RpcRequest) -> Bool {
    routeIssue(request, serverId: selectedServerId) == nil
  }

  func canRetry(_ entry: PendingMutation) -> Bool {
    guard supports(entry.request) else { return false }
    if entry.request.requiresAdministrator && !permissions.canManageUsers { return false }
    return entry.canRetry && isCurrent(entry) && isConfigured
  }
  func retry(_ id: UUID) { send(id) }
  func cancelMutation(_ id: UUID) {
    mutationTasks[id]?.cancel()
    awaitedMutationTasks[id]?.cancel()
  }
  func discard(_ id: UUID) { if ledger.discard(id) { persistLedger() } }
  func isMutationInFlight(_ id: UUID) -> Bool {
    mutationTasks[id] != nil || awaitedMutationTasks[id] != nil
  }
  func isVmBusy(_ vmId: UInt16, on serverId: UUID) -> Bool {
    guard let server = settings.servers.first(where: { $0.id == serverId }),
      let identity = server.identity
    else { return false }
    return sessions[serverId]?.dashboard?.operations.contains(where: {
      $0.vmId == vmId && $0.status.isActive
    }) == true
      || ledger.isBusy(vmId: vmId, serverId: serverId, identity: identity)
  }
  func isVmBusy(_ vmId: UInt16) -> Bool {
    guard let server = selectedServer, let identity = server.identity else { return false }
    if dashboard?.operations.contains(where: { $0.vmId == vmId && $0.status.isActive }) == true {
      return true
    }
    return ledger.isBusy(vmId: vmId, serverId: server.id, identity: identity)
  }
  func inFlightEntry(vmId: UInt16, on serverId: UUID) -> PendingMutation? {
    ledger.entries.first { entry in
      guard entry.serverId == serverId, entry.phase == .inFlight else { return false }
      if case .vmAction(let action) = entry.request { return action.vmId == vmId }
      return false
    }
  }
  func sendState(vmId: UInt16, on serverId: UUID) -> RpcSendState? {
    inFlightEntry(vmId: vmId, on: serverId).flatMap { mutationSendStates[$0.id] }
  }
  func serverName(for entry: PendingMutation) -> String {
    settings.servers.first { $0.id == entry.serverId }?.name
      ?? (entry.hasServerBinding ? "Removed server" : "Legacy request (identity unknown)")
  }
  func dismissNotice(_ notice: Notice) { allNotices.removeAll { $0.id == notice.id } }

  private func isCurrent(_ entry: PendingMutation) -> Bool {
    !isReadOnly && entry.hasServerBinding && entry.serverId == selectedServerId
      && entry.identity == selectedServer?.identity
      && entry.profilePath == currentProfilePath
  }

  private func configuration(for id: UUID, requireIdentity: Bool = true) throws(RpcFailure)
    -> RpcConfiguration
  {
    guard settings.servers.contains(where: { $0.id == id }) else {
      throw .configuration("This server is no longer configured.")
    }
    var snapshot = settings
    snapshot.selectedServerId = id
    return try snapshot.configuration(requirePinnedIdentity: requireIdentity)
  }

  private func send(_ id: UUID, origin: MutationOrigin? = nil) {
    guard !ledgerLoadFailed, let entry = ledger.entry(id) else { return }
    // Manual retries still require selecting the original route; new forms
    // pass their reviewed origin and need not alter the sidebar selection.
    guard let origin = origin ?? (isCurrent(entry) ? requestOrigin : nil),
      let server = settings.servers.first(where: { $0.id == origin.serverId }),
      let identity = server.identity,
      let session = sessions[server.id]
    else { return }
    let configuration: RpcConfiguration
    do { configuration = try checkedRoute(entry.request, origin: origin) } catch {
      postNotSent(error.detail, origin: origin)
      return
    }
    let request: RpcRequest
    do {
      request = try ledger.beginAttempt(
        id, profilePath: configuration.profilePath, serverId: server.id, identity: identity)
    } catch {
      postNotSent(Self.describe(error), origin: origin)
      return
    }
    guard persistLedger() else {
      ledger.recordFailure(
        id,
        failure: .configuration("The pending request could not be durably saved. Nothing was sent.")
      )
      return
    }
    let client = self.client
    let reviewedVm = reviewedMachine(request, on: origin.serverId)
    mutationSendStates[id] = .queued
    mutationTasks[id] = Task { [weak self] in
      let outcome: Result<MutationAcknowledgement, RpcFailure>
      do throws(RpcFailure) {
        outcome = .success(
          try await client.submit(
            request, configuration: configuration,
            onState: { [weak self] state in await self?.setSendState(state, id: id) },
            shouldLaunch: { [weak self] in
              await self?.authorizeLaunch(
                id, request: request, origin: origin, configuration: configuration,
                reviewedVm: reviewedVm) ?? false
            }))
      } catch { outcome = .failure(error) }
      self?.complete(
        id, request: request, server: server, revision: session.revision, outcome: outcome)
    }
  }

  private func reviewedMachine(_ request: RpcRequest, on id: UUID) -> Vm? {
    if case .vmAction(let action) = request {
      return dashboard(forServer: id)?.vms.first { $0.vmId == action.vmId }
    }
    return nil
  }

  /// MainActor gate + sending transition are atomic relative to Settings and
  /// dashboard updates. Later cancellation is conservatively "may have sent".
  private func authorizeLaunch(
    _ id: UUID, request: RpcRequest, origin: MutationOrigin,
    configuration: RpcConfiguration, reviewedVm: Vm?
  ) -> Bool {
    do throws(RpcFailure) {
      guard try checkedRoute(request, origin: origin) == configuration else {
        throw .configuration("The server configuration changed while this request was queued.")
      }
      guard let entry = ledger.entry(id), entry.isInFlight else {
        throw .configuration("The pending request changed.")
      }
      if entry.attempts == 1, let reviewedVm, case .vmAction(let action) = request {
        guard let current = reviewedMachine(request, on: origin.serverId), current == reviewedVm,
          dashboard(forServer: origin.serverId)?.availableActions(for: current).contains(
            action.kind) == true,
          dashboard(forServer: origin.serverId)?.operations.contains(where: {
            $0.vmId == action.vmId && !$0.status.isTerminal
          }) != true
        else {
          throw .configuration(
            "The VM changed while this request was queued. Refresh and review it again.")
        }
      }
      mutationSendStates[id] = .sending
      return true
    } catch {
      launchRefusals[id] = error.detail
      return false
    }
  }

  private func setSendState(_ state: RpcSendState, id: UUID) { mutationSendStates[id] = state }

  private func complete(
    _ id: UUID, request: RpcRequest, server: ServerProfile, revision: UUID,
    outcome: Result<MutationAcknowledgement, RpcFailure>
  ) {
    mutationTasks[id] = nil
    let sendState = mutationSendStates.removeValue(forKey: id)
    let refusal = launchRefusals.removeValue(forKey: id)
    let changedRoute = sessions[server.id]?.revision != revision
    switch outcome {
    case .success(let acknowledgement):
      ledger.recordAcknowledged(id)
      post(
        .success, "\(server.name): \(request.summary)", acknowledgement.summary,
        serverId: server.id, identity: server.identity, global: changedRoute)
    case .failure(let failure):
      if failure == .cancelled && sendState == .queued {
        ledger.recordNotLaunched(id)
      } else {
        ledger.recordFailure(id, failure: failure)
      }
      if failure == .cancelled && sendState == .queued {
        post(
          .failure, "\(server.name): Not sent",
          refusal ?? "Cancelled before this attempt launched. The original request ID is retained.",
          global: true)
      } else {
        post(
          .failure, "\(server.name): \(request.summary): \(failure.title)",
          failure.detail
            + (failure.isDefinitiveRejection
              ? ""
              : "\nThe request ID was kept. Retry from Pending Requests to avoid a duplicate."),
          serverId: server.id, identity: server.identity, global: changedRoute)
      }
    }
    persistLedger()
    // Finishing an A action must never refresh selected server B or a replacement A profile.
    if sessions[server.id]?.revision == revision && sendState != .queued { refresh(server.id) }
  }

  @discardableResult
  private func persistLedger() -> Bool {
    guard !ledgerLoadFailed else { return false }
    do {
      try ledgerStore.save(ledger)
      ledgerStorageIssue = nil
      return true
    } catch {
      ledgerStorageIssue =
        "Pending requests could not be saved. New sends are blocked until the ledger can be written. Already accepted server work is not cancelled."
      return false
    }
  }

  private func post(
    _ kind: Notice.Kind, _ title: String, _ detail: String,
    serverId: UUID? = nil, identity: ServerIdentity? = nil, global: Bool = false
  ) {
    allNotices.append(
      Notice(
        kind: kind, title: title, detail: detail,
        serverId: global ? nil : (serverId ?? selectedServerId),
        identity: global ? nil : (identity ?? selectedServer?.identity)))
    if kind == .success, let notice = allNotices.last {
      Task { [weak self] in
        try? await Task.sleep(for: .seconds(8))
        self?.dismissNotice(notice)
      }
    }
    if allNotices.count > 32 { allNotices.removeFirst(allNotices.count - 32) }
  }

  private static func describe(_ error: LedgerError) -> String {
    switch error {
    case .unknownMutation: "The request is no longer pending."
    case .alreadyInFlight: "This request is already in progress."
    case .rejectedByServer:
      "The server rejected this request. Discard it and submit a corrected one."
    case .profileMismatch:
      "This request belongs to a different profile path. Restore the original profile to retry."
    case .serverMismatch:
      "This request belongs to a different server or device identity. Switch to its original server to retry."
    case .unboundIdentity:
      "This older request has no saved server identity and cannot be safely retried. Inspect it before discarding."
    }
  }
}
