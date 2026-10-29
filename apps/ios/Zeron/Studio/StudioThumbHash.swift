import Foundation
import Synchronization
import UIKit

// ThumbHash decoder adapted from Evan Wallace's MIT-licensed reference
// implementation: https://github.com/evanw/thumbhash
// Copyright (c) 2023 Evan Wallace

enum StudioThumbHash {
    nonisolated static func image(base64: String?, aspectRatio: CGFloat? = nil) -> UIImage? {
        guard let base64,
              let hash = Data(base64Encoded: base64),
              let decoded = decode(hash, aspectRatio: aspectRatio) else { return nil }
        return decoded.image
    }

    private struct DecodedImage {
        let image: UIImage
    }

    private nonisolated static func decode(_ hash: Data, aspectRatio: CGFloat?) -> DecodedImage? {
        guard hash.count >= 5 else { return nil }
        let bytes = [UInt8](hash)
        let header24 = UInt32(bytes[0]) | UInt32(bytes[1]) << 8 | UInt32(bytes[2]) << 16
        let header16 = UInt16(bytes[3]) | UInt16(bytes[4]) << 8

        let luminanceDC = Float32(header24 & 63) / 63
        let pDC = Float32((header24 >> 6) & 63) / 31.5 - 1
        let qDC = Float32((header24 >> 12) & 63) / 31.5 - 1
        let luminanceScale = Float32((header24 >> 18) & 31) / 31
        let hasAlpha = (header24 >> 23) != 0
        guard !hasAlpha || bytes.count >= 6 else { return nil }

        let pScale = Float32((header16 >> 3) & 63) / 63
        let qScale = Float32((header16 >> 9) & 63) / 63
        let landscape = (header16 >> 15) != 0
        let encodedCount = Int(header16 & 7)
        let luminanceX = max(3, landscape ? (hasAlpha ? 5 : 7) : encodedCount)
        let luminanceY = max(3, landscape ? encodedCount : (hasAlpha ? 5 : 7))
        let alphaDC = hasAlpha ? Float32(bytes[5] & 15) / 15 : 1
        let alphaScale = hasAlpha ? Float32(bytes[5] >> 4) / 15 : 1

        let coefficientStart = hasAlpha ? 6 : 5
        var coefficientIndex = 0
        func decodeChannel(_ nx: Int, _ ny: Int, _ scale: Float32) -> [Float32]? {
            var coefficients: [Float32] = []
            var cy = 0
            while cy < ny {
                var cx = cy > 0 ? 0 : 1
                while cx * ny < nx * (ny - cy) {
                    let byteIndex = coefficientStart + (coefficientIndex >> 1)
                    guard bytes.indices.contains(byteIndex) else { return nil }
                    let shift = (coefficientIndex & 1) << 2
                    let value = (bytes[byteIndex] >> shift) & 15
                    coefficients.append((Float32(value) / 7.5 - 1) * scale)
                    coefficientIndex += 1
                    cx += 1
                }
                cy += 1
            }
            return coefficients
        }

        guard let luminanceAC = decodeChannel(luminanceX, luminanceY, luminanceScale),
              let pAC = decodeChannel(3, 3, pScale * 1.25),
              let qAC = decodeChannel(3, 3, qScale * 1.25) else { return nil }
        let alphaAC: [Float32]
        if hasAlpha {
            guard let decoded = decodeChannel(5, 5, alphaScale) else { return nil }
            alphaAC = decoded
        } else {
            alphaAC = []
        }

        let suppliedRatio = aspectRatio.flatMap { ratio in
            ratio.isFinite && ratio > 0 ? Float32(ratio) : nil
        }
        let ratio = suppliedRatio ?? approximateAspectRatio(bytes)
        guard ratio.isFinite, ratio > 0 else { return nil }
        let width = Int(round(ratio > 1 ? 32 : 32 * ratio))
        let height = Int(round(ratio > 1 ? 32 / ratio : 32))
        guard width > 0, height > 0 else { return nil }

        let xStop = max(luminanceX, hasAlpha ? 5 : 3)
        let yStop = max(luminanceY, hasAlpha ? 5 : 3)
        // The cosine bases, once per column and row (not per pixel).
        var xCos = [Float32](repeating: 0, count: width * xStop)
        for x in 0..<width {
            for cx in 0..<xStop {
                xCos[x * xStop + cx] = cos(Float32.pi / Float32(width) * (Float32(x) + 0.5) * Float32(cx))
            }
        }
        var yCos = [Float32](repeating: 0, count: height * yStop)
        for y in 0..<height {
            for cy in 0..<yStop {
                yCos[y * yStop + cy] = cos(Float32.pi / Float32(height) * (Float32(y) + 0.5) * Float32(cy)) * 2
            }
        }

        // BGRA, premultiplied first, little-endian: the display's native
        // format, so Core Animation draws it as is.
        var bgra = Data(count: width * height * 4)
        bgra.withUnsafeMutableBytes { rawBuffer in
            guard var output = rawBuffer.baseAddress?.bindMemory(
                to: UInt8.self,
                capacity: rawBuffer.count
            ) else { return }

            for y in 0..<height {
                let yRow = y * yStop
                for x in 0..<width {
                    let xRow = x * xStop
                    var luminance = luminanceDC
                    var p = pDC
                    var q = qDC
                    var alpha = alphaDC

                    var coefficient = 0
                    var cy = 0
                    while cy < luminanceY {
                        var cx = cy > 0 ? 0 : 1
                        let fy = yCos[yRow + cy]
                        while cx * luminanceY < luminanceX * (luminanceY - cy) {
                            luminance += luminanceAC[coefficient] * xCos[xRow + cx] * fy
                            coefficient += 1
                            cx += 1
                        }
                        cy += 1
                    }

                    coefficient = 0
                    cy = 0
                    while cy < 3 {
                        var cx = cy > 0 ? 0 : 1
                        let fy = yCos[yRow + cy]
                        while cx < 3 - cy {
                            let factor = xCos[xRow + cx] * fy
                            p += pAC[coefficient] * factor
                            q += qAC[coefficient] * factor
                            coefficient += 1
                            cx += 1
                        }
                        cy += 1
                    }

                    if hasAlpha {
                        coefficient = 0
                        cy = 0
                        while cy < 5 {
                            var cx = cy > 0 ? 0 : 1
                            let fy = yCos[yRow + cy]
                            while cx < 5 - cy {
                                alpha += alphaAC[coefficient] * xCos[xRow + cx] * fy
                                coefficient += 1
                                cx += 1
                            }
                            cy += 1
                        }
                    }

                    var blue = luminance - 2 / 3 * p
                    var red = (3 * luminance - blue + q) / 2
                    var green = red - q
                    red = max(0, 255 * min(1, red))
                    green = max(0, 255 * min(1, green))
                    blue = max(0, 255 * min(1, blue))
                    alpha = max(0, 255 * min(1, alpha))

                    let alphaByte = UInt16(alpha)
                    output[0] = UInt8(min(255, UInt16(blue) * alphaByte / 255))
                    output[1] = UInt8(min(255, UInt16(green) * alphaByte / 255))
                    output[2] = UInt8(min(255, UInt16(red) * alphaByte / 255))
                    output[3] = UInt8(alphaByte)
                    output = output.advanced(by: 4)
                }
            }
        }

        guard let provider = CGDataProvider(data: bgra as CFData),
              let cgImage = CGImage(
                width: width,
                height: height,
                bitsPerComponent: 8,
                bitsPerPixel: 32,
                bytesPerRow: width * 4,
                space: CGColorSpaceCreateDeviceRGB(),
                bitmapInfo: CGBitmapInfo(
                    rawValue: CGBitmapInfo.byteOrder32Little.rawValue
                        | CGImageAlphaInfo.premultipliedFirst.rawValue
                ),
                provider: provider,
                decode: nil,
                shouldInterpolate: true,
                intent: .perceptual
              ) else { return nil }
        return DecodedImage(image: UIImage(cgImage: cgImage))
    }

