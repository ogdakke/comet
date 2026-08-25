import Foundation

enum StudioComposerMode: String, Codable, Hashable {
    case image
    case video
}

enum StudioMediaOperation: String, Codable, Hashable {
    case textToImage = "text_to_image"
    case imageToImage = "image_to_image"
    case imageEdit = "image_edit"
    case upscale
    case textToVideo = "text_to_video"
    case imageToVideo = "image_to_video"
    case referenceToVideo = "reference_to_video"
    case videoToVideo = "video_to_video"

    var acceptsReferences: Bool {
        switch self {
        case .imageToImage, .imageToVideo, .referenceToVideo, .videoToVideo: true
        default: false
        }
    }

    var label: String {
        switch self {
        case .textToImage: "Text to image"
        case .imageToImage: "Image to image"
        case .imageEdit: "Image edit"
        case .upscale: "Upscale"
        case .textToVideo: "Text to video"
        case .imageToVideo: "Image to video"
        case .referenceToVideo: "Reference to video"
        case .videoToVideo: "Video to video"
        }
    }
}

enum StudioControlKind: String, Codable, Hashable {
    case `enum`
    case integer
    case number
    case boolean
    case dimensions
    case aspectRatio = "aspect_ratio"
    case resolution
    case duration
}

enum StudioControlValue: Hashable, Codable {
    case enumValue(String)
    case integer(Int64)
    case number(Double)
    case boolean(Bool)
    case dimensions(width: UInt32, height: UInt32)
    case aspectRatio(width: UInt32, height: UInt32)
    case aspectRatioAuto
    case aspectRatioAdaptive
    case resolution(String)
    case durationSeconds(Double)
    case durationAuto

    private enum CodingKeys: String, CodingKey { case type, value, width, height }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        let type = try container.decode(String.self, forKey: .type)
        switch type {
        case "enum": self = .enumValue(try container.decode(String.self, forKey: .value))
        case "integer": self = .integer(try container.decode(Int64.self, forKey: .value))
        case "number": self = .number(try container.decode(Double.self, forKey: .value))
        case "boolean": self = .boolean(try container.decode(Bool.self, forKey: .value))
        case "dimensions": self = .dimensions(
            width: try container.decode(UInt32.self, forKey: .width),
            height: try container.decode(UInt32.self, forKey: .height)
        )
        case "aspect_ratio": self = .aspectRatio(
            width: try container.decode(UInt32.self, forKey: .width),
            height: try container.decode(UInt32.self, forKey: .height)
        )
        case "aspect_ratio_auto": self = .aspectRatioAuto
        case "aspect_ratio_adaptive": self = .aspectRatioAdaptive
        case "resolution": self = .resolution(try container.decode(String.self, forKey: .value))
        case "duration_seconds": self = .durationSeconds(
            try container.decode(Double.self, forKey: .value)
        )
        case "duration_auto": self = .durationAuto
        default:
            throw DecodingError.dataCorruptedError(
                forKey: .type,
                in: container,
                debugDescription: "Unknown Studio control value \(type)"
            )
        }
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .enumValue(let value):
            try container.encode("enum", forKey: .type)
            try container.encode(value, forKey: .value)
        case .integer(let value):
            try container.encode("integer", forKey: .type)
            try container.encode(value, forKey: .value)
        case .number(let value):
            try container.encode("number", forKey: .type)
            try container.encode(value, forKey: .value)
        case .boolean(let value):
            try container.encode("boolean", forKey: .type)
            try container.encode(value, forKey: .value)
        case .dimensions(let width, let height):
            try container.encode("dimensions", forKey: .type)
            try container.encode(width, forKey: .width)
            try container.encode(height, forKey: .height)
        case .aspectRatio(let width, let height):
            try container.encode("aspect_ratio", forKey: .type)
            try container.encode(width, forKey: .width)
            try container.encode(height, forKey: .height)
        case .aspectRatioAuto:
            try container.encode("aspect_ratio_auto", forKey: .type)
        case .aspectRatioAdaptive:
            try container.encode("aspect_ratio_adaptive", forKey: .type)
        case .resolution(let value):
            try container.encode("resolution", forKey: .type)
            try container.encode(value, forKey: .value)
        case .durationSeconds(let value):
            try container.encode("duration_seconds", forKey: .type)
            try container.encode(value, forKey: .value)
        case .durationAuto:
            try container.encode("duration_auto", forKey: .type)
        }
    }

    var label: String {
        switch self {
        case .enumValue(let value), .resolution(let value): value
        case .integer(let value): String(value)
        case .number(let value): value.formatted()
        case .boolean(let value): value ? "On" : "Off"
        case .dimensions(let width, let height): "\(width)×\(height)"
        case .aspectRatio(let width, let height): "\(width):\(height)"
        case .aspectRatioAuto, .durationAuto: "Auto"
        case .aspectRatioAdaptive: "Adaptive"
        case .durationSeconds(let value):
            value.rounded() == value ? "\(Int(value))s" : "\(value.formatted())s"
        }
    }
}

struct StudioControlChoice: Codable, Hashable {
    var value: StudioControlValue
    var label: String
}

