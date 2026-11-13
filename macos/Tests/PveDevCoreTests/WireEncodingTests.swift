import Foundation
import Testing

@testable import PveDevCore

private let fixedId = UUID(uuidString: "6F9619FF-8B86-D011-B42D-00C04FC964FF")!

private func encoded(_ request: RpcRequest) throws -> String {
    String(decoding: try WireCoding.encodeRequest(request), as: UTF8.self)
}

private func object(_ request: RpcRequest) throws -> NSDictionary {
    try JSONSerialization.jsonObject(with: WireCoding.encodeRequest(request)) as! NSDictionary
}

@Suite("Request serialization")
struct WireEncodingTests {
    @Test func dashboardIsBareAction() throws {
        #expect(try encoded(.dashboard) == #"{"action":"dashboard"}"#)
    }

    @Test func createVmMatchesContractWithExplicitNullOwner() throws {
        let request = RpcRequest.createVm(
            CreateVmRequest(
                requestId: fixedId, name: "dev-frank", diskMib: 32, memoryMib: 128, minMemoryMib: 128, cores: 1,
                owner: nil))
        #expect(
            try encoded(request)
                == #"{"action":"create_vm","cores":1,"disk_mib":32,"memory_mib":128,"min_memory_mib":128,"name":"dev-frank","owner":null,"request_id":"6f9619ff-8b86-d011-b42d-00c04fc964ff"}"#
        )
    }

    @Test func createVmWithOwner() throws {
        let request = RpcRequest.createVm(
            CreateVmRequest(
                requestId: fixedId, name: "dev-heidi", diskMib: 64, memoryMib: 512, minMemoryMib: 256, cores: 2,
                owner: "heidi"))
        let dictionary = try object(request)
        #expect(dictionary["owner"] as? String == "heidi")
        #expect(dictionary.count == 8)
    }

    @Test(arguments: VmActionKind.allCases)
    func vmActionKinds(kind: VmActionKind) throws {
        let request = RpcRequest.vmAction(VmActionRequest(requestId: fixedId, vmId: 9001, kind: kind))
        #expect(
            try object(request)
                == [
                    "action": "vm_action", "request_id": "6f9619ff-8b86-d011-b42d-00c04fc964ff", "vm_id": 9001,
                    "kind": kind.rawValue,
                ] as NSDictionary)
    }

    @Test func createUserMatchesContract() throws {
        let request = RpcRequest.createUser(
            CreateUserRequest(
                requestId: fixedId, userId: "frank", budgetMib: 128, minMemoryBudgetMib: 256,
                maxMemoryBudgetMib: 1024, maxCores: 8, maxVms: 4))
        #expect(
            try encoded(request)
                == #"{"action":"create_user","budget_mib":128,"max_cores":8,"max_memory_budget_mib":1024,"max_vms":4,"min_memory_budget_mib":256,"request_id":"6f9619ff-8b86-d011-b42d-00c04fc964ff","user_id":"frank"}"#
        )
    }

    @Test func enrollAndRevokeCarryNoRequestId() throws {
        let endpoint = String(repeating: "ab", count: 32)
        let enroll = RpcRequest.enrollDevice(
            EnrollDeviceRequest(userId: "frank", endpointId: endpoint, label: "Frank laptop"))
        #expect(
            try object(enroll)
                == ["action": "enroll_device", "user_id": "frank", "endpoint_id": endpoint, "label": "Frank laptop"]
                as NSDictionary)
        let revoke = RpcRequest.revokeDevice(RevokeDeviceRequest(endpointId: endpoint))
        #expect(try encoded(revoke) == #"{"action":"revoke_device","endpoint_id":"\#(endpoint)"}"#)
        #expect(enroll.requestId == nil)
        #expect(revoke.requestId == nil)
    }

    @Test func requestsRoundTripForPersistence() throws {
        let requests: [RpcRequest] = [
            .dashboard,
            .createVm(
                CreateVmRequest(
                    requestId: fixedId, name: "a", diskMib: 1, memoryMib: 2, minMemoryMib: 2, cores: 1, owner: nil)),
            .createVm(
                CreateVmRequest(
                    requestId: fixedId, name: "a", diskMib: 1, memoryMib: 2, minMemoryMib: 2, cores: 1, owner: "x")),
            .vmAction(VmActionRequest(requestId: fixedId, vmId: 100, kind: .delete)),
            .createUser(
                CreateUserRequest(
                    requestId: fixedId, userId: "u", budgetMib: 1, minMemoryBudgetMib: 1, maxMemoryBudgetMib: 2,
                    maxCores: 1, maxVms: 1)),
            .enrollDevice(EnrollDeviceRequest(userId: "u", endpointId: "e", label: "l")),
            .revokeDevice(RevokeDeviceRequest(endpointId: "e")),
        ]
        for request in requests {
            let decoded = try JSONDecoder().decode(RpcRequest.self, from: WireCoding.encodeRequest(request))
            #expect(decoded == request)
        }
    }

    @Test func intentComparisonIgnoresRequestId() {
        let first = RpcRequest.vmAction(VmActionRequest(requestId: UUID(), vmId: 7, kind: .stop))
        let second = RpcRequest.vmAction(VmActionRequest(requestId: UUID(), vmId: 7, kind: .stop))
        let other = RpcRequest.vmAction(VmActionRequest(requestId: UUID(), vmId: 7, kind: .start))
        #expect(first != second)
        #expect(first.hasSameIntent(as: second))
        #expect(!first.hasSameIntent(as: other))
    }

    @Test func rpcInvocationUsesArgumentArrayAndJsonStdin() throws {
        let configuration = RpcConfiguration(
            executableURL: URL(fileURLWithPath: "/opt/pve dev/pve-devctl"),
            profilePath: "/Users/me/profile; rm -rf.json",
            limits: ProcessLimits(timeout: 10))
        let client = RpcClient(runner: ChildProcessRunner(), environment: ["PATH": "/usr/bin"])
        let invocation = try client.invocation(for: .dashboard, configuration: configuration)
        #expect(invocation.executableURL.path == "/opt/pve dev/pve-devctl")
        #expect(invocation.arguments == ["--profile", "/Users/me/profile; rm -rf.json", "rpc"])
        #expect(String(decoding: invocation.standardInput, as: UTF8.self) == "{\"action\":\"dashboard\"}\n")
        #expect(invocation.environment == ["PATH": "/usr/bin"])
    }

    @Test func childEnvironmentIsMinimal() {
        let environment = ChildEnvironment.make(from: [
            "HOME": "/Users/me", "AWS_SECRET_ACCESS_KEY": "nope", "PATH": "/evil", "LANG": "en_US.UTF-8",
        ])
        #expect(environment["HOME"] == "/Users/me")
        #expect(environment["LANG"] == "en_US.UTF-8")
        #expect(environment["AWS_SECRET_ACCESS_KEY"] == nil)
        #expect(environment["PATH"] == "/usr/bin:/bin:/usr/sbin:/sbin")
    }
}
