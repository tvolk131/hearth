import Foundation
import Testing

@testable import PveDevCore

@Suite("Real Rust CLI integration", .timeLimit(.minutes(2)))
struct RustCLIIntegrationTests {
    @Test(.enabled(if: ProcessInfo.processInfo.environment["PVE_DEV_TEST_PROFILE"] != nil))
    func realDashboardAndIdempotentMutation() async throws {
        let environment = ProcessInfo.processInfo.environment
        let executable = try #require(environment["PVE_DEV_TEST_CLI"])
        let profile = try #require(environment["PVE_DEV_TEST_PROFILE"])
        var configuration = RpcConfiguration(
            executableURL: URL(fileURLWithPath: executable), profilePath: profile,
            limits: ProcessLimits(timeout: 35))
        let client = RpcClient()
        let identity = try await client.profileIdentity(configuration: configuration)
        #expect(identity.isValid)
        configuration.expectedIdentity = identity
        var wrongIdentity = configuration
        wrongIdentity.expectedIdentity = ServerIdentity(
            serverId: String(repeating: identity.serverId == String(repeating: "a", count: 64) ? "b" : "a", count: 64),
            environment: identity.environment, deviceId: identity.deviceId)
        do {
            _ = try await client.dashboard(configuration: wrongIdentity)
            Issue.record("A saved identity mismatch must fail before connecting")
        } catch {
            guard case .processFailed = error else { Issue.record("Expected local identity rejection"); return }
        }
        let before = try await client.dashboard(configuration: configuration)
        #expect(before.identity.userId == "erin")
        #expect(before.identity.ready)
        let request = RpcRequest.createVm(CreateVmRequest(
            requestId: UUID(), name: "swift-integration", diskMib: 16,
            memoryMib: 128, minMemoryMib: 128, cores: 1, owner: nil))
        let first = try await client.submit(request, configuration: configuration)
        let repeated = try await client.submit(request, configuration: configuration)
        guard case .operation(let firstOperation) = first,
              case .operation(let repeatedOperation) = repeated else {
            Issue.record("Expected operation acknowledgements")
            return
        }
        #expect(firstOperation.operationId == repeatedOperation.operationId)
        #expect(firstOperation.vmId == repeatedOperation.vmId)
        var observed = false
        for _ in 0..<20 {
            let dashboard = try await client.dashboard(configuration: configuration)
            if dashboard.vms.contains(where: { $0.name == "swift-integration" && $0.state == .stopped }) {
                observed = true
                break
            }
            try await Task.sleep(for: .milliseconds(500))
        }
        #expect(observed)
    }
}
