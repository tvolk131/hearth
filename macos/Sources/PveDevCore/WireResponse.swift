import Foundation

public struct ServerErrorCode: RawRepresentable, Codable, Hashable, Sendable {
  public let rawValue: String
  public init(rawValue: String) { self.rawValue = rawValue }

  public static let forbidden = ServerErrorCode(rawValue: "forbidden")
  public static let invalid = ServerErrorCode(rawValue: "invalid")
  public static let conflict = ServerErrorCode(rawValue: "conflict")
  public static let unavailable = ServerErrorCode(rawValue: "unavailable")
  public static let `internal` = ServerErrorCode(rawValue: "internal")

  public var isDefinitiveRejection: Bool {
    self == .forbidden || self == .invalid || self == .conflict
  }

  public var title: String {
    switch self {
    case .forbidden: "Not permitted"
    case .invalid: "Invalid request"
    case .conflict: "Conflict"
    case .unavailable: "Server unavailable"
    case .internal: "Server error"
    default: "Server error (\(SafeText.sanitize(rawValue, limit: 32)))"
    }
  }
}

public struct ServerError: Codable, Hashable, Sendable, Error {
  public var code: ServerErrorCode
  public var message: String

  public init(code: ServerErrorCode, message: String) {
    self.code = code
    self.message = message
  }

  public var displayMessage: String {
    SafeText.sanitize(message, limit: 500)
  }
}

struct ResponseEnvelope<Value: Decodable>: Decodable {
  let outcome: Result<Value, ServerError>

  enum CodingKeys: String, CodingKey {
    case ok
    case result
    case error
  }

  init(from decoder: any Decoder) throws {
    let container = try decoder.container(keyedBy: CodingKeys.self)
    if try container.decode(Bool.self, forKey: .ok) {
      outcome = .success(try container.decode(Value.self, forKey: .result))
    } else {
      outcome = .failure(try container.decode(ServerError.self, forKey: .error))
    }
  }
}

public enum MutationAcknowledgement: Hashable, Sendable {
  case operation(OperationRecord)
  case updated(Bool)
  case migrationGrant(SignedMigrationGrant)
  case migrationTransfer(MigrationTransferRecord)

  public var summary: String {
    switch self {
    case .operation(let record):
      "Operation \(record.operationId) \(record.status.rawValue)"
    case .migrationGrant(let grant):
      "Migration authorization \(SafeText.sanitize(grant.value.plan.migrationId, limit: 64)) recorded"
    case .migrationTransfer(let record): "Migration \(SafeText.sanitize(record.phase, limit: 40))"
    case .updated(true):
      "Updated"
    case .updated(false):
      "No change was needed"
    }
  }
}

public enum RpcFailure: Error, Hashable, Sendable {
  case configuration(String)
  case launchFailed(String)
  case timedOut(seconds: Int)
  case cancelled
  case outputTooLarge
  case processFailed(status: Int32, signaled: Bool, stderr: String)
  case malformedResponse(String)
  case server(ServerError)

  public var isServerAcknowledged: Bool {
    if case .server = self { return true }
    return false
  }

  public var isDefinitiveRejection: Bool {
    if case .server(let error) = self { return error.code.isDefinitiveRejection }
    return false
  }

  public var title: String {
    switch self {
    case .configuration: "App settings need attention"
    case .launchFailed: "Could not start pve-devctl"
    case .timedOut: "pve-devctl timed out"
    case .cancelled: "Cancelled"
    case .outputTooLarge: "Response too large"
    case .processFailed: "pve-devctl failed"
    case .malformedResponse: "Unreadable response"
    case .server(let error): error.code.title
    }
  }

  public var detail: String {
    switch self {
    case .configuration(let message):
      message
    case .launchFailed(let message):
      message
    case .timedOut(let seconds):
      "No response within \(seconds) seconds. The child process was terminated."
    case .cancelled:
      "The request was cancelled."
    case .outputTooLarge:
      "The backend produced more output than allowed. The child process was terminated."
    case .processFailed(let status, let signaled, let stderr):
      (signaled ? "Terminated by signal \(status)." : "Exited with status \(status).")
        + (stderr.isEmpty ? "" : "\n" + stderr)
    case .malformedResponse(let message):
      message
    case .server(let error):
      error.displayMessage
    }
  }
}

public enum ResponseInterpreter {
  public static func interpret<Value: Decodable>(_ result: ProcessResult, as type: Value.Type)
    throws(RpcFailure) -> Value
  {
    if result.stdoutOverflowed {
      throw .outputTooLarge
    }
    let exitedCleanly = !result.terminatedBySignal && result.terminationStatus == 0
    let envelope: ResponseEnvelope<Value>
    do {
      envelope = try JSONDecoder().decode(ResponseEnvelope<Value>.self, from: result.stdout)
    } catch {
      if !exitedCleanly {
        throw .processFailed(
          status: result.terminationStatus, signaled: result.terminatedBySignal,
          stderr: SafeText.tail(result.stderr, limit: 1_500))
      }
      throw .malformedResponse(describe(error))
    }
    switch envelope.outcome {
    case .failure(let serverError):
      throw .server(serverError)
    case .success(let value):
      guard exitedCleanly else {
        throw .processFailed(
          status: result.terminationStatus, signaled: result.terminatedBySignal,
          stderr: SafeText.tail(result.stderr, limit: 1_500))
      }
      return value
    }
  }

  public static func acknowledgement(for request: RpcRequest, from result: ProcessResult)
    throws(RpcFailure) -> MutationAcknowledgement
  {
    if case .migration(let action) = request {
      switch action {
      case .plan, .authorizeDestination, .renew:
        return .migrationGrant(try interpret(result, as: SignedMigrationGrant.self))
      case .start, .abort, .finish:
        return .migrationTransfer(try interpret(result, as: MigrationTransferRecord.self))
      default: throw .configuration("This migration request is read-only.")
      }
    }
    if request.expectsOperation {
      return .operation(try interpret(result, as: OperationRecord.self))
    }
    return .updated(try interpret(result, as: UpdatedResult.self).updated)
  }

  static func describe(_ error: any Error) -> String {
    guard let decodingError = error as? DecodingError else {
      return "The backend response was not valid JSON."
    }
    let path: [any CodingKey]
    let reason: String
    switch decodingError {
    case .dataCorrupted(let context):
      path = context.codingPath
      reason = context.codingPath.isEmpty ? "not valid JSON" : "invalid value"
    case .keyNotFound(let key, let context):
      path = context.codingPath + [key]
      reason = "missing field"
    case .typeMismatch(_, let context):
      path = context.codingPath
      reason = "unexpected type"
    case .valueNotFound(_, let context):
      path = context.codingPath
      reason = "unexpected null"
    @unknown default:
      path = []
      reason = "unexpected format"
    }
    let location = path.map { $0.intValue.map { "[\($0)]" } ?? $0.stringValue }.joined(
      separator: ".")
    let suffix = location.isEmpty ? "" : " at \(SafeText.sanitize(location, limit: 120))"
    return "The backend response did not match the expected format (\(reason)\(suffix))."
  }
}