    /// The source image's shape as the hash records it (coarse: the
    /// encoder's component grid, e.g. 7:4 for 16:9), for artifacts whose
    /// provider didn't report dimensions. Nil for an unreadable hash.
    nonisolated static func aspectRatio(base64: String?) -> CGFloat? {
        guard let base64 else { return nil }
        if let known = aspectRatios.withLock({ $0[base64] }) { return known }
        guard let hash = Data(base64Encoded: base64), hash.count >= 5 else { return nil }
        let ratio = CGFloat(approximateAspectRatio([UInt8](hash)))
        guard ratio.isFinite, ratio > 0 else { return nil }
        aspectRatios.withLock { cache in
            if cache.count > 8_000 { cache.removeAll(keepingCapacity: true) }
            cache[base64] = ratio
        }
        return ratio
    }

    private nonisolated static let aspectRatios = Mutex<[String: CGFloat]>([:])

    private nonisolated static func approximateAspectRatio(_ hash: [UInt8]) -> Float32 {
        let header = hash[3]
        let hasAlpha = (hash[2] & 0x80) != 0
        let landscape = (hash[4] & 0x80) != 0
        let x = landscape ? (hasAlpha ? 5 : 7) : header & 7
        let y = landscape ? header & 7 : (hasAlpha ? 5 : 7)
        return Float32(x) / Float32(max(y, 1))
    }
}

/// Decoded thumbhashes by hash and aspect ratio, shared by every Studio
/// screen and safe from any thread: gallery pages and prefetch decode them
/// in the background, so a square has its placeholder on its first frame.
/// A 32px image is 4 KB; the limit holds a few thousand.
final class StudioThumbHashCache: @unchecked Sendable {
    static let shared = StudioThumbHashCache()
    private let cache = NSCache<NSString, UIImage>()

    private init() {
        cache.countLimit = 4_000
        cache.totalCostLimit = 16 * 1024 * 1024
    }

    func image(_ thumbhash: String, aspectRatio: CGFloat?) -> UIImage? {
        cache.object(forKey: Self.key(thumbhash, aspectRatio))
    }

    /// Decodes and keeps it (any thread).
    @discardableResult
    func decode(_ thumbhash: String, aspectRatio: CGFloat?) -> UIImage? {
        let key = Self.key(thumbhash, aspectRatio)
        if let image = cache.object(forKey: key) { return image }
        guard let image = StudioThumbHash.image(base64: thumbhash, aspectRatio: aspectRatio) else { return nil }
        let cost = Int(image.size.width * image.scale * image.size.height * image.scale * 4)
        cache.setObject(image, forKey: key, cost: cost)
        return image
    }

    /// Every item's thumbhash, ahead of display (call off the main thread).
    func warm(_ items: [StudioGalleryItem]) {
        for item in items {
            if let thumbhash = item.thumbhash { decode(thumbhash, aspectRatio: item.aspectRatio) }
        }
    }

    func removeAll() { cache.removeAllObjects() }

    private static func key(_ thumbhash: String, _ aspectRatio: CGFloat?) -> NSString {
        guard let aspectRatio, aspectRatio.isFinite else { return "\(thumbhash):raw" as NSString }
        return "\(thumbhash):\(Int((aspectRatio * 10_000).rounded()))" as NSString
    }
}
