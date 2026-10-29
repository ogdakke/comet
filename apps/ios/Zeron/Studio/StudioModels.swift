// Minimal iOS projections of the provider-neutral Studio wire types. The
// engine sends more run and model configuration than browsing needs; Swift's
// Decodable intentionally ignores those fields instead of persisting them on
// the phone.

import Foundation
import Observation
import UIKit

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

    /// Reported dimensions, else the thumbhash's recorded shape (some
    /// providers report none), else square.
    var aspectRatio: CGFloat {
        studioAspectRatio(width: width, height: height, thumbhash: thumbhash)
    }
}

struct StudioRunModel: Codable, Hashable {
    var id: String
    var displayName: String
    /// What it makes: a creating run's tiles say image or video.
    var outputKind: StudioMediaKind?

    var isVideo: Bool { outputKind == .video }

    private enum CodingKeys: String, CodingKey {
        case id
        case displayName
        case legacyDisplayName = "display_name"
        case outputKind = "output_kind"
        case camelOutputKind = "outputKind"
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        id = try container.decode(String.self, forKey: .id)
        if let value = try container.decodeIfPresent(String.self, forKey: .displayName) {
            displayName = value
        } else {
            displayName = try container.decode(String.self, forKey: .legacyDisplayName)
        }
        // Lenient: an unknown kind mustn't fail the whole thread's decode.
        outputKind = (try? container.decodeIfPresent(StudioMediaKind.self, forKey: .outputKind))
            ?? (try? container.decodeIfPresent(StudioMediaKind.self, forKey: .camelOutputKind))
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        try container.encode(id, forKey: .id)
        try container.encode(displayName, forKey: .displayName)
        try container.encodeIfPresent(outputKind, forKey: .outputKind)
    }
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
    /// How many it was asked for: a creating run shows that many tiles.
    var outputCount: UInt32?
    /// `[width, height]` the outputs are shaped to (the requested aspect).
    var displayAspectRatio: [UInt32]?

    var displayAspect: CGFloat? {
        guard let ratio = displayAspectRatio, ratio.count == 2, ratio[0] > 0, ratio[1] > 0 else { return nil }
        return CGFloat(ratio[0]) / CGFloat(ratio[1])
    }
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
    /// Reported dimensions, else the thumbhash's recorded shape (some
    /// providers report none), else square.
    var aspectRatio: CGFloat {
        studioAspectRatio(width: width, height: height, thumbhash: thumbhash)
    }
}

struct StudioArtifactDetail: Hashable, Identifiable {
    var id: String
    var conversationId: String
    var mediaKind: StudioMediaKind
    var mimeType: String
    var width: UInt32?
    var height: UInt32?
    var durationSeconds: Double?
    var sizeBytes: UInt64
    var prompt: String
    var modelDisplayName: String
    var createdAt: String
    var thumbhash: String?

    init(item: StudioGalleryItem) {
        id = item.id
        conversationId = item.conversationId
        mediaKind = item.mediaKind
        mimeType = item.mimeType
        width = item.width
        height = item.height
        durationSeconds = item.durationSeconds
        sizeBytes = item.sizeBytes
        prompt = item.prompt
        modelDisplayName = item.modelDisplayName
        createdAt = item.createdAt
        thumbhash = item.thumbhash
    }

    init(artifact: StudioArtifact, turn: StudioTurn, run: StudioRun, conversationId: String) {
        id = artifact.id
        self.conversationId = conversationId
        mediaKind = artifact.mediaKind
        mimeType = artifact.mimeType
        width = artifact.width
        height = artifact.height
        durationSeconds = artifact.durationSeconds
        sizeBytes = artifact.sizeBytes
        prompt = turn.prompt
        modelDisplayName = run.model.displayName
        createdAt = artifact.createdAt
        thumbhash = artifact.thumbhash
    }

    var createdDate: Date { studioDate(createdAt) }
    /// Reported dimensions, else the thumbhash's recorded shape (some
    /// providers report none), else square.
    var aspectRatio: CGFloat {
        studioAspectRatio(width: width, height: height, thumbhash: thumbhash)
    }
}

@MainActor
@Observable
final class StudioViewerSession: Identifiable {
    let id = UUID()
    var artifacts: [StudioArtifactDetail]
    let openedFromGallery: Bool
    var selectedId: String
    var presentationActive = false
    let openingPreview: UIImage?
    let openingPreviewArtifactId: String
    private(set) var artifactRevision = 0
    @ObservationIgnored private var artifactIndex: [String: Int]

    init(
        artifacts: [StudioArtifactDetail],
        selectedId: String,
        openedFromGallery: Bool,
        openingPreview: UIImage? = nil
    ) {
        self.artifacts = artifacts
        self.selectedId = selectedId
        self.openedFromGallery = openedFromGallery
        self.openingPreview = openingPreview
        openingPreviewArtifactId = selectedId
        artifactIndex = Dictionary(uniqueKeysWithValues: artifacts.indices.map { (artifacts[$0].id, $0) })
    }

    var selected: StudioArtifactDetail? {
        artifactIndex[selectedId].map { artifacts[$0] }
    }

    func openingPreview(for artifactId: String) -> UIImage? {
        artifactId == openingPreviewArtifactId ? openingPreview : nil
    }

    func append(_ additions: [StudioArtifactDetail]) {
        let existing = Set(artifacts.map(\.id))
        let additions = additions.filter { !existing.contains($0.id) }
        guard !additions.isEmpty else { return }
        artifacts.append(contentsOf: additions)
        artifactRevision &+= 1
        rebuildArtifactIndex()
    }

    func replaceArtifacts(with orderedArtifacts: [StudioArtifactDetail]) {
        var seen = Set<String>()
        let next = orderedArtifacts.filter { seen.insert($0.id).inserted }
        guard next != artifacts else { return }
        artifacts = next
        artifactRevision &+= 1
        rebuildArtifactIndex()
    }

    private func rebuildArtifactIndex() {
        artifactIndex = Dictionary(uniqueKeysWithValues: artifacts.indices.map { (artifacts[$0].id, $0) })
    }
}

struct StudioThreadListResponse: Codable {
    var conversations: [StudioThreadSummary]
}

struct StudioGalleryCursor: Codable, Hashable {
    var createdAt: String
    var artifactId: String
}

struct StudioGalleryResponse: Codable {
    var artifacts: [StudioGalleryItem]
    var nextCursor: StudioGalleryCursor?
}

struct StudioArtifactChunk: Codable {
    var artifactId: String
    var fileName: String
    var mimeType: String
    var data: String
    var nextOffset: UInt64
    var done: Bool
}

struct StudioArtifactBytes: Sendable {
    var fileName: String
    var mimeType: String
    var data: Data
    var nextOffset: UInt64
    var done: Bool
}

/// Made once: building an `ISO8601DateFormatter` loads ICU tables (about a
/// millisecond), and sorts call this per comparison. The formatter is
/// thread-safe for parsing.
private nonisolated(unsafe) let studioFractionalDates: ISO8601DateFormatter = {
    let formatter = ISO8601DateFormatter()
    formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
    return formatter
}()
private nonisolated(unsafe) let studioWholeSecondDates = ISO8601DateFormatter()

func studioDate(_ value: String) -> Date {
    studioFractionalDates.date(from: value) ?? studioWholeSecondDates.date(from: value) ?? .distantPast
}

func studioAspectRatio(width: UInt32?, height: UInt32?, thumbhash: String?) -> CGFloat {
    if let width, let height, height > 0 { return CGFloat(width) / CGFloat(height) }
    return StudioThumbHash.aspectRatio(base64: thumbhash) ?? 1
}
