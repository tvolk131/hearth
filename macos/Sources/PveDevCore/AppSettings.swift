import Foundation

public struct AppSettings: Codable, Hashable, Sendable {
  public static let timeoutRange: ClosedRange<Int> = 5...600
  public static let pollingRange: ClosedRange<Int> = 5...3600

  public var executablePath: String
  public var inventoryExecutablePath: String?
  public var servers: [ServerProfile]
  public var selectedServerId: UUID?

  public var selectedServer: ServerProfile? { servers.first { $0.id == selectedServerId } }
  public var isReadOnly: Bool { selectedServer?.isReadOnly == true }
  public var resolvedInventoryExecutablePath: String {
    if let path = inventoryExecutablePath, !path.isEmpty { return path }
    guard executablePath.hasPrefix("/") else { return "" }
    return URL(fileURLWithPath: executablePath).deletingLastPathComponent().appendingPathComponent(
      "pve-dev-inventory"
    ).path
  }

  public var profilePath: String {
    get { selectedServer?.profilePath ?? "" }
    set {
      if let index = servers.firstIndex(where: { $0.id == selectedServerId }) {
        servers[index].profilePath = newValue
      } else if !newValue.isEmpty {
        let server = ServerProfile(name: "Default Server", profilePath: newValue)
        servers.append(server)
        selectedServerId = server.id
      }
    }
  }
  public var timeoutSeconds: Int
  public var pollingEnabled: Bool
  public var pollingIntervalSeconds: Int

  public init(
    executablePath: String = "", profilePath: String = "", timeoutSeconds: Int = 60,
    pollingEnabled: Bool = false, pollingIntervalSeconds: Int = 15
  ) {
    self.executablePath = executablePath
    self.inventoryExecutablePath = nil
    self.servers = []
    self.selectedServerId = nil
    self.timeoutSeconds = timeoutSeconds
    self.pollingEnabled = pollingEnabled
    self.pollingIntervalSeconds = pollingIntervalSeconds
    self.profilePath = profilePath
  }

  enum CodingKeys: String, CodingKey {
    case executablePath, inventoryExecutablePath, servers, selectedServerId, profilePath
    case timeoutSeconds, pollingEnabled, pollingIntervalSeconds
  }

  public init(from decoder: any Decoder) throws {
    let values = try decoder.container(keyedBy: CodingKeys.self)
    executablePath = try values.decode(String.self, forKey: .executablePath)
    inventoryExecutablePath = try values.decodeIfPresent(
      String.self, forKey: .inventoryExecutablePath)
    timeoutSeconds = try values.decodeIfPresent(Int.self, forKey: .timeoutSeconds) ?? 60
    pollingEnabled = false
    pollingIntervalSeconds =
      try values.decodeIfPresent(Int.self, forKey: .pollingIntervalSeconds) ?? 15
    if let configured = try values.decodeIfPresent([ServerProfile].self, forKey: .servers) {
      servers = configured
      selectedServerId =
        try values.decodeIfPresent(UUID.self, forKey: .selectedServerId) ?? configured.first?.id
    } else {
      let path = try values.decodeIfPresent(String.self, forKey: .profilePath) ?? ""
      servers = path.isEmpty ? [] : [ServerProfile(name: "Default Server", profilePath: path)]
      selectedServerId = servers.first?.id
    }
  }

  public func encode(to encoder: any Encoder) throws {
    var values = encoder.container(keyedBy: CodingKeys.self)
    try values.encode(executablePath, forKey: .executablePath)
    try values.encodeIfPresent(inventoryExecutablePath, forKey: .inventoryExecutablePath)
    try values.encode(servers, forKey: .servers)
    try values.encodeIfPresent(selectedServerId, forKey: .selectedServerId)
    try values.encode(timeoutSeconds, forKey: .timeoutSeconds)
    try values.encode(false, forKey: .pollingEnabled)
    try values.encode(pollingIntervalSeconds, forKey: .pollingIntervalSeconds)
  }

