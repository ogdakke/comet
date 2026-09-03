import Foundation
import UIKit

// ThumbHash decoder adapted from Evan Wallace's MIT-licensed reference
// implementation: https://github.com/evanw/thumbhash
// Copyright (c) 2023 Evan Wallace

enum StudioThumbHash {
    static func image(base64: String?, aspectRatio: CGFloat? = nil) -> UIImage? {
        guard let base64,
              let hash = Data(base64Encoded: base64),
              let decoded = decode(hash, aspectRatio: aspectRatio) else { return nil }
        return decoded.image
    }

    private struct DecodedImage {
        let image: UIImage
    }

    private static func decode(_ hash: Data, aspectRatio: CGFloat?) -> DecodedImage? {
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

        var rgba = Data(count: width * height * 4)
        let xStop = max(luminanceX, hasAlpha ? 5 : 3)
        let yStop = max(luminanceY, hasAlpha ? 5 : 3)
        var xCoefficients = [Float32](repeating: 0, count: xStop)
        var yCoefficients = [Float32](repeating: 0, count: yStop)

        rgba.withUnsafeMutableBytes { rawBuffer in
            guard var output = rawBuffer.baseAddress?.bindMemory(
                to: UInt8.self,
                capacity: rawBuffer.count
            ) else { return }

            var y = 0
            while y < height {
                var x = 0
                while x < width {
                    var luminance = luminanceDC
                    var p = pDC
                    var q = qDC
                    var alpha = alphaDC

                    var cx = 0
                    while cx < xStop {
                        xCoefficients[cx] = cos(
                            Float32.pi / Float32(width) * (Float32(x) + 0.5) * Float32(cx)
                        )
                        cx += 1
                    }
                    var cy = 0
                    while cy < yStop {
                        yCoefficients[cy] = cos(
                            Float32.pi / Float32(height) * (Float32(y) + 0.5) * Float32(cy)
                        )
                        cy += 1
                    }

                    var coefficient = 0
                    cy = 0
                    while cy < luminanceY {
                        cx = cy > 0 ? 0 : 1
                        let fy = yCoefficients[cy] * 2
                        while cx * luminanceY < luminanceX * (luminanceY - cy) {
                            luminance += luminanceAC[coefficient] * xCoefficients[cx] * fy
                            coefficient += 1
                            cx += 1
                        }
                        cy += 1
                    }

                    coefficient = 0
                    cy = 0
                    while cy < 3 {
                        cx = cy > 0 ? 0 : 1
                        let fy = yCoefficients[cy] * 2
                        while cx < 3 - cy {
                            let factor = xCoefficients[cx] * fy
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
                            cx = cy > 0 ? 0 : 1
                            let fy = yCoefficients[cy] * 2
                            while cx < 5 - cy {
                                alpha += alphaAC[coefficient] * xCoefficients[cx] * fy
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
                    output[0] = UInt8(min(255, UInt16(red) * alphaByte / 255))
                    output[1] = UInt8(min(255, UInt16(green) * alphaByte / 255))
                    output[2] = UInt8(min(255, UInt16(blue) * alphaByte / 255))
                    output[3] = UInt8(alphaByte)
                    output = output.advanced(by: 4)
                    x += 1
                }
                y += 1
            }
        }

        guard let provider = CGDataProvider(data: rgba as CFData),
              let cgImage = CGImage(
                width: width,
                height: height,
                bitsPerComponent: 8,
                bitsPerPixel: 32,
                bytesPerRow: width * 4,
                space: CGColorSpaceCreateDeviceRGB(),
                bitmapInfo: CGBitmapInfo(
                    rawValue: CGBitmapInfo.byteOrder32Big.rawValue
                        | CGImageAlphaInfo.premultipliedLast.rawValue
                ),
                provider: provider,
                decode: nil,
                shouldInterpolate: true,
                intent: .perceptual
              ) else { return nil }
        return DecodedImage(image: UIImage(cgImage: cgImage))
    }

    private static func approximateAspectRatio(_ hash: [UInt8]) -> Float32 {
        let header = hash[3]
        let hasAlpha = (hash[2] & 0x80) != 0
        let landscape = (hash[4] & 0x80) != 0
        let x = landscape ? (hasAlpha ? 5 : 7) : header & 7
        let y = landscape ? header & 7 : (hasAlpha ? 5 : 7)
        return Float32(x) / Float32(max(y, 1))
    }
}
