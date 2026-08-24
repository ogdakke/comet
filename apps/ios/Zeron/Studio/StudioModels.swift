// Minimal iOS projections of the provider-neutral Studio wire types. The
// engine sends more run and model configuration than browsing needs; Swift's
// Decodable intentionally ignores those fields instead of persisting them on
// the phone.

import Foundation

enum StudioMediaKind: String, Codable, Hashable {
    case image
    case video
}

enum StudioRunState: String, Codable, Hashable {
    case draft
    case quoting
    case awaitingConfirmation = "awaiting_confirmation"
    case queued
    case running
    case downloading
    case succeeded
    case failed
    case cancelling
    case cancelled

    var isCreating: Bool {
        switch self {
        case .succeeded, .failed, .cancelled: false
        default: true
        }
    }
}

struct StudioThreadSummary: Codable, Hashable, Identifiable {
    var id: String
    var title: String
    var turnCount: UInt32
    var createdAt: String
    var updatedAt: String
    var archived: Bool
    var forkedFromTurnId: String?
    var creating: Bool = false
    var done: Bool = false

    var updatedDate: Date { studioDate(updatedAt) }
}

struct StudioArtifact: Codable, Hashable, Identifiable {
    var id: String
    var outputPosition: UInt32
    var mediaKind: StudioMediaKind
    var mimeType: String
    var sizeBytes: UInt64
    var width: UInt32?
    var height: UInt32?
    var durationSeconds: Double?
    var createdAt: String
    var thumbhash: String?
    var contentHash: String = ""

    var aspectRatio: CGFloat {
        guard let width, let height, height > 0 else { return 1 }
        return CGFloat(width) / CGFloat(height)
    }
}

struct StudioRunModel: Codable, Hashable {
    var id: String
    var displayName: String
}

struct StudioRun: Codable, Hashable, Identifiable {
    var id: String
    var position: UInt32
    var providerId: String
    var model: StudioRunModel
    var state: StudioRunState
    var progress: Float?
    var error: String?
    var artifacts: [StudioArtifact]
}

struct StudioTurn: Codable, Hashable, Identifiable {
    var id: String
    var position: UInt32
    var prompt: String
    var sourceTurnId: String?
    var batchId: String
    var runs: [StudioRun]
    var createdAt: String

    var createdDate: Date { studioDate(createdAt) }
}

struct StudioThread: Codable, Hashable {
    var conversation: StudioThreadSummary
    var turns: [StudioTurn]
}

struct StudioGalleryItem: Codable, Hashable, Identifiable {
    var id: String
    var conversationId: String
    var turnId: String
    var outputPosition: UInt32
    var mediaKind: StudioMediaKind
    var mimeType: String
    var sizeBytes: UInt64
    var width: UInt32?
    var height: UInt32?
    var prompt: String
    var modelDisplayName: String
    var createdAt: String
    var thumbhash: String?
    var sourceArtifactId: String?
    var durationSeconds: Double?

    var createdDate: Date { studioDate(createdAt) }
    var aspectRatio: CGFloat {
        guard let width, let height, height > 0 else { return 1 }
        return CGFloat(width) / CGFloat(height)
    }
}

struct StudioThreadListResponse: Codable {
    var conversations: [StudioThreadSummary]
}

struct StudioGalleryResponse: Codable {
    var artifacts: [StudioGalleryItem]
}

struct StudioArtifactChunk: Codable {
    var artifactId: String
    var fileName: String
    var mimeType: String
    var data: String
    var nextOffset: UInt64
    var done: Bool
}

private func studioDate(_ value: String) -> Date {
    let fractional = ISO8601DateFormatter()
    fractional.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
    if let date = fractional.date(from: value) { return date }
    return ISO8601DateFormatter().date(from: value) ?? .distantPast
}
