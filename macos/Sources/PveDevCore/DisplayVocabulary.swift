import Foundation

/// Canonical user-facing wording. Wire values never change; only these display
/// strings do, so every page describes the same state with the same words.
public enum DisplayVocabulary {
    /// Column and field label for the account a VM or operation belongs to.
    public static let owner = "Owner"
    /// User-facing name for a blocked VM or operation.
    public static let needsInvestigation = "Needs investigation"
    /// User-facing name for a successfully finished operation.
    public static let completed = "Completed"

    public static func vmState(_ state: VmState) -> String {
        switch state {
        case .creating: "Creating"
        case .stopped: "Stopped"
        case .running: "Running"
        case .deleting: "Deleting"
        case .deleted: "Deleted"
        case .blocked: needsInvestigation
        default: readable(state.rawValue)
        }
    }

    public static func operationStatus(_ status: OperationStatus) -> String {
        switch status {
        case .queued: "Queued"
        case .running: "Running"
        case .verified: completed
        case .blocked: needsInvestigation
        case .cancelled: "Cancelled"
        default: readable(status.rawValue)
        }
    }

    public static func backend(_ backend: BackendKind) -> String {
        switch backend {
        case .lab: "Lab"
        case .simulated: "Simulated"
        case .physicalTrial: "Physical trial"
        case .managed: "Managed"
        default: readable(backend.rawValue)
        }
    }

    public static func migrationRole(_ role: String) -> String {
        switch role {
        case "source": "Source"
        case "destination": "Destination"
        default: readable(role)
        }
    }

    /// Readable labels for the server-reported transfer phases.
    public static func migrationPhase(_ phase: String) -> String {
        switch phase {
        case "authorized": "Authorized, not started"
        case "preparing": "Preparing transfer"
        case "fenced": "Source locked for transfer"
        case "receiving": "Copying disk data"
        case "ready": "Copy finished, awaiting handoff"
        case "relinquished": "Handed off to destination"
        case "accepted": "Accepted by destination"
        case "cleaning": "Cleaning up"
        case "complete": completed
        case "aborted": "Cancelled"
        case "blocked": needsInvestigation
        default: readable(phase)
        }
    }

    /// For example "Source · Handed off to destination" instead of "Source: relinquished".
    public static func migrationProgress(role: String, phase: String) -> String {
        "\(migrationRole(role)) · \(migrationPhase(phase))"
    }

    /// Fallback for unknown wire values: sanitized, spaced and sentence-cased.
    static func readable(_ raw: String) -> String {
        let spaced = SafeText.sanitize(raw.replacingOccurrences(of: "_", with: " "), limit: 80)
        guard let first = spaced.first else { return "Unknown" }
        return first.uppercased() + spaced.dropFirst()
    }
}
