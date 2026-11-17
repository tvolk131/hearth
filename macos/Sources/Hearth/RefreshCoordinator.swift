import Foundation
import HearthCore

/// Inventory and polling: dashboard and observation refreshes, administrator
/// hardware inventory, approved image catalogs and per-server auto-refresh.
/// Read-only work only; nothing here sends or retries a change.
@MainActor
final class RefreshCoordinator: ServerSessionLifecycle {
  private let registry: ServerRegistry
  private let routes: ServerRoutes
  private let client: RpcClient
  private let observationClient: ObservationClient
  private var refreshTasks: [UUID: Task<Void, Never>] = [:]
  private var pollingTasks: [UUID: Task<Void, Never>] = [:]
  private var inventoryTasks: [UUID: Task<Void, Never>] = [:]

  init(
    registry: ServerRegistry, routes: ServerRoutes, client: RpcClient,
    observationClient: ObservationClient
  ) {
    self.registry = registry
    self.routes = routes
    self.client = client
    self.observationClient = observationClient
  }

  private var sessions: [UUID: ServerSession] { registry.sessions }
  private var settings: AppSettings { registry.settings }

  // MARK: Freshness

  var fleetIsRefreshing: Bool {
    sessions.values.contains { $0.isRefreshing || $0.isInventoryRefreshing }
  }
  func refreshing(_ id: UUID) -> Bool {
    sessions[id]?.isRefreshing == true || sessions[id]?.isInventoryRefreshing == true
  }
  func freshness(_ id: UUID) -> String {
    if refreshing(id) { return "Refreshing…" }
    if sessions[id]?.refreshFailure != nil {
      return sessions[id]?.dashboard == nil ? "Unavailable" : "Stale · refresh failed"
    }
    return sessions[id]?.lastRefreshedAt.map {
      "Updated " + $0.formatted(date: .omitted, time: .standard)
    } ?? "Not loaded"
  }

  // MARK: Fleet refresh