  public var serverListIssues: [String] {
    var issues: [String] = []
    if servers.count > 32 { issues.append("Configure at most 32 servers.") }
    if !servers.isEmpty && selectedServer == nil { issues.append("Select a configured server.") }
    if Set(servers.map(\.id)).count != servers.count {
      issues.append("Server identifiers must be unique.")
    }
    if servers.contains(where: { $0.name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty })
    {
      issues.append("Give every server a name.")
    }
    let names = servers.map { $0.name.trimmingCharacters(in: .whitespacesAndNewlines).lowercased() }
    if Set(names).count != names.count {
      issues.append("Server names must be unique (ignoring case and surrounding spaces).")
    }
    let identities = servers.compactMap(\.identity)
    if identities.contains(where: { !$0.isValid }) {
      issues.append("A saved server identity is invalid. Inspect and trust its profile again.")
    }
    if Set(identities).count != identities.count {
      issues.append("This server and device identity are already configured under another name.")
    }
    let observations = servers.compactMap(\.observationIdentity)
    if observations.contains(where: { !$0.isValid }) {
      issues.append("A saved read-only host identity is invalid. Inspect its profile again.")
    }
    if Set(observations.map(\.binding)).count != observations.count {
      issues.append("This read-only host is already configured under another name.")
    }
    if servers.contains(where: {
      $0.isReadOnly ? $0.identity != nil : $0.observationIdentity != nil
    }) {
      issues.append(
        "Controller and read-only host identities cannot be combined in one server entry.")
    }
    return issues
  }

  public var clampedTimeout: Int {
    min(max(timeoutSeconds, Self.timeoutRange.lowerBound), Self.timeoutRange.upperBound)
  }

  public var clampedPollingInterval: Int {
    min(max(pollingIntervalSeconds, Self.pollingRange.lowerBound), Self.pollingRange.upperBound)
  }

  public func validationIssues(fileManager: FileManager = .default) -> [String] {
    var issues: [String] = []
    issues += Self.executableIssues(
      isReadOnly ? resolvedInventoryExecutablePath : executablePath, fileManager: fileManager,
      name: isReadOnly ? "pve-dev-inventory" : "pve-devctl")
    issues += Self.profileIssues(profilePath, fileManager: fileManager)
    return issues + serverListIssues
  }

  public func configuration(
    fileManager: FileManager = .default, requirePinnedIdentity: Bool = false
  ) throws(RpcFailure) -> RpcConfiguration {
    guard !isReadOnly else {
      throw .configuration("This host is read-only. Controller RPC and mutations are unavailable.")
    }
    var issues = validationIssues(fileManager: fileManager)
    if requirePinnedIdentity && selectedServer?.identity == nil {
      issues.append("Inspect and trust this server profile in Settings before connecting.")
    }
    guard issues.isEmpty else {
      throw .configuration(issues.joined(separator: "\n"))
    }
    let executable = URL(fileURLWithPath: executablePath).standardizedFileURL
    let profile = URL(fileURLWithPath: profilePath).standardizedFileURL.path
    return RpcConfiguration(
      executableURL: executable,
      profilePath: profile,
      expectedIdentity: selectedServer?.identity,
      limits: ProcessLimits(timeout: TimeInterval(clampedTimeout)))
  }

  public func observationConfiguration(
    fileManager: FileManager = .default, requirePinnedIdentity: Bool = true
  ) throws(RpcFailure) -> ObservationConfiguration {
    guard isReadOnly else {
      throw .configuration("Select a read-only host before observing inventory.")
    }
    var issues = validationIssues(fileManager: fileManager)
    if requirePinnedIdentity && selectedServer?.observationIdentity == nil {
      issues.append("Inspect and trust this read-only host profile before refreshing.")
    }
    guard issues.isEmpty else { throw .configuration(issues.joined(separator: "\n")) }
    return ObservationConfiguration(
      executableURL: URL(fileURLWithPath: resolvedInventoryExecutablePath).standardizedFileURL,
      profilePath: URL(fileURLWithPath: profilePath).standardizedFileURL.path,
      expectedIdentity: selectedServer?.observationIdentity,
      limits: ProcessLimits(timeout: TimeInterval(clampedTimeout)))
  }

