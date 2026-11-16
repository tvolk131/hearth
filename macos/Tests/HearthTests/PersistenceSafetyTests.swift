import Foundation
import Testing
import HearthCore

@testable import Hearth

private actor CountingRunner: ProcessRunning {
    private(set) var calls = 0

    func run(_ invocation: ProcessInvocation) async throws(ProcessRunFailure) -> ProcessResult {
        calls += 1
        throw .launchFailed("A persistence failure must not reach the runner")
    }
}

@Suite("Fail-closed mutation persistence")
@MainActor
struct PersistenceSafetyTests {
    @Test(arguments: [false, true])
    func failedLedgerNeverSendsOrOverwritesUnreadableState(corrupt: Bool) async throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: directory) }
        let profile = directory.appendingPathComponent("profile.json")
        try Data("{}".utf8).write(to: profile)
        let suiteName = "PveDevApp.tests.\(UUID().uuidString)"
        let settings = SettingsStore(suiteName: suiteName)
        defer { UserDefaults(suiteName: suiteName)?.removePersistentDomain(forName: suiteName) }
        var configured = AppSettings(executablePath: "/usr/bin/true", profilePath: profile.path)
        configured.servers[0].identity = ServerIdentity(serverId: String(repeating: "a", count: 64), environment: String(repeating: "b", count: 32), deviceId: String(repeating: "c", count: 64))
        settings.save(configured)
        let ledgerURL = directory.appendingPathComponent(corrupt ? "pending.json" : "blocked/pending.json")
        let original = Data("unreadable-state".utf8)
        if corrupt {
            try original.write(to: ledgerURL)
        } else {
            try original.write(to: directory.appendingPathComponent("blocked"))
        }
        let runner = CountingRunner()
        let model = AppModel(client: RpcClient(runner: runner), settingsStore: settings, ledgerStore: LedgerStore(fileURL: ledgerURL))
        if !corrupt { #expect(model.ledgerStorageIssue == nil) }
        let request = RpcRequest.vmAction(VmActionRequest(requestId: UUID(), vmId: 9001, kind: .delete))
        #expect(model.submit(request, origin: model.requestOrigin) == nil)
        try await Task.sleep(for: .milliseconds(30))
        #expect(await runner.calls == 0)
        #expect(model.ledgerStorageIssue != nil)
        if corrupt {
            #expect(try Data(contentsOf: ledgerURL) == original)
        }
    }
}

extension PersistenceSafetyTests {
    @Test func unsavedAttemptIsRecordedAsNotSent() async throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: directory) }
        let profile = directory.appendingPathComponent("profile.json")
        try Data("{}".utf8).write(to: profile)
        let suiteName = "PveDevApp.tests.\(UUID().uuidString)"
        let settings = SettingsStore(suiteName: suiteName)
        defer { UserDefaults(suiteName: suiteName)?.removePersistentDomain(forName: suiteName) }
        var configured = AppSettings(executablePath: "/usr/bin/true", profilePath: profile.path)
        configured.servers[0].identity = ServerIdentity(serverId: String(repeating: "a", count: 64), environment: String(repeating: "b", count: 32), deviceId: String(repeating: "c", count: 64))
        settings.save(configured)
        // A file where the ledger directory should be makes every save fail.
        try Data("blocked".utf8).write(to: directory.appendingPathComponent("blocked"))
        let runner = CountingRunner()
        let model = AppModel(
            client: RpcClient(runner: runner), settingsStore: settings,
            ledgerStore: LedgerStore(fileURL: directory.appendingPathComponent("blocked/pending.json")))
        let origin = try #require(model.requestOrigin)
        let request = RpcRequest.vmAction(VmActionRequest(requestId: UUID(), vmId: 9001, kind: .delete))
        do {
            _ = try await model.performMutation(request, origin: origin)
            Issue.record("An unsaved request was sent")
        } catch {
            #expect(error.detail.contains("Nothing was sent"))
        }
        #expect(await runner.calls == 0)
        let entry = try #require(model.ledger.entries.first)
        #expect(entry.attempts == 0 && entry.phase == .indeterminate)
        #expect(entry.lastErrorTitle == "Not sent")
        #expect(entry.request == request)
    }
}
