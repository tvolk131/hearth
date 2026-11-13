import PveDevCore
import SwiftUI

struct MigrationTransfersView: View {
    @Environment(AppModel.self) private var model
    @State private var records: [MigrationTransferRecord] = []
    @State private var failure: RpcFailure?
    @State private var isLoading = false
    @State private var review: MigrationSheetTarget?

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Text("VM transfers").font(.title2.weight(.semibold))
                Spacer()
                Button("Reload Transfers") { Task { await load() } }.disabled(isLoading)
            }
            if let failure { Text(failure.detail).foregroundStyle(.red).textSelection(.enabled) }
            if isLoading && records.isEmpty { ProgressView("Loading transfers…") }
            else if records.isEmpty && failure == nil {
                ContentUnavailableView("No transfers", systemImage: "arrow.left.arrow.right")
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                ScrollView {
                    VStack(alignment: .leading, spacing: 14) {
                        ForEach(records.sorted { $0.createdAt > $1.createdAt }) { record in
                            VStack(alignment: .leading, spacing: 6) {
                                Text("\(record.plan.name) · VM \(record.plan.sourceVmId) → VM \(record.plan.targetVmId)")
                                    .font(.headline)
                                Text("\(serverName(record.plan.source)) → \(serverName(record.plan.destination))")
                                    .foregroundStyle(.secondary)
                                MigrationProgress(record: record)
                                if record.role == "source" {
                                    Button("Review Transfer…") {
                                        let p = record.plan
                                        let vm = Vm(vmId: p.sourceVmId, owner: p.sourceOwner, name: p.name,
                                            diskMib: p.diskMib, memoryMib: p.memoryMib, minMemoryMib: p.minMemoryMib,
                                            cores: p.cores, state: .stopped, guest: p.guest)
                                        review = MigrationSheetTarget(vm: vm, origin: model.requestOrigin, transfer: record)
                                    }
                                }
                            }.frame(maxWidth: .infinity, alignment: .leading)
                            Divider()
                        }
                    }
                }
            }
        }
        .padding(20)
        .navigationTitle("Transfers")
        .task { await load() }
        .sheet(item: $review) { MigrationSheet(target: $0) }
    }

    private func serverName(_ peer: MigrationPeer) -> String {
        model.settings.servers.first { $0.identity.map { peer.matches($0) } == true }?.name
            ?? EndpointIdentifier.abbreviated(peer.serverId)
    }

    @MainActor
    private func load() async {
        guard !isLoading else { return }
        isLoading = true; failure = nil
        defer { isLoading = false }
        do throws(RpcFailure) {
            records = try await model.query(.migration(.list), origin: model.requestOrigin, as: [MigrationTransferRecord].self)
        } catch { failure = error }
    }
}
