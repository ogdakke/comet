import Foundation

/// Studio media and control calls share the device relay. Keep chunked media
/// transfers ordered so gallery prefetch cannot create a wall of RPCs that
/// starves the relay or the image being opened.
actor StudioMediaRequestGate {
    private struct Waiter {
        let id: UUID
        let continuation: CheckedContinuation<Void, Error>
    }

    private var occupied = false
    private var foreground: [Waiter] = []
    private var background: [Waiter] = []

    func acquire(priority: StudioMediaRequestPriority) async throws {
        try Task.checkCancellation()
        if !occupied {
            occupied = true
            return
        }

        let id = UUID()
        try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { continuation in
                let waiter = Waiter(id: id, continuation: continuation)
                switch priority {
                case .foreground: foreground.append(waiter)
                case .background: background.append(waiter)
                }
                if Task.isCancelled {
                    cancelWaiter(id: id)
                }
            }
        } onCancel: {
            Task { await self.cancelWaiter(id: id) }
        }

        if Task.isCancelled {
            release()
            throw CancellationError()
        }
    }

    func release() {
        if let waiter = popNext() {
            waiter.continuation.resume()
        } else {
            occupied = false
        }
    }

    private func popNext() -> Waiter? {
        if !foreground.isEmpty { return foreground.removeFirst() }
        if !background.isEmpty { return background.removeFirst() }
        return nil
    }

    private func cancelWaiter(id: UUID) {
        if let index = foreground.firstIndex(where: { $0.id == id }) {
            foreground.remove(at: index).continuation.resume(throwing: CancellationError())
            return
        }
        if let index = background.firstIndex(where: { $0.id == id }) {
            background.remove(at: index).continuation.resume(throwing: CancellationError())
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
