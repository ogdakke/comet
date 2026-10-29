import Foundation

/// Studio media and control calls share the device relay. Keep chunked media
/// transfers bounded so gallery prefetch cannot create a wall of RPCs that
/// starves the relay or the image being opened.
///
/// Foreground reads (the viewer's original, a video's chunks) go one at a
/// time and first: while one runs or waits, no preview starts. Previews run
/// a few at once (the relay is latency-bound, so one at a time leaves it
/// idle between round trips), squares on screen before prefetch, the one
/// that came on screen last first: in a fling it's the one that stays.
actor StudioMediaRequestGate {
    private struct Waiter {
        let id: UUID
        let key: String?
        let continuation: CheckedContinuation<Void, Error>
    }

    private let previewSlots: Int
    private var foregroundActive = false
    private var previewsActive = 0
    private var foreground: [Waiter] = []
    private var visible: [Waiter] = []
    private var prefetch: [Waiter] = []
    /// Promoted before they queued (a prefetch whose square appeared while
    /// it was still checking the disk cache).
    private var promotedKeys: Set<String> = []

    init(previewSlots: Int = 3) {
        self.previewSlots = previewSlots
    }

    func acquire(priority: StudioMediaRequestPriority, key: String? = nil) async throws {
        try Task.checkCancellation()
        var priority = priority
        if priority == .prefetch, let key, promotedKeys.remove(key) != nil { priority = .visible }
        if tryGrant(priority) { return }

        let id = UUID()
        try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { continuation in
                let waiter = Waiter(id: id, key: key, continuation: continuation)
                switch priority {
                case .foreground: foreground.append(waiter)
                case .visible: visible.append(waiter)
                case .prefetch: prefetch.append(waiter)
                }
                if Task.isCancelled {
                    cancelWaiter(id: id)
                }
            }
        } onCancel: {
            Task { await self.cancelWaiter(id: id) }
        }

        if Task.isCancelled {
            release(priority: priority)
            throw CancellationError()
        }
    }

    /// Release with the priority the hold was acquired with (a promoted
    /// prefetch is still a preview slot).
    func release(priority: StudioMediaRequestPriority) {
        if priority == .foreground {
            foregroundActive = false
        } else {
            previewsActive = max(0, previewsActive - 1)
        }
        pump()
    }

    /// A square came on screen: its queued prefetch moves to the front.
    func promote(key: String) {
        if let index = prefetch.firstIndex(where: { $0.key == key }) {
            visible.append(prefetch.remove(at: index))
        } else if !visible.contains(where: { $0.key == key }) {
            if promotedKeys.count > 512 { promotedKeys.removeAll() }
            promotedKeys.insert(key)
        }
    }

    private func tryGrant(_ priority: StudioMediaRequestPriority) -> Bool {
        switch priority {
        case .foreground:
            guard !foregroundActive, foreground.isEmpty else { return false }
            foregroundActive = true
            return true
        case .visible, .prefetch:
            guard !foregroundActive, foreground.isEmpty, previewsActive < previewSlots,
                  visible.isEmpty, priority == .visible || prefetch.isEmpty else { return false }
            previewsActive += 1
            return true
        }
    }

    private func pump() {
        if !foregroundActive, !foreground.isEmpty {
            foregroundActive = true
            foreground.removeFirst().continuation.resume()
        }
        guard !foregroundActive, foreground.isEmpty else { return }
        while previewsActive < previewSlots {
            let next: Waiter
            if let last = visible.popLast() {
                next = last
            } else if !prefetch.isEmpty {
                next = prefetch.removeFirst()
            } else {
                return
            }
            previewsActive += 1
            next.continuation.resume()
        }
    }

    private func cancelWaiter(id: UUID) {
        if let index = foreground.firstIndex(where: { $0.id == id }) {
            foreground.remove(at: index).continuation.resume(throwing: CancellationError())
        } else if let index = visible.firstIndex(where: { $0.id == id }) {
            visible.remove(at: index).continuation.resume(throwing: CancellationError())
        } else if let index = prefetch.firstIndex(where: { $0.id == id }) {
            prefetch.remove(at: index).continuation.resume(throwing: CancellationError())
        }
    }
}

/// A JSON value for the Studio wire fields the app passes through untyped.
enum JSONValue: Hashable, Sendable {
    case null
    case bool(Bool)
    case int(Int64)
    case double(Double)
    case string(String)
    case array([JSONValue])
    case object([String: JSONValue])
}

extension JSONValue: Codable {
    init(from decoder: Decoder) throws {
        let c = try decoder.singleValueContainer()
        if c.decodeNil() {
            self = .null
        } else if let b = try? c.decode(Bool.self) {
            self = .bool(b)
        } else if let i = try? c.decode(Int64.self) {
            self = .int(i)
        } else if let d = try? c.decode(Double.self) {
            self = .double(d)
        } else if let s = try? c.decode(String.self) {
            self = .string(s)
        } else if let a = try? c.decode([JSONValue].self) {
            self = .array(a)
        } else if let o = try? c.decode([String: JSONValue].self) {
            self = .object(o)
        } else {
            throw DecodingError.dataCorruptedError(in: c, debugDescription: "not a JSON value")
        }
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        switch self {
        case .null: try c.encodeNil()
        case .bool(let v): try c.encode(v)
        case .int(let v): try c.encode(v)
        case .double(let v): try c.encode(v)
        case .string(let v): try c.encode(v)
        case .array(let v): try c.encode(v)
        case .object(let v): try c.encode(v)
        }
    }
}

extension JSONValue {
    var stringValue: String? {
        if case .string(let v) = self { return v }
        return nil
    }

    var boolValue: Bool? {
        if case .bool(let v) = self { return v }
        return nil
    }

    var int64Value: Int64? {
        switch self {
        case .int(let v): return v
        case .double(let v): return Int64(v)
        default: return nil
        }
    }

    var objectValue: [String: JSONValue]? {
        if case .object(let v) = self { return v }
        return nil
    }

    var isNull: Bool { self == .null }

    /// Bridge an Encodable (e.g. ChatConfig) into a field value.
    init?<T: Encodable>(encodable: T) {
        guard let data = try? JSONEncoder().encode(encodable),
              let value = try? JSONDecoder().decode(JSONValue.self, from: data) else { return nil }
        self = value
    }
}