  static func executableIssues(
    _ path: String, fileManager: FileManager, name: String = "pve-devctl"
  ) -> [String] {
    guard !path.isEmpty else { return ["Choose the \(name) executable in Settings."] }
    guard path.hasPrefix("/") else { return ["The \(name) path must be absolute."] }
    guard !path.contains("\0") else { return ["The \(name) path is not valid."] }
    var isDirectory: ObjCBool = false
    guard fileManager.fileExists(atPath: path, isDirectory: &isDirectory) else {
      return ["The \(name) executable does not exist."]
    }
    guard !isDirectory.boolValue else { return ["The \(name) path is a directory."] }
    guard fileManager.isExecutableFile(atPath: path) else {
      return ["The \(name) file is not executable."]
    }
    return []
  }

  static func profileIssues(_ path: String, fileManager: FileManager) -> [String] {
    guard !path.isEmpty else { return ["Choose the profile JSON file in Settings."] }
    guard path.hasPrefix("/") else { return ["The profile path must be absolute."] }
    guard !path.contains("\0") else { return ["The profile path is not valid."] }
    guard URL(fileURLWithPath: path).pathExtension.lowercased() == "json" else {
      return ["The profile must be a .json file. Never select key material or a .redb database."]
    }
    var isDirectory: ObjCBool = false
    guard fileManager.fileExists(atPath: path, isDirectory: &isDirectory) else {
      return ["The profile file does not exist."]
    }
    guard !isDirectory.boolValue else { return ["The profile path is a directory."] }
    return []
  }
}

public struct SettingsStore: Sendable {
  public static let defaultsKey = "PveDevApp.settings.v2"
  public static let legacyDefaultsKey = "PveDevApp.settings.v1"
  private let suiteName: String?
  private let bootstrapURL: URL?

  public init(suiteName: String? = nil, bootstrapURL: URL? = nil) {
    self.suiteName = suiteName
    self.bootstrapURL = bootstrapURL
  }

  /// Packaging may opt into an isolated suite and a public, pinned settings seed.
  /// A seed is used only when the suite has no settings; saved settings always win.
  public static func applicationStore(bundle: Bundle = .main) -> SettingsStore {
    let suite = bundle.object(forInfoDictionaryKey: "PveDevSettingsSuite") as? String
    let seed = bundle.url(forResource: "bootstrap-settings", withExtension: "json")
    return SettingsStore(suiteName: suite, bootstrapURL: seed)
  }

  private var defaults: UserDefaults {
    suiteName.flatMap { UserDefaults(suiteName: $0) } ?? .standard
  }

  public func load() -> AppSettings {
    if let data = defaults.data(forKey: Self.defaultsKey) {
      return (try? JSONDecoder().decode(AppSettings.self, from: data)) ?? AppSettings()
    }
    if let data = defaults.data(forKey: Self.legacyDefaultsKey),
      let migrated = try? JSONDecoder().decode(AppSettings.self, from: data)
    {
      save(migrated)
      return migrated
    }
    guard let bootstrapURL, let data = try? Data(contentsOf: bootstrapURL),
      let seed = try? JSONDecoder().decode(AppSettings.self, from: data),
      seed.servers.count == 2,
      seed.servers.allSatisfy({ !$0.isReadOnly && $0.identity?.isValid == true }),
      seed.serverListIssues.isEmpty
    else { return AppSettings() }
    save(seed)
    return seed
  }

  public func save(_ settings: AppSettings) {
    guard let data = try? JSONEncoder().encode(settings) else { return }
    defaults.set(data, forKey: Self.defaultsKey)
  }
}
