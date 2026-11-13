import PveDevCore
import SwiftUI

struct ServersView: View {
  @Environment(AppModel.self) private var model

  var body: some View {
    ScrollView {
      VStack(alignment: .leading, spacing: 20) {
        ForEach(model.settings.servers.filter { !$0.isReadOnly }) { server in
          SectionCard(title: server.name, systemImage: "server.rack") {
            VStack(alignment: .leading, spacing: 14) {
              if let inventory = model.inventory(forServer: server.id) {
                specifications(inventory)
                DisclosureGroup("Physical disks and ZFS pool") {
                  VStack(alignment: .leading, spacing: 8) {
                    ForEach(inventory.blockDevices.filter { $0.type == "disk" }) { disk in
                      DiskLayoutRow(device: disk)
                    }
                    Text(inventory.zpoolTopology).font(.caption.monospaced()).textSelection(
                      .enabled)
                  }.frame(maxWidth: .infinity, alignment: .leading).padding(.top, 8)
                }
                DisclosureGroup("User allocations and VM usage") {
                  VStack(alignment: .leading, spacing: 12) {
                    ForEach(inventory.storageAllocations) { allocation in
                      allocationView(allocation, inventory: inventory, server: server)
                    }
                  }.padding(.top, 8)
                }
                DisclosureGroup("All ZFS datasets and volumes") {
                  VStack(alignment: .leading, spacing: 5) {
                    ForEach(inventory.datasets, id: \.self) { dataset in
                      HStack(alignment: .firstTextBaseline) {
                        Text(dataset["name"] ?? "").font(.caption.monospaced())
                          .padding(
                            .leading,
                            CGFloat((dataset["name"] ?? "").filter { $0 == "/" }.count) * 10)
                        Spacer(minLength: 12)
                        Text(
                          "Charged \(bytes(dataset["used"])) · Available \(bytes(dataset["available"]))"
                        )
                        .font(.caption).foregroundStyle(.secondary)
                      }
                    }
                  }.padding(.top, 8)
                }
                Text("Captured \(Formatting.captureDate(inventory.capturedAt))").font(.caption)
                  .foregroundStyle(.secondary)
              } else {
                if model.refreshing(server.id) {
                  HStack {
                    ProgressView().controlSize(.small)
                    Text("Loading hardware and storage…")
                  }
                } else {
                  Text("Hardware and storage has not loaded. Refresh this server.").foregroundStyle(
                    .secondary)
                }
              }
              if let failure = model.inventoryFailure(forServer: server.id) {
                FailureBox(title: failure.title, detail: failure.detail)
              }
              if model.inventory(forServer: server.id) != nil && model.refreshing(server.id) {
                HStack {
                  ProgressView().controlSize(.small)
                  Text("Refreshing server…")
                }.font(.caption)
              }
              Button("Refresh Server") { model.refreshServer(server.id) }
                .disabled(model.refreshing(server.id))
            }
          }
        }
      }.padding(20).frame(maxWidth: .infinity, alignment: .leading)
    }
    .navigationTitle("Servers")
    .task { model.refreshServers(onlyStale: true) }
  }

  private func specifications(_ inventory: HostInventory) -> some View {
    VStack(alignment: .leading, spacing: 6) {
      Text(inventory.cpuModel).font(.headline)
      Text(
        "\(inventory.cpuSockets) sockets · \(inventory.logicalCpus) logical CPUs · \(bytes(String(inventory.memoryTotalBytes))) RAM"
      )
      Text("RAM in use: \(bytes(String(inventory.memoryUsedBytes))) · \(inventory.pveVersion)")
        .font(.callout).foregroundStyle(.secondary)
      Text(
        "ZFS pool: \(bytes(String(inventory.poolSizeBytes))) usable · \(bytes(String(inventory.poolAllocatedBytes))) allocated · \(bytes(String(inventory.poolFreeBytes))) physical free"
      )
      .font(.callout)
      Text(
        "Unreserved space: \(bytes(inventory.datasets.first(where: { $0["name"] == "rpool" })?["available"]))"
      )
      .font(.callout).foregroundStyle(.secondary)
    }
  }

  private func allocationView(
    _ allocation: StorageAllocation, inventory: HostInventory, server: ServerProfile
  ) -> some View {
    let dashboard = model.dashboard(forServer: server.id)
    let vms =
      dashboard?.vms.filter { allocation.vmIds.contains($0.vmId) && $0.state != .deleted } ?? []
    let datasets = inventory.datasets.filter {
      $0["name"] == allocation.dataset || ($0["name"] ?? "").hasPrefix(allocation.dataset + "/")
    }
    let root = datasets.first { $0["name"] == allocation.dataset }
    let written = datasets.reduce(UInt64(0)) { $0 + (UInt64($1["usedbydataset"] ?? "") ?? 0) }
    let snapshots = datasets.reduce(UInt64(0)) { $0 + (UInt64($1["usedbysnapshots"] ?? "") ?? 0) }
    return VStack(alignment: .leading, spacing: 6) {
      HStack {
        Label(allocation.owner, systemImage: "person")
        Spacer()
        Text("\(Formatting.mib(allocation.budgetMib)) budget").fontWeight(.medium)
      }
      Text(allocation.dataset).font(.caption.monospaced()).textSelection(.enabled)
      Text(
        "Reserved \(bytes(root?["reservation"])) · ZFS charged \(bytes(root?["used"])) · Available \(bytes(root?["available"]))"
      )
      .font(.caption).foregroundStyle(.secondary)
      Text("Written data \(bytes(String(written))) · Snapshots \(bytes(String(snapshots)))")
        .font(.caption).foregroundStyle(.secondary)
      ForEach(vms) { vm in
        HStack {
          Text("\(vm.name) · VM \(vm.vmId)")
          Spacer()
          Text(
            "\(Formatting.mib(vm.diskMib)) disk · \(Formatting.mib(vm.memoryMib)) RAM · \(vm.cores) vCPU"
          )
          VmStateBadge(state: vm.state)
        }.font(.callout)
      }
      if vms.isEmpty {
        Text("No active VMs in this allocation").font(.caption).foregroundStyle(.secondary)
      }
      Text(
        "ZFS figures include filesystem and storage overhead. Written data is measured at the host, not free space inside the guest."
      )
      .font(.caption).foregroundStyle(.secondary)
    }.padding(12).background(.quaternary.opacity(0.4), in: RoundedRectangle(cornerRadius: 8))
  }

  private func bytes(_ value: String?) -> String {
    guard let value, let count = UInt64(value) else { return "—" }
    return Formatting.bytes(count)
  }
}

private struct DiskLayoutRow: View {
  let device: BlockDevice
  var body: some View {
    VStack(alignment: .leading, spacing: 5) {
      HStack {
        Label(
          device.path, systemImage: device.type == "disk" ? "internaldrive" : "rectangle.split.3x1")
        if device.type == "disk" { Text(device.rota == true ? "HDD" : "SSD") }
        Text(Formatting.bytes(device.size))
        Text(
          [device.model, device.serial, device.fstype].compactMap { $0 }.joined(separator: " · ")
        )
        .foregroundStyle(.secondary)
      }.font(.caption)
      ForEach(device.children ?? []) { child in
        AnyView(DiskLayoutRow(device: child)).padding(.leading, 18)
      }
    }
  }
}
