import Foundation
import Testing
@testable import PveDevCore

private func observationJSON(binding: String = String(repeating: "b", count: 64), managed: Bool = false) throws -> Data {
    try JSONSerialization.data(withJSONObject: [
        "schema": "pve-dev-observation-result/1",
        "identity": ["host_id": String(repeating: "a", count: 64), "binding": binding,
            "node": "pve1", "host_key_sha256": "SHA256:" + String(repeating: "A", count: 43),
            "machine_id_sha256": String(repeating: "c", count: 64), "zpool_guid": "42", "zfs_root": "rpool/data",
            "known_hosts_sha256": String(repeating: "d", count: 64)],
        "captured_at": "2026-11-11T00:00:00Z", "received_at_unix_ms": 100,
        "snapshot_digest": String(repeating: "e", count: 64), "collector_sha256": String(repeating: "f", count: 64),
        "pve_version": "pve-manager/9.2", "pool_name": "rpool", "pool_health": "ONLINE",
        "root_used_bytes": 4096, "root_available_bytes": 8192, "vms": [], "datasets": [],
        "container_count": 0, "warnings": [], "managed": managed, "authorizes_management": false,
        "creates_controller_state": false,
    ])
}

private actor ObservationRecordingRunner: ProcessRunning {
    let output: Data
    private(set) var calls: [ProcessInvocation] = []
    init(output: Data) { self.output = output }
    func run(_ invocation: ProcessInvocation) async throws(ProcessRunFailure) -> ProcessResult {
        calls.append(invocation)
        return ProcessResult(terminationStatus: 0, terminatedBySignal: false, stdout: output, stderr: Data())
    }
}

@Suite("Read-only observation boundary")
struct ObservationTests {
    @Test func observationInvocationCannotSelectAnRpcOrRemoteCommand() async throws {
        let bytes = try observationJSON()
        let identity = try JSONDecoder().decode(ObservationResult.self, from: bytes).identity
        let runner = ObservationRecordingRunner(output: bytes)
        let client = ObservationClient(runner: runner, environment: ["PATH": "/usr/bin:/bin"])
        let configuration = ObservationConfiguration(executableURL: URL(fileURLWithPath: "/tmp/inventory"),
            profilePath: "/tmp/a profile.json", expectedIdentity: identity, limits: ProcessLimits(timeout: 45))
        let result = try await client.observe(configuration: configuration)
        #expect(result.identity == identity)
        let calls = await runner.calls
        #expect(calls.count == 1)
        #expect(calls[0].arguments == ["observe", "--profile", "/tmp/a profile.json", "--expected-binding", identity.binding, "--timeout-seconds", "45"])
        #expect(calls[0].standardInput.isEmpty)
    }

    @Test func untrustedObservationNeverStartsAProcess() async throws {
        let runner = ObservationRecordingRunner(output: Data())
        let client = ObservationClient(runner: runner)
        do {
            _ = try await client.observe(configuration: ObservationConfiguration(executableURL: URL(fileURLWithPath: "/tmp/inventory"),
                profilePath: "/tmp/host.json", expectedIdentity: nil, limits: ProcessLimits()))
            Issue.record("Untrusted host was observed")
        } catch { #expect(error.title == RpcFailure.configuration("").title) }
        let calls = await runner.calls
        #expect(calls.isEmpty)
    }

    @Test func changedHostAndManagementAuthorityAreRejected() async throws {
        let identity = try JSONDecoder().decode(ObservationResult.self, from: observationJSON()).identity
        let configuration = ObservationConfiguration(executableURL: URL(fileURLWithPath: "/tmp/inventory"),
            profilePath: "/tmp/host.json", expectedIdentity: identity, limits: ProcessLimits())
        for output in [try observationJSON(binding: String(repeating: "9", count: 64)), try observationJSON(managed: true)] {
            do {
                _ = try await ObservationClient(runner: ObservationRecordingRunner(output: output)).observe(configuration: configuration)
                Issue.record("Changed host or management authority was accepted")
            } catch {}
        }
    }

    @Test func readOnlySettingsCannotProduceControllerConfiguration() throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: directory) }
        let profile = directory.appendingPathComponent("host.json")
        try Data("{}".utf8).write(to: profile)
        var settings = AppSettings(executablePath: "/usr/bin/true")
        settings.inventoryExecutablePath = "/usr/bin/true"
        let identity = try JSONDecoder().decode(ObservationResult.self, from: observationJSON()).identity
        let server = ServerProfile(name: "Physical host", profilePath: profile.path, mode: .observation, observationIdentity: identity)
        settings.servers = [server]; settings.selectedServerId = server.id
        #expect(settings.validationIssues().isEmpty)
        #expect(throws: RpcFailure.self) { try settings.configuration() }
        #expect(try settings.observationConfiguration().expectedIdentity == identity)
        let decoded = try JSONDecoder().decode(AppSettings.self, from: JSONEncoder().encode(settings))
        #expect(decoded.selectedServer?.isReadOnly == true)
        #expect(decoded.selectedServer?.observationIdentity == identity)
        #expect(!decoded.pollingEnabled)
    }
}
