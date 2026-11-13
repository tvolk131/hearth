import Foundation
import Testing

@testable import PveDevCore

private actor QueueRunner: ProcessRunning {
  private var pending: [String: CheckedContinuation<ProcessResult, Never>] = [:]
  private(set) var calls: [String] = []
  func run(_ invocation: ProcessInvocation) async throws(ProcessRunFailure) -> ProcessResult {
    let path = invocation.arguments[1]
    calls.append(path)
    return await withCheckedContinuation { pending[path] = $0 }
  }
  func has(_ path: String) -> Bool { pending[path] != nil }
  func finish(_ path: String) {
    pending.removeValue(forKey: path)?.resume(
      returning: ProcessResult(
        terminationStatus: 0,
        terminatedBySignal: false, stdout: Data(), stderr: Data()))
  }
}

private actor SendingGate {
  var waiting = false
  private var continuation: CheckedContinuation<Void, Never>?
  func pause() async {
    waiting = true
    await withCheckedContinuation { continuation = $0 }
  }
  func finish() {
    continuation?.resume()
    continuation = nil
  }
}

@Suite("Device identity RPC queues")
struct RpcQueueTests {
  private func config(_ path: String, device: String = "c") -> RpcConfiguration {
    RpcConfiguration(
      executableURL: URL(fileURLWithPath: "/usr/bin/true"), profilePath: path,
      expectedIdentity: ServerIdentity(
        serverId: String(repeating: "a", count: 64),
        environment: String(repeating: "b", count: 32),
        deviceId: String(repeating: device, count: 64)),
      limits: ProcessLimits())
  }
  private func wait(_ runner: QueueRunner, _ path: String) async throws {
    for _ in 0..<200 {
      if await runner.has(path) { return }
      try await Task.sleep(for: .milliseconds(5))
    }
    throw NSError(domain: "RPC did not start", code: 1)
  }
  @Test func sameDeviceUsesOneEndpointAndCancelledQueuedCallsNeverLaunch() async throws {
    let runner = QueueRunner()
    let shared = RpcClient(runner: runner)
    let first = Task { try await shared.execute(.dashboard, configuration: config("first")) }
    try await wait(runner, "first")
    let cancelled = Task {
      try await shared.execute(.dashboard, configuration: config("cancelled"))
    }
    try await Task.sleep(for: .milliseconds(30))
    #expect(await runner.calls == ["first"])
    cancelled.cancel()
    do {
      _ = try await cancelled.value
      Issue.record("Cancelled call succeeded")
    } catch { #expect(error as? RpcFailure == .cancelled) }
    let second = Task { try await shared.execute(.dashboard, configuration: config("second")) }
    try await Task.sleep(for: .milliseconds(30))
    #expect(await runner.calls == ["first"])
    await runner.finish("first")
    _ = try await first.value
    try await wait(runner, "second")
    await runner.finish("second")
    _ = try await second.value
    #expect(await runner.calls == ["first", "second"])
  }
  @Test func queuedReadDeadlineExpiresWithoutLaunchingOrStealingQueue() async throws {
    let runner = QueueRunner()
    let client = RpcClient(runner: runner)
    let first = Task { try await client.execute(.dashboard, configuration: config("active")) }
    try await wait(runner, "active")
    var short = config("expired")
    short.limits.timeout = 0.05
    do {
      _ = try await client.execute(.dashboard, configuration: short)
      Issue.record("Queued read exceeded its deadline")
    } catch { #expect(error as? RpcFailure == .timedOut(seconds: 0)) }
    #expect(await runner.calls == ["active"])
    let next = Task { try await client.execute(.dashboard, configuration: config("next")) }
    await runner.finish("active")
    _ = try await first.value
    try await wait(runner, "next")
    await runner.finish("next")
    _ = try await next.value
    #expect(await runner.calls == ["active", "next"])
  }

  @Test func independentDeviceIdentitiesCanConnectTogether() async throws {
    let runner = QueueRunner()
    let shared = RpcClient(runner: runner)
    let first = Task { try await shared.execute(.dashboard, configuration: config("first")) }
    try await wait(runner, "first")
    let second = Task {
      try await shared.execute(.dashboard, configuration: config("second", device: "d"))
    }
    try await wait(runner, "second")
    await runner.finish("first")
    await runner.finish("second")
    _ = try await first.value
    _ = try await second.value
  }
  @Test func mutationsOnAnotherServerTakePriorityOverQueuedReads() async throws {
    let runner = QueueRunner()
    let shared = RpcClient(runner: runner)
    let active = Task {
      try await shared.execute(.dashboard, configuration: config("offline-host"))
    }
    try await wait(runner, "offline-host")
    let read = Task { try await shared.execute(.dashboard, configuration: config("healthy-read")) }
    try await Task.sleep(for: .milliseconds(20))
    let mutation = Task {
      try await shared.execute(
        .vmAction(VmActionRequest(requestId: UUID(), vmId: 103, kind: .shutdown)),
        configuration: config("healthy-mutation"))
    }
    try await Task.sleep(for: .milliseconds(20))
    #expect(await runner.calls == ["offline-host"])
    await runner.finish("offline-host")
    _ = try await active.value
    try await wait(runner, "healthy-mutation")
    #expect(await runner.calls == ["offline-host", "healthy-mutation"])
    await runner.finish("healthy-mutation")
    _ = try await mutation.value
    try await wait(runner, "healthy-read")
    await runner.finish("healthy-read")
    _ = try await read.value
  }

  @Test func cancellationDuringSendingCallbackNeverStartsChild() async throws {
    let runner = QueueRunner()
    let gate = SendingGate()
    let client = RpcClient(runner: runner)
    let send = Task {
      try await client.execute(
        .dashboard, configuration: config("handoff"),
        onState: { state in
          if state == .sending { await gate.pause() }
        })
    }
    for _ in 0..<200 {
      if await gate.waiting { break }
      try await Task.sleep(for: .milliseconds(5))
    }
    #expect(await gate.waiting)
    send.cancel()
    await gate.finish()
    do {
      _ = try await send.value
      Issue.record("Cancelled handoff succeeded")
    } catch { #expect(error as? RpcFailure == .cancelled) }
    #expect(await runner.calls.isEmpty)
  }

}
