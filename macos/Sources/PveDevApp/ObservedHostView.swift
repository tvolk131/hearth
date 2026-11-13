import PveDevCore
import SwiftUI

/// Separate read model: no controller VM, permission, operation or mutation UI.
struct ObservedHostView: View {
  @Environment(AppModel.self) private var model
  let machinesOnly: Bool

  var body: some View {
    Group {
      if let observation = model.observation {
        ScrollView {
          VStack(alignment: .leading, spacing: 16) {
            provenance(observation)
            if !machinesOnly { summary(observation) }
            machines(observation)
            if !machinesOnly { storage(observation) }
          }
          .padding(.horizontal, 20).padding(.bottom, 20)
          .frame(maxWidth: .infinity, alignment: .leading)
        }
      } else if model.isRefreshing {
        ProgressView("Collecting read-only host inventory…")
          .frame(maxWidth: .infinity, maxHeight: .infinity)
      } else {
        VStack(spacing: 16) {
          if let failure = model.refreshFailure {
            FailureBox(title: failure.title, detail: failure.detail).frame(maxWidth: 560)
          }
          ContentUnavailableView {
            Label("No host observation loaded", systemImage: "eye")
          } description: {
            Text(
              "Refresh runs a fixed collector over pinned SSH. It does not change VMs, storage or server configuration."
            )
          } actions: {
            Button("Observe Host") { model.refresh() }.keyboardShortcut(.defaultAction)
          }
        }
        .padding().frame(maxWidth: .infinity, maxHeight: .infinity)
      }
    }
    .navigationTitle(machinesOnly ? "Virtual Machines" : "Host Overview")
  }

  private func provenance(_ observed: ObservationResult) -> some View {
    VStack(alignment: .leading, spacing: 4) {
      HStack {
        Label(SafeText.sanitize(observed.identity.node, limit: 128), systemImage: "server.rack")
          .font(.headline)
        Pill(text: "Read-only", color: .teal, systemImage: "eye")
        Spacer(minLength: 0)
      }
      Text(
        "Observed \(captureDate(observed.capturedAt)) · Refreshed \(model.lastRefreshedAt?.formatted(date: .abbreviated, time: .standard) ?? "—")"
      )
      .font(.caption).foregroundStyle(.secondary)
      Text("Point-in-time snapshot; changes made afterward appear on the next successful refresh.")
        .font(.caption).foregroundStyle(.secondary)
      Text("Snapshot \(observed.snapshotDigest)")
        .font(.caption2.monospaced()).foregroundStyle(.secondary).lineLimit(1).truncationMode(
          .middle
        )
        .textSelection(.enabled).help(observed.snapshotDigest)
    }
  }

  private func summary(_ observed: ObservationResult) -> some View {
    SectionCard(title: "Host and storage", systemImage: "externaldrive") {
      VStack(alignment: .leading, spacing: 8) {
        LabeledContent("Proxmox", value: SafeText.sanitize(observed.pveVersion, limit: 128))
        HStack {
          Text("ZFS pool \(SafeText.sanitize(observed.poolName, limit: 128))")
          Spacer()
          Pill(
            text: SafeText.sanitize(observed.poolHealth, limit: 32),
            color: observed.poolHealth == "ONLINE" ? .green : .orange)
        }
        LabeledContent(
          "VMs",
          value:
            "\(observed.vms.filter { $0.status == "running" }.count) running · \(observed.vms.count) total"
        )
        LabeledContent("Containers observed", value: String(observed.containerCount))
        LabeledContent("ZFS root used", value: bytes(observed.rootUsedBytes))
        LabeledContent("ZFS root available", value: bytes(observed.rootAvailableBytes))
        Text(
          "Available space follows ZFS dataset accounting and existing reservations. Configured VM resources below are allocations, not measured CPU or RAM use."
        )
        .font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
        ForEach(Array(observed.warnings.enumerated()), id: \.offset) { _, warning in
          Label(SafeText.sanitize(warning, limit: 1000), systemImage: "exclamationmark.triangle")
            .font(.caption).foregroundStyle(.orange).fixedSize(horizontal: false, vertical: true)
        }
      }
    }
  }

  private func machines(_ observed: ObservationResult) -> some View {
    SectionCard(title: "Observed virtual machines", systemImage: "server.rack") {
      if observed.vms.isEmpty {
        Text("No virtual machines were present in this snapshot.").foregroundStyle(.secondary)
      } else {
        VStack(spacing: 12) {
          ForEach(observed.vms) { vm in
            VStack(alignment: .leading, spacing: 6) {
              HStack {
                Text(SafeText.sanitize(vm.name, limit: 128)).font(.headline)
                Text("#\(vm.vmid)").font(.caption.monospaced()).foregroundStyle(.secondary)
                Spacer(minLength: 0)
                Pill(
                  text: SafeText.sanitize(vm.status.capitalized, limit: 32),
                  color: vm.status == "running" ? .green : .secondary)
              }
              if let note = vm.note {
                Label(SafeText.sanitize(note, limit: 512), systemImage: "info.circle")
                  .font(.callout).foregroundStyle(.orange).fixedSize(
                    horizontal: false, vertical: true)
              }
              Text(
                "\(vm.cores.map { "\($0) vCPU" } ?? "CPU unknown") · \(memory(vm.memoryMib)) RAM · \(bytes(vm.diskBytes)) disks"
              )
              .font(.callout).foregroundStyle(.secondary).fixedSize(
                horizontal: false, vertical: true)
              if vm.pendingChanges {
                Text("Proxmox has pending configuration changes.").font(.caption).foregroundStyle(
                  .orange)
              }
            }
            if vm.id != observed.vms.last?.id { Divider() }
          }
        }
      }
    }
  }

  private func storage(_ observed: ObservationResult) -> some View {
    SectionCard(title: "Observed ZFS datasets", systemImage: "internaldrive") {
      VStack(alignment: .leading, spacing: 12) {
        ForEach(observed.datasets) { dataset in
          VStack(alignment: .leading, spacing: 3) {
            Text(SafeText.sanitize(dataset.name, limit: 256)).font(.callout.monospaced())
              .textSelection(.enabled)
            Text("Used \(bytes(dataset.usedBytes)) · Available \(bytes(dataset.availableBytes))")
              .font(.caption).foregroundStyle(.secondary)
            Text(
              "Quota \(limit(dataset.quotaBytes)) · Reservation \(bytes(dataset.reservationBytes))"
            )
            .font(.caption).foregroundStyle(.secondary)
          }
        }
      }
    }
  }

  private func bytes(_ value: UInt64?) -> String {
    guard let value else { return "Unknown" }
    return Formatting.bytes(value)
  }
  private func limit(_ value: UInt64?) -> String { value == 0 ? "None" : bytes(value) }
  private func memory(_ value: UInt64?) -> String {
    guard let value, value <= UInt64.max / (1024 * 1024) else { return "Unknown" }
    return bytes(value * 1024 * 1024)
  }
  private func captureDate(_ raw: String) -> String {
    ISO8601DateFormatter().date(from: raw)?.formatted(date: .abbreviated, time: .standard)
      ?? SafeText.sanitize(raw, limit: 64)
  }
}