  func refreshServer(_ id: UUID) {
    guard inventoryTasks[id] == nil, let origin = registry.origin(forServer: id) else { return }
    inventoryTasks[id] = Task { [weak self] in
      await self?.loadHost(on: origin)
      // A settings change may have cancelled and replaced this task; keep the new handle.
      guard let self, self.sessions[id]?.revision == origin.revision else { return }
      self.inventoryTasks[id] = nil
    }
  }
  /// Whether the signed-in account is an administrator on this server's last dashboard.
  func isAdmin(onServer id: UUID) -> Bool {
    Permissions(identity: sessions[id]?.dashboard?.identity).isAdmin
  }
  /// Never loaded, never failed: the first read for a page, not background polling.
  private func needsInitialLoad(_ id: UUID) -> Bool {
    guard let session = sessions[id] else { return false }
    return session.dashboard == nil && session.refreshFailure == nil && !session.backgroundPaused
  }
  /// Manual refresh refreshes every fleet server. `onlyStale` is used when a page
  /// appears: it only performs first loads (hardware inventory only for admins)
  /// and otherwise leaves servers to their own auto-refresh setting.
  func refreshServers(onlyStale: Bool = false) {
    for server in registry.fleetServers {
      if onlyStale {
        let session = sessions[server.id]
        let firstInventory =
          isAdmin(onServer: server.id) && session?.hostInventory == nil
          && session?.inventoryBackgroundPaused != true
        guard needsInitialLoad(server.id) || firstInventory else { continue }
      }
      refreshServer(server.id)
    }
  }
  /// Manual refresh of every fleet server. `onlyStale` is the background path:
  /// only servers with auto-refresh on and not paused by a failure.
  func refreshAll(onlyStale: Bool = false) {
    for server in registry.fleetServers {
      if onlyStale && !shouldPoll(server.id) { continue }
      refresh(server.id)
    }
  }
  /// First loads for fleet servers that have never been read. Servers with data
  /// are refreshed manually or by their own auto-refresh, never by navigation.
  func loadFleetIfNeeded() {
    for server in registry.fleetServers where needsInitialLoad(server.id) { refresh(server.id) }
  }
  /// Auto-refresh reads only servers with polling on that a failure has not paused.
  func shouldPoll(_ id: UUID) -> Bool {
    guard let session = sessions[id] else { return false }
    return session.pollingEnabled && !session.backgroundPaused
  }
  /// A failed background read stopped this server's auto-refresh until an explicit read.
  func autoRefreshPaused(_ id: UUID) -> Bool {
    sessions[id]?.pollingEnabled == true && sessions[id]?.backgroundPaused == true
  }
  func retryAutoRefresh(_ id: UUID) { refresh(id) }
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
      // Hardware inventory is administrator-only; members never request it.
      guard isAdmin(onServer: origin.serverId) else {
        sessions[origin.serverId]?.hostInventory = nil
        sessions[origin.serverId]?.inventoryFailure = nil
        sessions[origin.serverId]?.inventoryBackgroundPaused = false
        return
      }
      let inventory = try await routes.queryOnRoute(
        .hostInventory, origin: origin, as: HostInventory.self)
      guard registry.origin(forServer: origin.serverId) == origin else { return }
      sessions[origin.serverId]?.hostInventory = inventory
      sessions[origin.serverId]?.inventoryFailure = nil
      sessions[origin.serverId]?.inventoryBackgroundPaused = false
      sessions[origin.serverId]?.inventoryRefreshedAt = Date()
    } catch {
      guard registry.origin(forServer: origin.serverId) == origin else { return }
      if error != .cancelled {
        sessions[origin.serverId]?.inventoryFailure = error
        sessions[origin.serverId]?.inventoryBackgroundPaused = true
      }
    }
  }

  // MARK: Single-server refresh

  func refresh() {
    guard let id = registry.selectedServerId else { return }
    refresh(id)
  }

  func refresh(_ id: UUID) {
    guard refreshTasks[id] == nil, let session = sessions[id] else {
      return
    }
    if registry.server(id)?.isReadOnly == true {
      refreshObservation(id, session: session)
      return
    }
    let configuration: RpcConfiguration
    do { configuration = try registry.configuration(for: id) } catch {
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
    guard let id = registry.selectedServerId else { return }
    refreshTasks[id]?.cancel()
  }

  func loadDashboard(on origin: MutationOrigin) async throws(RpcFailure) -> DashboardResult {
    do throws(RpcFailure) {
      let value = try await routes.queryOnRoute(
        .dashboard, origin: origin, as: DashboardResult.self)
      sessions[origin.serverId]?.dashboard = value
      sessions[origin.serverId]?.lastRefreshedAt = Date()
      sessions[origin.serverId]?.refreshFailure = nil
      sessions[origin.serverId]?.backgroundPaused = false
      return value
    } catch {
      if origin == registry.origin(forServer: origin.serverId), error != .cancelled {
        sessions[origin.serverId]?.refreshFailure = error
        sessions[origin.serverId]?.backgroundPaused = true
      }
      throw error
    }
  }

  func loadImageCatalog(origin: MutationOrigin?) async throws(RpcFailure) -> ImageCatalog {
    guard let origin, origin == registry.origin(forServer: origin.serverId),
      sessions[origin.serverId]?.dashboard?.backend == .managed
    else {
      throw .configuration("Choose the current managed controller before loading approved images.")
    }
    // Never retain previously accepted image metadata after a failed reload.
    let revision = UUID()
    sessions[origin.serverId]?.catalogRevision = revision
    sessions[origin.serverId]?.imageCatalog = nil
    let value = try await routes.queryOnRoute(
      .listImageCatalog, origin: origin, as: ImageCatalog.self)
    guard origin == registry.origin(forServer: origin.serverId),
      sessions[origin.serverId]?.catalogRevision == revision,
      sessions[origin.serverId]?.dashboard?.backend == .managed,
      sessions[origin.serverId]?.dashboard?.supports("list_image_catalog") == true
    else {
      throw .configuration("The catalog's controller route changed. Reload approved images.")
    }
    sessions[origin.serverId]?.imageCatalog = value
    return value
  }

  // MARK: Polling

  func setPolling(_ enabled: Bool) {
    guard let id = registry.selectedServerId else { return }
    setPolling(enabled, forServer: id)
  }

  /// Auto-refresh is chosen per server; it never depends on which page is showing.
  func setPolling(_ enabled: Bool, forServer id: UUID) {
    guard let session = sessions[id] else { return }
    session.pollingEnabled = enabled
    if id == registry.selectedServerId { registry.setSelectedPollingFlag(enabled) }
    if enabled { restartPolling(id) } else { pollingTasks.removeValue(forKey: id)?.cancel() }
  }

  private func restartPolling(_ id: UUID) {
    pollingTasks.removeValue(forKey: id)?.cancel()
    let interval = settings.clampedPollingInterval
    pollingTasks[id] = Task { [weak self] in
      while !Task.isCancelled {
        guard let self, self.sessions[id]?.pollingEnabled == true else { return }
        // Each server polls only itself; servers with auto-refresh off are never touched.
        if self.shouldPoll(id) { self.refresh(id) }
        do { try await Task.sleep(for: .seconds(interval)) } catch { return }
      }
    }
  }

  // MARK: ServerSessionLifecycle

  func sessionWillReset(_ id: UUID) {
    refreshTasks.removeValue(forKey: id)?.cancel()
    inventoryTasks.removeValue(forKey: id)?.cancel()
    pollingTasks.removeValue(forKey: id)?.cancel()
  }
  func pollingIntervalWillChange(_ id: UUID) {
    pollingTasks.removeValue(forKey: id)?.cancel()
  }
  func settingsDidApply() {
    for (id, session) in sessions where session.pollingEnabled { restartPolling(id) }
  }
}
