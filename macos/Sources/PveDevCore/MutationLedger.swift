import Darwin
import Foundation

public enum MutationPhase: String, Codable, Hashable, Sendable {
  case inFlight = "in_flight"
  case indeterminate
  case rejected
}

public struct PendingMutation: Codable, Hashable, Sendable, Identifiable {
  public let id: UUID
  public let request: RpcRequest
  public let profilePath: String
  public let serverId: UUID?
  public let identity: ServerIdentity?
  public let createdAt: Date
  public var attempts: Int
  public var lastAttemptAt: Date?
  public var phase: MutationPhase
  public var lastErrorTitle: String?
  public var lastErrorDetail: String?

  public var wireRequestId: String? {
    request.requestId.map(WireUUID.string)
  }

  public var hasServerBinding: Bool { serverId != nil && identity?.isValid == true }
  public var canRetry: Bool { phase == .indeterminate && hasServerBinding }
  public var isInFlight: Bool { phase == .inFlight }
}

public enum LedgerError: Error, Hashable, Sendable {
  case unknownMutation
  case alreadyInFlight
  case rejectedByServer
  case profileMismatch
  case serverMismatch
  case unboundIdentity
}

public struct MutationLedger: Codable, Hashable, Sendable {
  public private(set) var entries: [PendingMutation]

  public init(entries: [PendingMutation] = []) {
    self.entries = entries
  }

  public func entry(_ id: UUID) -> PendingMutation? {
    entries.first { $0.id == id }
  }

  public func unresolved(forProfile profilePath: String) -> [PendingMutation] {
    entries.filter { $0.profilePath == profilePath && $0.phase != .rejected }
  }

  public func isBusy(vmId: UInt16, serverId: UUID, identity: ServerIdentity) -> Bool {
    entries.contains { entry in
      guard entry.serverId == serverId, entry.identity == identity else { return false }
      guard entry.phase != .rejected else { return false }
      switch entry.request {
      case .vmAction(let action): return action.vmId == vmId
      case .growVm(let action): return action.vmId == vmId
      case .enrollVm(let action): return action.vmId == vmId
      case .migration(let action): return action.vmId == vmId
      case .shrinkVm(let action): return action.vmId == vmId
      case .finishVmShrink(let action): return action.vmId == vmId
      default: return false
      }
    }
  }

  @discardableResult
  public mutating func prepare(
    _ request: RpcRequest, profilePath: String, serverId: UUID? = nil,
    identity: ServerIdentity? = nil, now: Date = Date()
  ) -> PendingMutation {
    if let existing = entries.first(where: {
      $0.profilePath == profilePath && $0.serverId == serverId && $0.identity == identity
        && $0.phase != .rejected && $0.request.hasSameIntent(as: request)
    }) {
      return existing
    }
    entries.removeAll {
      $0.profilePath == profilePath && $0.serverId == serverId && $0.identity == identity
        && $0.phase == .rejected && $0.request.hasSameIntent(as: request)
    }
    let entry = PendingMutation(
      id: UUID(),
      request: request,
      profilePath: profilePath,
      serverId: serverId,
      identity: identity,
      createdAt: now,
      attempts: 0,
      lastAttemptAt: nil,
      phase: .indeterminate,
      lastErrorTitle: nil,
      lastErrorDetail: nil)
    entries.append(entry)
    return entry
  }

  public mutating func beginAttempt(
    _ id: UUID, profilePath: String, serverId: UUID, identity: ServerIdentity,
    now: Date = Date()
  ) throws(LedgerError) -> RpcRequest {
    guard let index = entries.firstIndex(where: { $0.id == id }) else { throw .unknownMutation }
    guard entries[index].hasServerBinding else { throw .unboundIdentity }
    guard entries[index].serverId == serverId, entries[index].identity == identity else {
      throw .serverMismatch
    }
    switch entries[index].phase {
    case .inFlight: throw .alreadyInFlight
    case .rejected: throw .rejectedByServer
    case .indeterminate: break
    }
    guard entries[index].profilePath == profilePath else { throw .profileMismatch }
    entries[index].phase = .inFlight
    entries[index].attempts += 1
    entries[index].lastAttemptAt = now
    return entries[index].request
  }

