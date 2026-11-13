import Foundation

public struct RpcConfiguration: Hashable, Sendable {
  public var executableURL: URL
  public var profilePath: String
  public var expectedIdentity: ServerIdentity?
  public var limits: ProcessLimits

  public init(
    executableURL: URL, profilePath: String, expectedIdentity: ServerIdentity? = nil,
    limits: ProcessLimits
  ) {
    self.executableURL = executableURL
    self.profilePath = profilePath
    self.expectedIdentity = expectedIdentity
    self.limits = limits
  }

  public var arguments: [String] {
    ["--profile", profilePath] + (expectedIdentity?.arguments ?? []) + ["rpc"]
  }
}

public enum ChildEnvironment {
  static let inheritedKeys = ["HOME", "USER", "LOGNAME", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE"]

  public static func make(from parent: [String: String] = ProcessInfo.processInfo.environment)
    -> [String: String]
  {
    var environment: [String: String] = [:]
    for key in inheritedKeys {
      if let value = parent[key] {
        environment[key] = value
      }
    }
    environment["PATH"] = "/usr/bin:/bin:/usr/sbin:/sbin"
    environment["NO_COLOR"] = "1"
    environment["RUST_BACKTRACE"] = "0"
    return environment
  }
}

public enum RpcSendState: String, Sendable {
  case queued = "Queued"
  case sending = "Sending"
}

public struct RpcClient: Sendable {
  public let runner: any ProcessRunning
  public let environment: [String: String]
  private let queues = RpcProcessQueues()

  public init(
    runner: any ProcessRunning = ChildProcessRunner(),
    environment: [String: String] = ChildEnvironment.make()
  ) {
    self.runner = runner
    self.environment = environment
  }

  public func invocation(for request: RpcRequest, configuration: RpcConfiguration)
    throws(RpcFailure) -> ProcessInvocation
  {
    let body: Data
    do {
      body = try WireCoding.encodeRequest(request)
    } catch {
      throw .configuration("The request could not be encoded.")
    }
    return ProcessInvocation(
      executableURL: configuration.executableURL,
      arguments: configuration.arguments,
      standardInput: body + Data("\n".utf8),
      environment: environment,
      limits: configuration.limits)
  }

  public func execute(
    _ request: RpcRequest, configuration: RpcConfiguration,
    onState: (@Sendable (RpcSendState) async -> Void)? = nil,
    shouldLaunch: (@Sendable () async -> Bool)? = nil
  ) async throws(RpcFailure) -> ProcessResult {
    var invocation = try invocation(for: request, configuration: configuration)
    let started = ProcessInfo.processInfo.systemUptime
    let readBudget = request.isMutation ? nil : min(30, configuration.limits.timeout)
    // Every short-lived CLI creates an iroh endpoint. Concurrent endpoints
    // with the same device key can replace one another's relay route, even
    // when connecting to different servers. Keep network calls sequential.
    let queue = await queues.queue(for: configuration.expectedIdentity?.deviceId ?? "unverified")
    await onState?(.queued)
    guard await queue.acquire(UUID(), priority: request.isMutation, timeout: readBudget) else {
      if Task.isCancelled { throw .cancelled }
      throw .timedOut(seconds: Int(readBudget ?? configuration.limits.timeout))
    }
    if let readBudget {
      let remaining = readBudget - (ProcessInfo.processInfo.systemUptime - started)
      guard remaining > 0 else {
        await queue.release()
        throw .timedOut(seconds: Int(readBudget))
      }
      invocation.limits.timeout = remaining
    }
    // The gate runs only after this endpoint owns the device queue. A refusal
    // remains queued/not-launched and must not be mistaken for server rejection.
    let allowed = Task.isCancelled ? false : (await shouldLaunch?() ?? true)
    if !allowed {
      await queue.release()
      throw .cancelled
    }
    let outcome: Result<ProcessResult, ProcessRunFailure>
    do {
      if Task.isCancelled {
        outcome = .failure(.cancelled)
      } else {
        await onState?(.sending)
        if Task.isCancelled {
          outcome = .failure(.cancelled)
        } else {
          outcome = .success(try await runner.run(invocation))
        }
      }
    } catch {
      outcome = .failure(error)
    }
    await queue.release()
    do {
      return try outcome.get()
    } catch {
      switch error {
      case .launchFailed(let reason): throw .launchFailed(reason)
      case .timedOut:
        throw .timedOut(seconds: Int((readBudget ?? configuration.limits.timeout).rounded()))
      case .cancelled: throw .cancelled
      case .stdoutOverflow: throw .outputTooLarge
      }
    }
  }

