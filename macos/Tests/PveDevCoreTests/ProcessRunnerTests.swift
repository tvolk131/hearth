import Foundation
import Testing

@testable import PveDevCore

@Suite("Child process runner", .timeLimit(.minutes(1)))
struct ProcessRunnerTests {
    func invocation(_ path: String, _ arguments: [String], stdin: Data = Data(), limits: ProcessLimits = ProcessLimits(timeout: 10))
        -> ProcessInvocation
    {
        ProcessInvocation(
            executableURL: URL(fileURLWithPath: path), arguments: arguments, standardInput: stdin,
            environment: ["PATH": "/usr/bin:/bin"], limits: limits)
    }

    @Test func echoesStdinToStdout() async throws {
        let payload = Data(#"{"action":"dashboard"}"#.utf8)
        let result = try await ChildProcessRunner().run(invocation("/bin/cat", [], stdin: payload))
        #expect(result.stdout == payload)
        #expect(result.terminationStatus == 0)
        #expect(!result.terminatedBySignal)
    }

    @Test func largeStdinDoesNotDeadlock() async throws {
        let payload = Data(repeating: UInt8(ascii: "x"), count: 1_000_000)
        let result = try await ChildProcessRunner().run(invocation("/bin/cat", [], stdin: payload))
        #expect(result.stdout.count == payload.count)
    }

    @Test func childThatIgnoresStdinDoesNotCrashWriter() async throws {
        let payload = Data(repeating: 0, count: 2_000_000)
        let result = try await ChildProcessRunner().run(invocation("/usr/bin/true", [], stdin: payload))
        #expect(result.terminationStatus == 0)
    }

    @Test func capturesStderrAndExitStatus() async throws {
        let result = try await ChildProcessRunner().run(invocation("/bin/ls", ["/nonexistent-pve-dev-path"]))
        #expect(result.terminationStatus != 0)
        #expect(!result.stderr.isEmpty)
        #expect(result.stdout.isEmpty)
    }

    @Test func stderrIsBounded() async throws {
        let limits = ProcessLimits(stdoutLimit: 1024, stderrLimit: 16, timeout: 10)
        let result = try await ChildProcessRunner().run(
            invocation("/bin/ls", ["/nonexistent-pve-dev-path-with-a-long-name"], limits: limits))
        #expect(result.stderr.count == 16)
        #expect(result.stderrOverflowed)
    }

    @Test func stdoutOverflowTerminatesChild() async {
        let limits = ProcessLimits(stdoutLimit: 64 * 1024, stderrLimit: 1024, timeout: 20)
        let start = Date()
        await #expect(throws: ProcessRunFailure.stdoutOverflow) {
            try await ChildProcessRunner().run(invocation("/usr/bin/yes", [], limits: limits))
        }
        #expect(Date().timeIntervalSince(start) < 10)
    }

    @Test func timeoutTerminatesChild() async {
        let limits = ProcessLimits(timeout: 0.3, terminateGrace: 0.5)
        let start = Date()
        await #expect(throws: ProcessRunFailure.timedOut) {
            try await ChildProcessRunner().run(invocation("/bin/sleep", ["30"], limits: limits))
        }
        #expect(Date().timeIntervalSince(start) < 5)
    }

    @Test func cancellationTerminatesOnlyItsChild() async throws {
        let runner = ChildProcessRunner()
        let bystander = Task {
            try await runner.run(invocation("/bin/sleep", ["1"]))
        }
        let target = Task {
            try await runner.run(invocation("/bin/sleep", ["30"]))
        }
        try await Task.sleep(for: .milliseconds(200))
        let start = Date()
        target.cancel()
        do {
            _ = try await target.value
            Issue.record("expected cancellation")
        } catch let error as ProcessRunFailure {
            #expect(error == .cancelled)
        }
        #expect(Date().timeIntervalSince(start) < 5)
        let survivor = try await bystander.value
        #expect(survivor.terminationStatus == 0)
        #expect(!survivor.terminatedBySignal)
    }

    @Test func cancellationBeforeLaunchNeverStartsChild() async {
        let task = Task {
            withUnsafeCurrentTask { $0?.cancel() }
            return try await ChildProcessRunner().run(invocation("/bin/sleep", ["30"]))
        }
        await #expect(throws: ProcessRunFailure.cancelled) {
            try await task.value
        }
    }

    @Test func missingExecutableIsLaunchFailure() async {
        do {
            _ = try await ChildProcessRunner().run(invocation("/nonexistent/pve-devctl", []))
            Issue.record("expected launch failure")
        } catch {
            guard case .launchFailed = error else {
                Issue.record("unexpected \(error)")
                return
            }
        }
    }

    @Test func rpcClientMapsRunnerFailuresAndDecodes() async throws {
        let configuration = RpcConfiguration(
            executableURL: URL(fileURLWithPath: "/bin/sleep"), profilePath: "/tmp/p.json",
            limits: ProcessLimits(timeout: 0.3, terminateGrace: 0.3))
        do {
            _ = try await RpcClient(runner: ChildProcessRunner()).dashboard(configuration: configuration)
            Issue.record("expected failure")
        } catch {
            guard case .processFailed = error else {
                Issue.record("sleep rejects the arguments, expected process failure, got \(error)")
                return
            }
        }
    }
}

