import PveDevCore
import SwiftUI

/// Separate from the mutable table: no action closures, sheets or context menu.
struct ImportedVirtualMachinesView: View {
    let records: [ImportedVm]
    let serverId: UUID?

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 12) {
                Label("Existing VMs · Imported read-only", systemImage: "lock.shield")
                    .font(.headline)
                Text("Metadata captured on this controller’s host. These records do not consume the new managed namespace budget.")
                    .font(.caption).foregroundStyle(.secondary)
                ForEach(records) { record in
                    VStack(alignment: .leading, spacing: 5) {
                        HStack {
                            Text("\(record.vm.name) · VM \(record.vm.vmId)").fontWeight(.medium)
                            Text("Owner: \(record.vm.owner)")
                            VmStateBadge(state: record.vm.state)
                            Spacer()
                            Text(record.label).font(.caption)
                        }
                        Text("Captured \(Date(timeIntervalSince1970: TimeInterval(record.observedAt)).formatted(date: .abbreviated, time: .standard))")
                            .font(.caption)
                        Text(record.recoveryDescription).font(.callout)
                        Text(record.reason).font(.caption).foregroundStyle(.secondary)
                        Text("Import only · power, delete, disk resizing and moving are unavailable.")
                            .font(.caption).foregroundStyle(.secondary)
                    }
                    .textSelection(.enabled)
                    .padding(10)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(.quaternary.opacity(0.5), in: RoundedRectangle(cornerRadius: 8))
                    .id(serverId.map { record.scopedId(serverId: $0) } ?? "unbound/\(record.vm.vmId)")
                }
            }
            .padding(.horizontal, 16).padding(.bottom, 16)
        }
    }
}