  public mutating func recordAcknowledged(_ id: UUID) {
    entries.removeAll { $0.id == id }
  }

  public mutating func recordFailure(_ id: UUID, failure: RpcFailure) {
    guard let index = entries.firstIndex(where: { $0.id == id }) else { return }
    entries[index].phase = failure.isDefinitiveRejection ? .rejected : .indeterminate
    entries[index].lastErrorTitle = failure.title
    entries[index].lastErrorDetail = failure.detail
  }

  /// Only the exact queue-not-launched path may undo its provisional attempt.
  /// Earlier uncertain attempts stay uncertain and keep their request ID.
  public mutating func recordNotLaunched(_ id: UUID) {
    guard let index = entries.firstIndex(where: { $0.id == id }) else { return }
    entries[index].phase = .indeterminate
    entries[index].attempts = max(0, entries[index].attempts - 1)
    if entries[index].attempts == 0 {
      entries[index].lastErrorTitle = "Not sent"
      entries[index].lastErrorDetail =
        "This attempt did not launch. The original request ID is retained."
      entries[index].lastAttemptAt = nil
    } else {
      // beginAttempt retains the previous uncertainty; do not erase its cause.
      let note = "The queued retry did not launch; the earlier outcome remains uncertain."
      let detail = entries[index].lastErrorDetail ?? "Earlier attempt unacknowledged."
      entries[index].lastErrorDetail = detail.contains(note) ? detail : detail + "\n" + note
    }
  }

  public mutating func discard(_ id: UUID) -> Bool {
    guard let index = entries.firstIndex(where: { $0.id == id }), entries[index].phase != .inFlight
    else {
      return false
    }
    entries.remove(at: index)
    return true
  }

  public mutating func recoverAfterLaunch() {
    for index in entries.indices where entries[index].phase == .inFlight {
      entries[index].phase = .indeterminate
      entries[index].lastErrorTitle = "Outcome unknown"
      entries[index].lastErrorDetail =
        "The app stopped before the server acknowledged this request. Refresh, then retry with the same request ID or discard it."
    }
  }
}

public struct LedgerStore: Sendable {
  public let fileURL: URL

  public init(fileURL: URL) {
    self.fileURL = fileURL
  }

  public static func defaultStore(fileManager: FileManager = .default, bundle: Bundle = .main)
    -> LedgerStore
  {
    let base =
      fileManager.urls(for: .applicationSupportDirectory, in: .userDomainMask).first
      ?? URL(fileURLWithPath: NSTemporaryDirectory())
    let namespace =
      bundle.object(forInfoDictionaryKey: "PveDevLedgerNamespace") as? String ?? "PveDevApp"
    let safeNamespace =
      namespace.range(of: "^[A-Za-z0-9.-]+$", options: .regularExpression) != nil
      ? namespace : "PveDevApp"
    return LedgerStore(
      fileURL: base.appendingPathComponent(safeNamespace, isDirectory: true)
        .appendingPathComponent("pending-mutations.json"))
  }

  public func load() throws -> MutationLedger {
    guard FileManager.default.fileExists(atPath: fileURL.path) else { return MutationLedger() }
    let data = try Data(contentsOf: fileURL)
    let decoder = JSONDecoder()
    decoder.dateDecodingStrategy = .iso8601
    var ledger = try decoder.decode(MutationLedger.self, from: data)
    ledger.recoverAfterLaunch()
    return ledger
  }

  public func save(_ ledger: MutationLedger) throws {
    let directory = fileURL.deletingLastPathComponent()
    try FileManager.default.createDirectory(
      at: directory, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
    let encoder = JSONEncoder()
    encoder.dateEncodingStrategy = .iso8601
    encoder.outputFormatting = [.sortedKeys, .prettyPrinted]
    let data = try encoder.encode(ledger)
    try data.write(to: fileURL, options: [.atomic])
    try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: fileURL.path)
    let file = try FileHandle(forWritingTo: fileURL)
    defer { try? file.close() }
    try file.synchronize()
    let descriptor = open(directory.path, O_RDONLY)
    guard descriptor >= 0 else { throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO) }
    defer { close(descriptor) }
    guard fsync(descriptor) == 0 else { throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO) }
  }
}
