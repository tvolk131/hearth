import Darwin
import Foundation

public struct ProcessLimits: Hashable, Sendable {
    public var stdoutLimit: Int
    public var stderrLimit: Int
    public var timeout: TimeInterval
    public var terminateGrace: TimeInterval
    public var drainGrace: TimeInterval

    public init(
        stdoutLimit: Int = 8 * 1024 * 1024, stderrLimit: Int = 64 * 1024, timeout: TimeInterval = 60,
        terminateGrace: TimeInterval = 2, drainGrace: TimeInterval = 1
    ) {
        self.stdoutLimit = stdoutLimit
        self.stderrLimit = stderrLimit
        self.timeout = timeout
        self.terminateGrace = terminateGrace
        self.drainGrace = drainGrace
    }
}

public struct ProcessInvocation: Hashable, Sendable {
    public var executableURL: URL
    public var arguments: [String]
    public var standardInput: Data
    public var environment: [String: String]
    public var limits: ProcessLimits

    public init(
        executableURL: URL, arguments: [String], standardInput: Data, environment: [String: String],
        limits: ProcessLimits
    ) {
        self.executableURL = executableURL
        self.arguments = arguments
        self.standardInput = standardInput
        self.environment = environment
        self.limits = limits
    }
}

public struct ProcessResult: Hashable, Sendable {
    public var terminationStatus: Int32
    public var terminatedBySignal: Bool
    public var stdout: Data
    public var stderr: Data
    public var stdoutOverflowed: Bool
    public var stderrOverflowed: Bool

    public init(
        terminationStatus: Int32, terminatedBySignal: Bool, stdout: Data, stderr: Data,
        stdoutOverflowed: Bool = false, stderrOverflowed: Bool = false
    ) {
        self.terminationStatus = terminationStatus
        self.terminatedBySignal = terminatedBySignal
        self.stdout = stdout
        self.stderr = stderr
        self.stdoutOverflowed = stdoutOverflowed
        self.stderrOverflowed = stderrOverflowed
    }
}

public enum ProcessRunFailure: Error, Hashable, Sendable {
    case launchFailed(String)
    case timedOut
    case cancelled
    case stdoutOverflow
}

public protocol ProcessRunning: Sendable {
    func run(_ invocation: ProcessInvocation) async throws(ProcessRunFailure) -> ProcessResult
}

public struct ChildProcessRunner: ProcessRunning {
    public init() {}

    public func run(_ invocation: ProcessInvocation) async throws(ProcessRunFailure) -> ProcessResult {
        let session = ChildSession(invocation: invocation)
        let outcome = await withTaskCancellationHandler {
            await session.start()
        } onCancel: {
            session.cancel()
        }
        return try outcome.get()
    }
}

private final class ChildSession: @unchecked Sendable {
    private enum Stream {
        case stdout
        case stderr
    }

    private let invocation: ProcessInvocation
    private let lock = NSLock()
    private let queue = DispatchQueue(label: "pve-dev.child-session")

    private var continuation: CheckedContinuation<Result<ProcessResult, ProcessRunFailure>, Never>?
    private var process: Process?
    private var outputHandles: [FileHandle] = []
    private var pid: pid_t = 0
    private var launched = false
    private var exited = false
    private var finished = false
    private var cancelled = false
    private var timedOut = false
    private var stdoutOverflow = false
    private var stderrOverflow = false
    private var stdoutEOF = false
    private var stderrEOF = false
    private var drainDeadlinePassed = false
    private var terminationRequested = false
    private var status: Int32 = 0
    private var signaled = false
    private var stdout = Data()
    private var stderr = Data()
    private var timeoutItem: DispatchWorkItem?

    init(invocation: ProcessInvocation) {
        self.invocation = invocation
    }

    func start() async -> Result<ProcessResult, ProcessRunFailure> {
        await withCheckedContinuation { continuation in
            lock.withLock { self.continuation = continuation }
            launch()
        }
    }

    func cancel() {
        lock.withLock {
            cancelled = true
            requestTerminationLocked()
        }
        finishIfPossible()
    }

