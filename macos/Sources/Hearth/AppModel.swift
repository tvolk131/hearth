import Foundation
import Observation
import HearthCore

/// The app's single environment object. It composes focused parts and keeps
/// the API pages use; each part owns its own state:
/// - `ServerRegistry`: settings, per-server sessions, selection and trust
/// - `ServerRoutes`: route checks and read-only queries (forms, plans, migration)
/// - `RefreshCoordinator`: dashboards, hardware inventory, catalogs and polling
/// - `MutationCoordinator`: the pending-request ledger and the one send path
/// - `NoticeCenter`: transient notices bound to server identities
@MainActor
@Observable
final class AppModel {
  /// True while a page that spans every server is showing (Servers, Virtual
  /// Machines, Operations, Pending Requests): refresh and notices then cover all servers.
  var fleetPageActive = true

  @ObservationIgnored let registry: ServerRegistry
  @ObservationIgnored let routes: ServerRoutes
  @ObservationIgnored let refresher: RefreshCoordinator
  @ObservationIgnored let mutations: MutationCoordinator
  @ObservationIgnored let noticeCenter: NoticeCenter

  init(
    client: RpcClient = RpcClient(), settingsStore: SettingsStore = .applicationStore(),
    ledgerStore: LedgerStore = .defaultStore(), observationClient: ObservationClient? = nil
  ) {
    let observationClient = observationClient ?? ObservationClient(runner: client.runner)
    let registry = ServerRegistry(
      client: client, observationClient: observationClient, settingsStore: settingsStore)
    let routes = ServerRoutes(registry: registry, client: client)
    let refresher = RefreshCoordinator(
      registry: registry, routes: routes, client: client, observationClient: observationClient)
    let noticeCenter = NoticeCenter(registry: registry)
    self.registry = registry
    self.routes = routes
    self.refresher = refresher
    self.noticeCenter = noticeCenter
    mutations = MutationCoordinator(
      registry: registry, routes: routes, refresher: refresher, notices: noticeCenter,
      client: client, ledgerStore: ledgerStore)
    registry.lifecycle = refresher
  }

  // MARK: Servers and sessions

  var settings: AppSettings { registry.settings }
  var settingsRevision: UUID { registry.settingsRevision }
  var sessions: [UUID: ServerSession] { registry.sessions }
  var selectedServerId: UUID? { registry.selectedServerId }
  var selectedServer: ServerProfile? { registry.selectedServer }
  var isReadOnly: Bool { registry.isReadOnly }
  var fleetServers: [ServerProfile] { registry.fleetServers }
  var requestOrigin: MutationOrigin? { registry.requestOrigin }
  func origin(forServer id: UUID) -> MutationOrigin? { registry.origin(forServer: id) }
  func serverName(on origin: MutationOrigin?) -> String { registry.serverName(on: origin) }
  var configurationIssues: [String] { registry.configurationIssues }
  var isConfigured: Bool { registry.isConfigured }
  var currentProfilePath: String { registry.currentProfilePath }
  func selectServer(_ id: UUID) { registry.selectServer(id) }
  @discardableResult
  func applySettings(_ newSettings: AppSettings, expectedRevision: UUID? = nil) -> Bool {
    registry.applySettings(newSettings, expectedRevision: expectedRevision)
  }
  /// Local inspection only. Rust opens the profile and key; Swift receives public identifiers.
  func pinServer(_ id: UUID) async throws(RpcFailure) { try await registry.pinServer(id) }

  func dashboard(forServer id: UUID) -> DashboardResult? { sessions[id]?.dashboard }
  func inventory(forServer id: UUID) -> HostInventory? { sessions[id]?.hostInventory }
  func inventoryFailure(forServer id: UUID) -> RpcFailure? { sessions[id]?.inventoryFailure }
  func failure(forServer id: UUID) -> RpcFailure? { sessions[id]?.refreshFailure }

  var dashboard: DashboardResult? { registry.selectedSession.dashboard }
  var observation: ObservationResult? { registry.selectedSession.observation }
  var lastRefreshedAt: Date? { registry.selectedSession.lastRefreshedAt }
  var isRefreshing: Bool { registry.selectedSession.isRefreshing }
  var refreshFailure: RpcFailure? { registry.selectedSession.refreshFailure }
  var permissions: Permissions { Permissions(identity: isReadOnly ? nil : dashboard?.identity) }
  var canCreateVm: Bool {
    !isReadOnly && dashboard?.canCreateManagedVm == true
      && (selectedServer?.expectedBackend == nil
        || selectedServer?.expectedBackend == dashboard?.backend)
  }
  /// Active operations across every controller, for the Operations sidebar badge.
  var activeOperationCount: Int {
    fleetServers.reduce(0) {
      $0 + (dashboard(forServer: $1.id)?.operations.filter { $0.status.isActive }.count ?? 0)
    }
  }
  func identityUser() -> User? { dashboard?.identity }
  var fleetVmCount: Int {
    sessions.values.reduce(0) {
      $0 + ($1.dashboard?.vms.filter { $0.state != .deleted }.count ?? 0)
    }
  }

  // MARK: Notices

  var notices: [Notice] { noticeCenter.visible(fleetPageActive: fleetPageActive) }
  func dismissNotice(_ notice: Notice) { noticeCenter.dismiss(notice) }

  // MARK: Refresh, inventory and polling