struct StudioModelControl: Codable, Hashable, Identifiable {
    var id: String
    var label: String
    var description: String?
    var kind: StudioControlKind
    var required: Bool
    var `default`: StudioControlValue?
    var minimum: Double?
    var maximum: Double?
    var step: Double?
    var choices: [StudioControlChoice]
}

struct StudioComposerModel: Codable, Hashable, Identifiable {
    var providerId: String
    var id: String
    var displayName: String
    var description: String?
    var operation: StudioMediaOperation
    var outputKind: StudioMediaKind
    var maximumOutputCount: UInt32
    var controls: [StudioModelControl]
    var manifestVersion: String

    private enum CodingKeys: String, CodingKey {
        case providerId = "provider_id"
        case id
        case displayName = "display_name"
        case description
        case operation
        case outputKind = "output_kind"
        case maximumOutputCount = "maximum_output_count"
        case controls
        case manifestVersion = "manifest_version"
    }
}

struct StudioProviderConnection: Codable, Hashable, Identifiable {
    var providerId: String
    var displayLabel: String
    var configured: Bool

    var id: String { providerId }
}

struct StudioProviderListResponse: Codable {
    var providers: [StudioProviderConnection]
}

struct StudioModelListResponse: Codable {
    var models: [StudioComposerModel]
    var fetchedAt: String
    var stale: Bool
}

struct StudioSelectedModel: Codable, Hashable, Identifiable {
    var providerId: String
    var modelId: String
    var outputCount: UInt32
    var controls: [String: StudioControlValue]

    var id: String { modelId }
}

enum StudioComposerMediaKind: String, Codable, Hashable {
    case image
    case video
    case audio
}

enum StudioAttachmentOrigin: Hashable, Codable {
    case asset
    case artifact(String)

    private enum CodingKeys: String, CodingKey { case type, artifactId }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        switch try container.decode(String.self, forKey: .type) {
        case "asset": self = .asset
        case "artifact": self = .artifact(try container.decode(String.self, forKey: .artifactId))
        default:
            throw DecodingError.dataCorruptedError(
                forKey: .type,
                in: container,
                debugDescription: "Unknown Studio attachment origin"
            )
        }
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .asset:
            try container.encode("asset", forKey: .type)
        case .artifact(let artifactId):
            try container.encode("artifact", forKey: .type)
            try container.encode(artifactId, forKey: .artifactId)
        }
    }
}

struct StudioComposerAttachment: Codable, Hashable, Identifiable {
    var id: String
    var kind: StudioComposerMediaKind
    var pending: Bool
    var origin: StudioAttachmentOrigin
    var mimeType: String
    var byteSize: UInt64
    var width: UInt32?
    var height: UInt32?
    var durationSeconds: Double?
    var contentHash: String
    var roleHint: String?
}

struct StudioComposerSnapshot: Codable, Hashable {
    var conversationId: String?
    var mode: StudioComposerMode
    var prompt: String
    var duration: StudioControlValue?
    var attachments: [StudioComposerAttachment]
    var selected: [StudioSelectedModel]
    var sourceTurnId: String?
    var catalogFetchedAt: String?

    init(conversationId: String, mode: StudioComposerMode = .image) {
        self.conversationId = conversationId
        self.mode = mode
        prompt = ""
        duration = nil
        attachments = []
        selected = []
        sourceTurnId = nil
        catalogFetchedAt = nil
    }
}

struct StudioComposerSendState: Codable, Hashable {
    var enabled: Bool
    var blockedReason: String?
}

struct StudioComposerGlobals: Codable, Hashable {
    var duration: StudioControlValue?
    var durationChoices: [StudioControlChoice]
}

struct StudioComposerChip: Codable, Hashable, Identifiable {
    var modelId: String
    var displayName: String
    var operation: StudioMediaOperation
    var outputCount: UInt32
    var controls: [StudioModelControl]
    var values: [String: StudioControlValue]
    var badge: String?

    var id: String { modelId }
}

struct StudioTrayAccept: Codable, Hashable {
    var mimeTypes: [String]
}

struct StudioComposerTray: Codable, Hashable {
    var items: [StudioComposerAttachment]
    var accept: StudioTrayAccept
    var addEnabled: Bool
}

struct StudioConflictAction: Codable, Hashable {
    var action: JSONValue
    var label: String
}

struct StudioComposerConflict: Codable, Hashable, Identifiable {
    var id: String
    var code: String
    var severity: String
    var title: String
    var explanation: String
    var actions: [StudioConflictAction]
}

struct StudioComposerEvaluation: Codable, Hashable {
    var send: StudioComposerSendState
    var globals: StudioComposerGlobals
    var models: [StudioComposerChip]
    var attachments: StudioComposerTray
    var budgets: [JSONValue]
    var hints: [JSONValue]
    var conflicts: [StudioComposerConflict]
    var catalogStale: Bool
    var openPicker: Bool
    var refreshCatalog: Bool
}

struct StudioAssetImportProgress: Codable {
    var assetId: String
    var nextOffset: UInt64
}

struct StudioCreateTurnRequest: Codable {
    var conversationId: String
    var prompt: String
    var runs: [JSONValue] = []
    var sourceTurnId: String?
    var composer: StudioComposerSnapshot
}
