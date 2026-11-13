import PveDevCore
import SwiftUI

struct OperationRow: View {
    let operation: OperationRecord

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            Group {
                if operation.status.isActive {
                    ProgressView().controlSize(.small)
                } else if operation.status == .verified {
                    Image(systemName: "checkmark.circle.fill").foregroundStyle(.green)
                } else if operation.status == .cancelled {
                    Image(systemName: "xmark.circle").foregroundStyle(.secondary)
                } else if operation.status == .blocked {
                    Image(systemName: "exclamationmark.octagon.fill").foregroundStyle(.red)
                } else {
                    Image(systemName: "questionmark.circle").foregroundStyle(.purple)
                }
            }
            .frame(width: 18)
            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 6) {
                    Text(OperationPresentation.title(for: operation.kind)).fontWeight(.medium)
                    if let vmId = operation.vmId {
                        Text("VM \(vmId)").foregroundStyle(.secondary)
                    }
                    OperationStatusBadge(status: operation.status)
                    Spacer(minLength: 0)
                    Text(operation.createdDate.relativeDescription)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Text("Step: \(SafeText.sanitize(operation.step, limit: 120))")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                if let progress = operation.progress {
                    OperationProgressView(progress: progress, status: operation.status)
                }
                if let error = operation.error, !error.isEmpty {
                    Text(SafeText.sanitize(error, limit: 400))
                        .font(.callout)
                        .foregroundStyle(.red)
                        .textSelection(.enabled)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
        }
    }
}

struct OperationsView: View {
    @Environment(AppModel.self) private var model
    @State private var inspectionTarget: OperationInspectionTarget?
    @Binding var showVerified: Bool

    var body: some View {
        DashboardGate { dashboard in
            let operations = dashboard.operations
                .filter { showVerified || $0.status != .verified }
                .sorted { $0.createdAt > $1.createdAt }
            if operations.isEmpty {
                ContentUnavailableView("No operations", systemImage: "list.bullet.rectangle")
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                ScrollView {
                    VStack(alignment: .leading, spacing: 0) {
                        ForEach(operations) { operation in
                            VStack(alignment: .leading, spacing: 6) {
                                OperationRow(operation: operation)
                                if operation.status == .blocked && model.permissions.isAdmin && !model.isReadOnly
                                    && dashboard.supports("inspect_operation") {
                                    Button("Inspect and Recover…") {
                                        inspectionTarget = OperationInspectionTarget(operation: operation, origin: model.requestOrigin)
                                    }
                                    .buttonStyle(.bordered)
                                    .controlSize(.small)
                                }
                                HStack(spacing: 12) {
                                    Text("Owner: \(operation.owner)")
                                    Text("Operation: \(SafeText.sanitize(operation.operationId, limit: 64))")
                                    Text("Request: \(SafeText.sanitize(operation.requestId, limit: 64))")
                                }
                                .font(.caption.monospaced())
                                .foregroundStyle(.tertiary)
                                .textSelection(.enabled)
                            }
                            .padding(.vertical, 10)
                            Divider()
                        }
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, 20)
                    .padding(.bottom, 20)
                }
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .navigationTitle("Operations")
        .sheet(item: $inspectionTarget) { target in
            OperationInspectionSheet(target: target)
        }
    }
}

private struct OperationProgressView: View {
    let progress: OperationProgress
    let status: OperationStatus

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            if let fraction = progress.fractionCompleted {
                HStack(spacing: 8) {
                    ProgressView(value: fraction).frame(maxWidth: 180)
                    Text("\(progress.completedSteps) of \(progress.totalSteps) stages verified")
                        .font(.caption).foregroundStyle(.secondary)
                }
                .accessibilityElement(children: .combine)
            }
            if let detail = progress.detail, !detail.isEmpty {
                Text(SafeText.sanitize(detail, limit: 240)).font(.callout).foregroundStyle(.secondary)
            }
            TimelineView(.periodic(from: .now, by: 5)) { context in
                HStack(spacing: 12) {
                    if let seconds = progress.elapsedSeconds(now: context.date) {
                        Text("\(status.isActive ? "Elapsed" : "Duration"): \(OperationPresentation.duration(seconds))")
                    }
                    if progress.updatedAt > 0 && progress.updatedAt <= UInt64(max(0, context.date.timeIntervalSince1970)) {
                        Text("Updated \(Date(timeIntervalSince1970: TimeInterval(progress.updatedAt)).relativeDescription)")
                    }
                }
                .font(.caption).foregroundStyle(.secondary).monospacedDigit()
            }
        }
    }
}