    private func launch() {
        let process = Process()
        process.executableURL = invocation.executableURL
        process.arguments = invocation.arguments
        process.environment = invocation.environment
        process.currentDirectoryURL = URL(fileURLWithPath: "/", isDirectory: true)

        let input = Pipe()
        let output = Pipe()
        let errors = Pipe()
        process.standardInput = input
        process.standardOutput = output
        process.standardError = errors
        _ = fcntl(input.fileHandleForWriting.fileDescriptor, F_SETNOSIGPIPE, 1)

        output.fileHandleForReading.readabilityHandler = { [self] handle in
            consume(handle.availableData, from: .stdout, handle: handle)
        }
        errors.fileHandleForReading.readabilityHandler = { [self] handle in
            consume(handle.availableData, from: .stderr, handle: handle)
        }
        process.terminationHandler = { [self] child in
            processExited(status: child.terminationStatus, reason: child.terminationReason)
        }

        let shouldLaunch = lock.withLock { () -> Bool in
            outputHandles = [output.fileHandleForReading, errors.fileHandleForReading]
            return !cancelled
        }
        guard shouldLaunch else {
            abandon(output: output, errors: errors, process: process, failure: .cancelled)
            return
        }

        do {
            try process.run()
        } catch {
            let reason = SafeText.sanitize(error.localizedDescription, limit: 300)
            abandon(output: output, errors: errors, process: process, failure: .launchFailed(reason))
            return
        }

        try? output.fileHandleForWriting.close()
        try? errors.fileHandleForWriting.close()
        try? input.fileHandleForReading.close()

        let timeoutItem = DispatchWorkItem { [self] in timeoutFired() }
        lock.withLock {
            self.process = process
            self.pid = process.processIdentifier
            self.launched = true
            self.timeoutItem = timeoutItem
            if cancelled {
                requestTerminationLocked()
            }
        }
        queue.asyncAfter(deadline: .now() + invocation.limits.timeout, execute: timeoutItem)

        let payload = invocation.standardInput
        let writer = input.fileHandleForWriting
        DispatchQueue.global(qos: .userInitiated).async {
            try? writer.write(contentsOf: payload)
            try? writer.close()
        }
    }

    private func abandon(output: Pipe, errors: Pipe, process: Process, failure: ProcessRunFailure) {
        output.fileHandleForReading.readabilityHandler = nil
        errors.fileHandleForReading.readabilityHandler = nil
        process.terminationHandler = nil
        let continuation = lock.withLock { () -> CheckedContinuation<Result<ProcessResult, ProcessRunFailure>, Never>? in
            finished = true
            outputHandles = []
            let pending = self.continuation
            self.continuation = nil
            return pending
        }
        continuation?.resume(returning: .failure(failure))
    }

    private func consume(_ data: Data, from stream: Stream, handle: FileHandle) {
        if data.isEmpty {
            handle.readabilityHandler = nil
            lock.withLock {
                switch stream {
                case .stdout: stdoutEOF = true
                case .stderr: stderrEOF = true
                }
            }
            finishIfPossible()
            return
        }
        lock.withLock {
            switch stream {
            case .stdout:
                if stdout.count + data.count > invocation.limits.stdoutLimit {
                    stdoutOverflow = true
                    requestTerminationLocked()
                } else if !stdoutOverflow {
                    stdout.append(data)
                }
            case .stderr:
                let room = invocation.limits.stderrLimit - stderr.count
                if room >= data.count {
                    stderr.append(data)
                } else {
                    if room > 0 { stderr.append(data.prefix(room)) }
                    stderrOverflow = true
                }
            }
        }
    }

    private func processExited(status: Int32, reason: Process.TerminationReason) {
        lock.withLock {
            exited = true
            self.status = status
            signaled = reason == .uncaughtSignal
            timeoutItem?.cancel()
        }
        queue.asyncAfter(deadline: .now() + invocation.limits.drainGrace) { [self] in
            lock.withLock { drainDeadlinePassed = true }
            finishIfPossible()
        }
        finishIfPossible()
    }

    private func timeoutFired() {
        lock.withLock {
            guard !exited else { return }
            timedOut = true
            requestTerminationLocked()
        }
    }

    private func requestTerminationLocked() {
        guard launched, !exited, !terminationRequested, pid > 0 else { return }
        terminationRequested = true
        let target = pid
        _ = kill(target, SIGTERM)
        queue.asyncAfter(deadline: .now() + invocation.limits.terminateGrace) { [self] in
            lock.withLock {
                guard !exited else { return }
                _ = kill(target, SIGKILL)
            }
        }
    }

    private func finishIfPossible() {
        let completion = lock.withLock { () -> (CheckedContinuation<Result<ProcessResult, ProcessRunFailure>, Never>, Result<ProcessResult, ProcessRunFailure>, [FileHandle])? in
            guard !finished, launched, exited, (stdoutEOF && stderrEOF) || drainDeadlinePassed,
                let continuation
            else { return nil }
            finished = true
            self.continuation = nil
            let handles = outputHandles
            outputHandles = []
            process?.terminationHandler = nil
            process = nil
            let result: Result<ProcessResult, ProcessRunFailure>
            if cancelled {
                result = .failure(.cancelled)
            } else if timedOut {
                result = .failure(.timedOut)
            } else if stdoutOverflow {
                result = .failure(.stdoutOverflow)
            } else {
                result = .success(
                    ProcessResult(
                        terminationStatus: status, terminatedBySignal: signaled, stdout: stdout, stderr: stderr,
                        stdoutOverflowed: false, stderrOverflowed: stderrOverflow))
            }
            return (continuation, result, handles)
        }
        guard let (continuation, result, handles) = completion else { return }
        for handle in handles {
            handle.readabilityHandler = nil
        }
        continuation.resume(returning: result)
    }
}
