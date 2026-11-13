import Foundation
import Testing

@testable import PveDevCore

@Suite("Settings, drafts, permissions, and display safety")
struct ValidationTests {
    @Test func settingsRequireAbsoluteExecutableAndJsonProfile() throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let profile = directory.appendingPathComponent("profile.json")
        try Data("{}".utf8).write(to: profile)
        let database = directory.appendingPathComponent("state.redb")
        try Data().write(to: database)

        #expect(AppSettings().validationIssues().count == 2)
        #expect(!AppSettings(executablePath: "bin/pve-devctl", profilePath: profile.path).validationIssues().isEmpty)
        #expect(!AppSettings(executablePath: "/bin/cat", profilePath: database.path).validationIssues().isEmpty)
        #expect(!AppSettings(executablePath: profile.path, profilePath: profile.path).validationIssues().isEmpty)
        #expect(!AppSettings(executablePath: "/bin", profilePath: profile.path).validationIssues().isEmpty)

        let valid = AppSettings(executablePath: "/bin/cat", profilePath: profile.path, timeoutSeconds: 1)
        #expect(valid.validationIssues().isEmpty)
        let configuration = try valid.configuration()
        #expect(configuration.arguments == ["--profile", profile.standardizedFileURL.path, "rpc"])
        #expect(configuration.limits.timeout == 5)
    }

    @Test func createVmDraftValidates() throws {
        var draft = CreateVmDraft()
        draft.name = "dev-frank"
        let id = UUID()
        let built = try draft.build(requestId: id, allowOwner: false).get()
        #expect(
            built
                == .createVm(
                    CreateVmRequest(
                        requestId: id, name: "dev-frank", diskMib: 32, memoryMib: 128, minMemoryMib: 128, cores: 1,
                        owner: nil)))

        draft.owner = "heidi"
        guard case .failure = draft.build(requestId: id, allowOwner: false) else {
            Issue.record("member must not set owner")
            return
        }
        guard case .success(.createVm(let adminRequest)) = draft.build(requestId: id, allowOwner: true) else {
            Issue.record("admin may set owner")
            return
        }
        #expect(adminRequest.owner == "heidi")

        var bad = CreateVmDraft()
        bad.name = "Bad Name"
        bad.diskMib = "-1"
        bad.memoryMib = "128"
        bad.minMemoryMib = "256"
        bad.cores = "70000"
        guard case .failure(let issues) = bad.build(requestId: id, allowOwner: true) else {
            Issue.record("expected issues")
            return
        }
        #expect(issues.messages.count == 4)
    }

    @Test func userAndDeviceDrafts() throws {
        var user = CreateUserDraft()
        user.userId = "frank"
        guard case .success(.createUser(let created)) = user.build(requestId: UUID()) else {
            Issue.record("expected user")
            return
        }
        #expect(created.maxMemoryBudgetMib == 1024)
        user.minMemoryBudgetMib = "4096"
        guard case .failure = user.build(requestId: UUID()) else {
            Issue.record("min above max")
            return
        }

        var enroll = EnrollDeviceDraft()
        enroll.userId = "frank"
        enroll.label = "Frank laptop"
        enroll.endpointId = String(repeating: "AB", count: 32)
        guard case .success(.enrollDevice(let device)) = enroll.build() else {
            Issue.record("expected enrollment")
            return
        }
        #expect(device.endpointId == String(repeating: "ab", count: 32))

        enroll.endpointId = "not-a-key"
        guard case .failure = enroll.build() else {
            Issue.record("expected invalid endpoint")
            return
        }
        var revoke = RevokeDeviceDraft()
        revoke.endpointId = String(repeating: "g", count: 64)
        guard case .failure = revoke.build() else {
            Issue.record("expected invalid endpoint")
            return
        }
    }

    @Test func permissionsFollowRoleAndOwnership() {
        func user(_ id: String, _ role: UserRole) -> User {
            User(
                userId: id, role: role, budgetMib: 1, minMemoryBudgetMib: 1, maxMemoryBudgetMib: 1, maxCores: 1,
                maxVms: 1, ready: true)
        }
        func vm(_ owner: String, _ state: VmState) -> Vm {
            Vm(vmId: 1, owner: owner, name: "v", diskMib: 1, memoryMib: 1, minMemoryMib: 1, cores: 1, state: state)
        }
        let member = Permissions(identity: user("frank", .member))
        let admin = Permissions(identity: user("erin", .admin))
        let unknown = Permissions(identity: user("eve", UserRole(rawValue: "superuser")))
        let disconnected = Permissions(identity: nil)

        #expect(!member.isAdmin && !member.canManageUsers && !member.canChooseVmOwner)
        #expect(admin.canManageUsers && admin.canManageDevices && admin.canChooseVmOwner)
        #expect(!unknown.isAdmin)
        #expect(!disconnected.canCreateVm)

        #expect(member.availableActions(for: vm("frank", .stopped)) == [.start, .delete])
        #expect(member.availableActions(for: vm("frank", .running)) == [.shutdown, .stop, .delete])
        #expect(member.availableActions(for: vm("heidi", .running)).isEmpty)
        #expect(admin.availableActions(for: vm("heidi", .running)) == [.shutdown, .stop, .delete])
        #expect(admin.availableActions(for: vm("heidi", .creating)).isEmpty)
        #expect(admin.availableActions(for: vm("heidi", .deleted)).isEmpty)
        #expect(admin.availableActions(for: vm("heidi", VmState(rawValue: "mystery"))).isEmpty)
    }

    @Test func safeTextStripsControlsRedactsAndTruncates() {
        let key = String(repeating: "f", count: 64)
        let text = SafeText.sanitize("bad\u{07}\u{1B}[0m key \(key)\nnext", limit: 200)
        #expect(text == "bad[0m key \(SafeText.redactionMarker)\nnext")
        #expect(SafeText.sanitize(String(repeating: "x ", count: 100), limit: 10).count == 11)
        #expect(SafeText.sanitize("request 6f9619ff-8b86-d011-b42d-00c04fc964ff", limit: 100).contains("6f9619ff"))
        let tail = SafeText.tail(Data((String(repeating: "a ", count: 2000) + "END").utf8), limit: 20)
        #expect(tail.hasPrefix("…"))
        #expect(tail.hasSuffix("END"))
    }

    @Test func formatting() {
        #expect(Formatting.mib(128) == "128 MiB")
        #expect(Formatting.mib(1024) == "1 GiB")
        #expect(Formatting.mib(1536) == "1.5 GiB")
        #expect(Formatting.fraction(5, of: 0) == 1)
        #expect(Formatting.fraction(0, of: 0) == 0)
        #expect(Formatting.fraction(200, of: 100) == 1)
    }
}