  var fleetIsRefreshing: Bool { refresher.fleetIsRefreshing }
  func refreshing(_ id: UUID) -> Bool { refresher.refreshing(id) }
  func freshness(_ id: UUID) -> String { refresher.freshness(id) }
  func refreshForPage() {
    if fleetPageActive {
      refreshAll()
      refreshServers(onlyStale: true)
    } else {
      refresh()
    }
  }
  func refresh() { refresher.refresh() }
  func refresh(_ id: UUID) { refresher.refresh(id) }
  func refreshServer(_ id: UUID) { refresher.refreshServer(id) }
  func refreshServers(onlyStale: Bool = false) { refresher.refreshServers(onlyStale: onlyStale) }
  func refreshAll(onlyStale: Bool = false) { refresher.refreshAll(onlyStale: onlyStale) }
  func loadFleetIfNeeded() { refresher.loadFleetIfNeeded() }
  func isAdmin(onServer id: UUID) -> Bool { refresher.isAdmin(onServer: id) }
  func shouldPoll(_ id: UUID) -> Bool { refresher.shouldPoll(id) }
  func autoRefreshPaused(_ id: UUID) -> Bool { refresher.autoRefreshPaused(id) }
  func retryAutoRefresh(_ id: UUID) { refresher.retryAutoRefresh(id) }
  func cancelRefresh() { refresher.cancelRefresh() }
  func cancelAllRefreshes() { refresher.cancelAllRefreshes() }
  func setPolling(_ enabled: Bool) { refresher.setPolling(enabled) }
  func setPolling(_ enabled: Bool, forServer id: UUID) {
    refresher.setPolling(enabled, forServer: id)
  }
  func pollingEnabled(forServer id: UUID) -> Bool { sessions[id]?.pollingEnabled == true }
  func loadHost(on origin: MutationOrigin) async { await refresher.loadHost(on: origin) }
  func loadDashboard(on origin: MutationOrigin) async throws(RpcFailure) -> DashboardResult {
    try await refresher.loadDashboard(on: origin)
  }
  func loadImageCatalog(origin: MutationOrigin?) async throws(RpcFailure) -> ImageCatalog {
    try await refresher.loadImageCatalog(origin: origin)
  }

  // MARK: Routes and queries

  /// Plans and inspections belong to the exact route captured when their form
  /// opened. A late reply must never authorize a mutation on another server.
  func query<Value: Decodable & Sendable>(
    _ request: RpcRequest, origin: MutationOrigin?, as type: Value.Type
  ) async throws(RpcFailure) -> Value {
    try await routes.query(request, origin: origin, as: type)
  }
  /// Used by a reviewed two-server workflow. Both routes carry independently
  /// pinned identities; choosing a destination never changes the selected host.
  func queryOnRoute<Value: Decodable & Sendable>(
    _ request: RpcRequest, origin: MutationOrigin, as type: Value.Type
  ) async throws(RpcFailure) -> Value {
    try await routes.queryOnRoute(request, origin: origin, as: type)
  }
  /// A fleet Start requires affirmative, recent observations from every peer.
  func startIssue(vm: Vm, on serverId: UUID, now: Date = Date()) -> String? {
    routes.startIssue(vm: vm, on: serverId, now: now)
  }

  // MARK: Mutations and pending requests

  var ledger: MutationLedger { mutations.ledger }
  var ledgerStorageIssue: String? { mutations.ledgerStorageIssue }
  var mutationSendStates: [UUID: RpcSendState] { mutations.mutationSendStates }
  var pendingForCurrentProfile: [PendingMutation] { mutations.pendingForCurrentProfile }
  var pendingForOtherProfiles: [PendingMutation] { mutations.pendingForOtherProfiles }
  func isRoutable(_ entry: PendingMutation) -> Bool { mutations.isRoutable(entry) }
  var unresolvedCount: Int { mutations.unresolvedCount }
  @discardableResult
  func submit(_ request: RpcRequest, origin: MutationOrigin?) -> UUID? {
    mutations.submit(request, origin: origin)
  }
  /// Returns typed migration grants/receipts only after the same durable
  /// request bookkeeping used by ordinary forms.
  func performMutation(_ request: RpcRequest, origin: MutationOrigin) async throws(RpcFailure)
    -> MutationAcknowledgement
  {
    try await mutations.performMutation(request, origin: origin)
  }
  func canRetry(_ entry: PendingMutation) -> Bool { mutations.canRetry(entry) }
  func retryIssue(_ entry: PendingMutation) -> String? { mutations.retryIssue(entry) }
  func retry(_ id: UUID) { mutations.retry(id) }
  func cancelMutation(_ id: UUID) { mutations.cancelMutation(id) }
  func discard(_ id: UUID) { mutations.discard(id) }
  func isMutationInFlight(_ id: UUID) -> Bool { mutations.isMutationInFlight(id) }
  func isVmBusy(_ vmId: UInt16, on serverId: UUID) -> Bool {
    mutations.isVmBusy(vmId, on: serverId)
  }
  func isVmBusy(_ vmId: UInt16) -> Bool { mutations.isVmBusy(vmId) }
  func inFlightEntry(vmId: UInt16, on serverId: UUID) -> PendingMutation? {
    mutations.inFlightEntry(vmId: vmId, on: serverId)
  }
  func sendState(vmId: UInt16, on serverId: UUID) -> RpcSendState? {
    mutations.sendState(vmId: vmId, on: serverId)
  }
  func serverName(for entry: PendingMutation) -> String { mutations.serverName(for: entry) }
}