  public func profileIdentity(configuration: RpcConfiguration) async throws(RpcFailure)
    -> ServerIdentity
  {
    let invocation = ProcessInvocation(
      executableURL: configuration.executableURL,
      arguments: ["--profile", configuration.profilePath, "profile-info"], standardInput: Data(),
      environment: environment, limits: configuration.limits)
    let result: ProcessResult
    do {
      result = try await runner.run(invocation)
    } catch {
      switch error {
      case .launchFailed(let reason): throw .launchFailed(reason)
      case .timedOut: throw .timedOut(seconds: Int(configuration.limits.timeout.rounded()))
      case .cancelled: throw .cancelled
      case .stdoutOverflow: throw .outputTooLarge
      }
    }
    guard !result.stdoutOverflowed else { throw .outputTooLarge }
    guard result.terminationStatus == 0 && !result.terminatedBySignal else {
      throw .processFailed(
        status: result.terminationStatus, signaled: result.terminatedBySignal,
        stderr: SafeText.tail(result.stderr, limit: 1_500))
    }
    guard let identity = try? JSONDecoder().decode(ServerIdentity.self, from: result.stdout),
      identity.isValid
    else {
      throw .malformedResponse(
        "pve-devctl did not return valid public profile identities. Update the CLI if profile-info is unavailable."
      )
    }
    return identity
  }

  public func dashboard(configuration: RpcConfiguration) async throws(RpcFailure) -> DashboardResult
  {
    try await query(.dashboard, configuration: configuration, as: DashboardResult.self)
  }

  public func query<Value: Decodable & Sendable>(
    _ request: RpcRequest, configuration: RpcConfiguration, as type: Value.Type
  ) async throws(RpcFailure) -> Value {
    guard !request.isMutation else {
      throw .configuration("A read-only query cannot send a mutation.")
    }
    let result = try await execute(request, configuration: configuration)
    return try ResponseInterpreter.interpret(result, as: type)
  }

  public func submit(
    _ request: RpcRequest, configuration: RpcConfiguration,
    onState: (@Sendable (RpcSendState) async -> Void)? = nil,
    shouldLaunch: (@Sendable () async -> Bool)? = nil
  ) async throws(RpcFailure) -> MutationAcknowledgement {
    guard request.isMutation else {
      throw .configuration("Only mutation requests can be submitted.")
    }
    let result = try await execute(
      request, configuration: configuration, onState: onState, shouldLaunch: shouldLaunch)
    return try ResponseInterpreter.acknowledgement(for: request, from: result)
  }
}

private actor RpcProcessQueues {
  private var queues: [String: RpcProcessQueue] = [:]
  func queue(for device: String) -> RpcProcessQueue {
    if let queue = queues[device] { return queue }
    let queue = RpcProcessQueue()
    queues[device] = queue
    return queue
  }
}

private actor RpcProcessQueue {
  private var busy = false
  private var order: [UUID] = []
  private var priorities: Set<UUID> = []
  private var waiting: [UUID: CheckedContinuation<Bool, Never>] = [:]

  func acquire(_ id: UUID, priority: Bool, timeout: TimeInterval? = nil) async -> Bool {
    let deadlineTask = timeout.map { seconds in
      Task {
        do { try await Task.sleep(for: .seconds(seconds)) } catch { return }
        self.cancel(id)
      }
    }
    defer { deadlineTask?.cancel() }
    guard !Task.isCancelled else { return false }
    if !busy {
      busy = true
      return true
    }
    return await withTaskCancellationHandler {
      await withCheckedContinuation { continuation in
        if Task.isCancelled {
          continuation.resume(returning: false)
        } else {
          if priority {
            let index = order.firstIndex { !priorities.contains($0) } ?? order.endIndex
            order.insert(id, at: index)
            priorities.insert(id)
          } else {
            order.append(id)
          }
          waiting[id] = continuation
        }
      }
    } onCancel: {
      Task { await self.cancel(id) }
    }
  }

  private func cancel(_ id: UUID) {
    waiting.removeValue(forKey: id)?.resume(returning: false)
    order.removeAll { $0 == id }
    priorities.remove(id)
  }

  func release() {
    while !order.isEmpty {
      let id = order.removeFirst()
      priorities.remove(id)
      if let continuation = waiting.removeValue(forKey: id) {
        continuation.resume(returning: true)
        return
      }
    }
    busy = false
  }
}