final class StubRunner: ProcessRunning, @unchecked Sendable {
    private let lock = NSLock()
    private var invocations: [ProcessInvocation] = []
    let response: Result<ProcessResult, ProcessRunFailure>

    init(_ response: Result<ProcessResult, ProcessRunFailure>) {
        self.response = response
    }

    var recorded: [ProcessInvocation] { lock.withLock { invocations } }

    func run(_ invocation: ProcessInvocation) async throws(ProcessRunFailure) -> ProcessResult {
        lock.withLock { invocations.append(invocation) }
        return try response.get()
    }
}

@Suite("RPC client")
struct RpcClientTests {
    let configuration = RpcConfiguration(
        executableURL: URL(fileURLWithPath: "/usr/local/bin/pve-devctl"), profilePath: "/Users/me/profile.json",
        limits: ProcessLimits(timeout: 42))

    @Test func dashboardSendsOneRequestAndDecodes() async throws {
        let stub = StubRunner(
            .success(ProcessResult(terminationStatus: 0, terminatedBySignal: false, stdout: Data(dashboardJSON.utf8), stderr: Data())))
        let dashboard = try await RpcClient(runner: stub, environment: [:]).dashboard(configuration: configuration)
        #expect(dashboard.identity.userId == "erin")
        let sent = try #require(stub.recorded.first)
        #expect(stub.recorded.count == 1)
        #expect(sent.arguments == ["--profile", "/Users/me/profile.json", "rpc"])
        #expect(sent.executableURL.path == "/usr/local/bin/pve-devctl")
        #expect(sent.limits.timeout > 0 && sent.limits.timeout <= 30)
    }

    @Test func submitSendsTheSameRequestIdOnEveryAttempt() async throws {
        let stub = StubRunner(.failure(.timedOut))
        let client = RpcClient(runner: stub, environment: [:])
        let request = RpcRequest.vmAction(VmActionRequest(requestId: UUID(), vmId: 9001, kind: .delete))
        for _ in 0..<2 {
            await #expect(throws: RpcFailure.timedOut(seconds: 42)) {
                try await client.submit(request, configuration: configuration)
            }
        }
        let bodies = stub.recorded.map(\.standardInput)
        #expect(bodies.count == 2)
        #expect(bodies[0] == bodies[1])
        let decoded = try JSONDecoder().decode(RpcRequest.self, from: bodies[0])
        #expect(decoded.requestId == request.requestId)
    }

    @Test func submitRejectsDashboard() async {
        let stub = StubRunner(.failure(.cancelled))
        await #expect(throws: RpcFailure.self) {
            try await RpcClient(runner: stub, environment: [:]).submit(.dashboard, configuration: configuration)
        }
        #expect(stub.recorded.isEmpty)
    }

    @Test(arguments: [
        (ProcessRunFailure.cancelled, RpcFailure.cancelled),
        (.stdoutOverflow, .outputTooLarge),
        (.launchFailed("no such file"), .launchFailed("no such file")),
    ])
    func runnerFailuresMap(runnerFailure: ProcessRunFailure, expected: RpcFailure) async {
        let stub = StubRunner(.failure(runnerFailure))
        await #expect(throws: expected) {
            try await RpcClient(runner: stub, environment: [:]).dashboard(configuration: configuration)
        }
    }
}
