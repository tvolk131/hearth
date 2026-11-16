import HearthCore
import SwiftUI

/// The per-server home: who you are there, what kind of controller it is, your own
/// budget, operation health and data freshness. Hardware inventory is admin-only
/// and is never requested for members.
struct ServersView: View {
  @Environment(AppModel.self) private var model

  var body: some View {
    ScrollView {
      VStack(alignment: .leading, spacing: 20) {
        if model.fleetServers.isEmpty {
          ContentUnavailableView(
            "No servers", systemImage: "server.rack",
            description: Text("Add a controller profile in Settings."))
        }
        ForEach(model.fleetServers) { server in
          serverCard(server)
        }
      }.padding(20).frame(maxWidth: .infinity, alignment: .leading)
    }
    .navigationTitle("Servers")
    .task { model.refreshServers(onlyStale: true) }
  }

  private func serverCard(_ server: ServerProfile) -> some View {
    let dashboard = model.dashboard(forServer: server.id)
    let physical = dashboard?.backend == .physicalTrial
    return SectionCard(title: server.name, systemImage: "server.rack") {
      VStack(alignment: .leading, spacing: 14) {
        header(server, dashboard: dashboard)
        if let dashboard {
          if physical {
            Label(
              "Physical trial: actions run on real hardware, limited to this controller's disposable-resource scope.",
              systemImage: "exclamationmark.triangle.fill"
            )
            .font(.callout).foregroundStyle(.orange)
            .fixedSize(horizontal: false, vertical: true)
          }
          HStack(alignment: .top, spacing: 24) {
            budgetSummary(dashboard)
            operationSummary(dashboard, server: server)
          }
        } else if model.refreshing(server.id) {
          HStack {
            ProgressView().controlSize(.small)
            Text("Loading dashboard…")
          }
        } else if model.failure(forServer: server.id) == nil {
          Text("Not loaded yet. Refresh this server to read its dashboard; nothing is changed.")
            .foregroundStyle(.secondary)
        }
        if let failure = model.failure(forServer: server.id) {
          FailureBox(title: failure.title, detail: failure.detail)
        }
        if let dashboard, Permissions(identity: dashboard.identity).isAdmin {
          hardware(server)
        }
        HStack(spacing: 12) {
          Button("Refresh Server") { model.refreshServer(server.id) }
            .disabled(model.refreshing(server.id))
          if model.refreshing(server.id) && dashboard != nil {
            ProgressView().controlSize(.small)
          }
        }
      }
    }
    .overlay {
      if physical {
        RoundedRectangle(cornerRadius: 8).strokeBorder(.orange.opacity(0.7), lineWidth: 1.5)
          .allowsHitTesting(false)
      }
    }
  }

  @ViewBuilder
  private func header(_ server: ServerProfile, dashboard: DashboardResult?) -> some View {
    VStack(alignment: .leading, spacing: 6) {
      HStack(spacing: 8) {
        if let dashboard {
          Image(systemName: "person.crop.circle")
          Text(dashboard.identity.userId).font(.headline).textSelection(.enabled)
          RoleBadge(role: dashboard.identity.role)
          BackendBadge(backend: dashboard.backend)
        } else {
          Text("Identity and environment appear after the dashboard loads.")
            .foregroundStyle(.secondary)
        }
        Spacer(minLength: 0)
      }
      HStack(spacing: 8) {
        if let dashboard {
          Text("Environment \(SafeText.sanitize(dashboard.environment, limit: 64))")
            .textSelection(.enabled)
          Text("·")
        }
        Text(model.freshness(server.id))
        Text("·")
        Text(model.sessions[server.id]?.pollingEnabled == true ? "Auto-refresh on" : "Auto-refresh off")
        AutoRefreshPausedBanner(serverId: server.id)
      }
      .font(.caption).foregroundStyle(.secondary)
    }
  }

  private func budgetSummary(_ dashboard: DashboardResult) -> some View {
    let budget = dashboard.budget
    let identity = dashboard.identity
    let vmCount = OwnedUsage(vms: dashboard.vms, owner: identity.userId).activeVmCount
    return VStack(alignment: .leading, spacing: 6) {
      Text("Your budget").font(.subheadline.weight(.semibold))
      if budget.budgetMib > 0 {
        ProgressView(
          value: Double(min(budget.allocatedMib, budget.budgetMib)), total: Double(budget.budgetMib)
        )
        .frame(maxWidth: 260)
      }
      Text(
        "Disk: \(Formatting.mib(budget.allocatedMib)) used · \(Formatting.mib(budget.remainingMib)) remaining of \(Formatting.mib(budget.budgetMib))"
      )
      Text(
        "Memory: \(Formatting.mib(budget.memoryAllocatedMib)) of \(Formatting.mib(identity.maxMemoryBudgetMib)) maximum · \(Formatting.mib(budget.minMemoryAllocatedMib)) of \(Formatting.mib(identity.minMemoryBudgetMib)) minimum"
      )
      .foregroundStyle(.secondary)
      Text(
        "Cores: \(budget.coresAllocated) of \(identity.maxCores) · VMs: \(vmCount) of \(identity.maxVms)"
      )
      .foregroundStyle(.secondary)
    }
    .font(.callout).monospacedDigit()
  }

  private func operationSummary(_ dashboard: DashboardResult, server: ServerProfile) -> some View {
    let active = dashboard.operations.filter { $0.status.isActive }.count
    let investigate = dashboard.operations.filter { $0.status == .blocked }.count
    return VStack(alignment: .leading, spacing: 6) {
      Text("Operations").font(.subheadline.weight(.semibold))
      Label("\(active) active", systemImage: "arrow.triangle.2.circlepath")
      Label {
        Text("\(investigate) \(DisplayVocabulary.needsInvestigation.lowercased())")
      } icon: {
        Image(systemName: "exclamationmark.octagon.fill")
          .foregroundStyle(investigate > 0 ? .red : .secondary)
      }
      Text(
        "\(dashboard.vms.filter { $0.state != .deleted }.count) VMs visible on \(server.name)"
      )
      .foregroundStyle(.secondary)
    }
    .font(.callout).monospacedDigit()
  }

  @ViewBuilder
  private func hardware(_ server: ServerProfile) -> some View {
    VStack(alignment: .leading, spacing: 14) {
      Divider()
      Label("Hardware and storage · administrators only", systemImage: "cpu")
        .font(.subheadline.weight(.semibold))
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
        } else if model.sessions[server.id]?.isInventoryRefreshing == true {
          HStack {
            ProgressView().controlSize(.small)
            Text("Loading hardware and storage…")
          }
        } else if model.inventoryFailure(forServer: server.id) == nil {
          Text("Hardware and storage has not loaded. Refresh this server.").foregroundStyle(
            .secondary)
        }
        if let failure = model.inventoryFailure(forServer: server.id) {
          FailureBox(title: failure.title, detail: failure.detail)
        }
      }
    }
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
