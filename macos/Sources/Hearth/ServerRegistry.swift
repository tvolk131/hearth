import Foundation
import Observation
import HearthCore

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

/// Told when applying settings replaces a server's session or changes polling.
@MainActor
protocol ServerSessionLifecycle: AnyObject {
  /// The server's session is about to be replaced or removed; stop its work.
  func sessionWillReset(_ id: UUID)
  /// The polling interval changed for a server that is polling.
  func pollingIntervalWillChange(_ id: UUID)
  /// New settings are saved; restart polling for servers that have it on.
  func settingsDidApply()
}

/// Session and server registry: settings, per-server sessions, selection,
/// explicit profile trust and the route identity (`MutationOrigin`) of each server.
@MainActor
@Observable
final class ServerRegistry {
  private(set) var settings: AppSettings
  private(set) var settingsRevision = UUID()
  private(set) var sessions: [UUID: ServerSession] = [:]
  private let emptySession = ServerSession()

  @ObservationIgnored weak var lifecycle: ServerSessionLifecycle?
  @ObservationIgnored private let client: RpcClient
  @ObservationIgnored private let observationClient: ObservationClient
  @ObservationIgnored private let settingsStore: SettingsStore

  init(client: RpcClient, observationClient: ObservationClient, settingsStore: SettingsStore) {
    self.client = client
    self.observationClient = observationClient
    self.settingsStore = settingsStore
    var loaded = settingsStore.load()
    loaded.pollingEnabled = false
    settings = loaded
    for server in loaded.servers { sessions[server.id] = ServerSession() }
  }

  var selectedServerId: UUID? { settings.selectedServerId }
  var selectedServer: ServerProfile? { settings.selectedServer }
  var isReadOnly: Bool { selectedServer?.isReadOnly == true }
  var selectedSession: ServerSession {
    selectedServerId.flatMap { sessions[$0] } ?? emptySession
  }
  /// Servers the fleet pages show and refresh. Read-only SSH hosts have their own pages.
  var fleetServers: [ServerProfile] { settings.servers.filter { !$0.isReadOnly } }

  func server(_ id: UUID?) -> ServerProfile? { settings.servers.first { $0.id == id } }

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
  func serverName(on origin: MutationOrigin?) -> String {
    settings.servers.first { $0.id == origin?.serverId }?.name ?? "Unavailable server"
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

  func configuration(for id: UUID, requireIdentity: Bool = true) throws(RpcFailure)
    -> RpcConfiguration
  {
    guard settings.servers.contains(where: { $0.id == id }) else {
      throw .configuration("This server is no longer configured.")
    }
    var snapshot = settings
    snapshot.selectedServerId = id
    return try snapshot.configuration(requirePinnedIdentity: requireIdentity)
  }

  func selectServer(_ id: UUID) {
    guard settings.servers.contains(where: { $0.id == id }) else { return }
    settings.selectedServerId = id
    settings.pollingEnabled = sessions[id]?.pollingEnabled ?? false
    settingsRevision = UUID()
    settingsStore.save(settings)
  }

  /// Mirrors the selected server's polling flag into settings (not persisted).
  func setSelectedPollingFlag(_ enabled: Bool) { settings.pollingEnabled = enabled }

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
        lifecycle?.sessionWillReset(id)
        sessions[id] = after == nil ? nil : ServerSession()
      } else if session.pollingEnabled
        && settings.pollingIntervalSeconds != stored.pollingIntervalSeconds
      {
        lifecycle?.pollingIntervalWillChange(id)
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
    lifecycle?.settingsDidApply()
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
}
