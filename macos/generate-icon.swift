import AppKit
import CoreGraphics
import Foundation

let outputURL: URL
if CommandLine.arguments.count > 1 {
    outputURL = URL(fileURLWithPath: CommandLine.arguments[1])
} else {
    outputURL = URL(fileURLWithPath: ".build/MacEveryIcon.icns")
}

try writeICNS(to: outputURL)

func writeICNS(to url: URL) throws {
    let chunks = [
        ("icp4", 16),
        ("icp5", 32),
        ("icp6", 64),
        ("ic07", 128),
        ("ic08", 256),
        ("ic09", 512),
        ("ic10", 1024)
    ].map { type, size in
        (type, pngData(for: size))
    }

    let totalLength = 8 + chunks.reduce(0) { partial, chunk in
        partial + 8 + chunk.1.count
    }

    var data = Data()
    appendFourCC("icns", to: &data)
    appendBE32(UInt32(totalLength), to: &data)
    for (type, payload) in chunks {
        appendFourCC(type, to: &data)
        appendBE32(UInt32(payload.count + 8), to: &data)
        data.append(payload)
    }
    try data.write(to: url)
}

func pngData(for size: Int) -> Data {
    let image = renderIcon(size: size)
    let rep = NSBitmapImageRep(cgImage: image)
    guard let data = rep.representation(using: .png, properties: [:]) else {
        fatalError("failed to encode \(size)x\(size) icon")
    }
    return data
}

func appendFourCC(_ value: String, to data: inout Data) {
    let bytes = Array(value.utf8)
    precondition(bytes.count == 4)
    data.append(contentsOf: bytes)
}

func appendBE32(_ value: UInt32, to data: inout Data) {
    var bigEndian = value.bigEndian
    withUnsafeBytes(of: &bigEndian) { bytes in
        data.append(contentsOf: bytes)
    }
}

func renderIcon(size: Int) -> CGImage {
    let width = size
    let height = size
    let colorSpace = CGColorSpaceCreateDeviceRGB()
    let bitmapInfo = CGImageAlphaInfo.premultipliedLast.rawValue
    guard let context = CGContext(
        data: nil,
        width: width,
        height: height,
        bitsPerComponent: 8,
        bytesPerRow: 0,
        space: colorSpace,
        bitmapInfo: bitmapInfo
    ) else {
        fatalError("failed to create bitmap context")
    }

    let scale = CGFloat(size) / 1024.0
    let bounds = CGRect(x: 0, y: 0, width: CGFloat(size), height: CGFloat(size))
    context.clear(bounds)

    let radius = 220.0 * scale
    let rounded = CGPath(
        roundedRect: bounds.insetBy(dx: 68 * scale, dy: 68 * scale),
        cornerWidth: radius,
        cornerHeight: radius,
        transform: nil
    )
    context.addPath(rounded)
    context.clip()

    let gradient = CGGradient(
        colorsSpace: colorSpace,
        colors: [
            NSColor(calibratedRed: 0.09, green: 0.35, blue: 0.96, alpha: 1).cgColor,
            NSColor(calibratedRed: 0.02, green: 0.70, blue: 0.86, alpha: 1).cgColor
        ] as CFArray,
        locations: [0.0, 1.0]
    )!
    context.drawLinearGradient(
        gradient,
        start: CGPoint(x: 160 * scale, y: 880 * scale),
        end: CGPoint(x: 880 * scale, y: 140 * scale),
        options: []
    )

    context.setFillColor(NSColor(calibratedWhite: 1, alpha: 0.15).cgColor)
    context.fillEllipse(in: CGRect(x: 600 * scale, y: 590 * scale, width: 380 * scale, height: 380 * scale))
    context.setFillColor(NSColor(calibratedWhite: 0, alpha: 0.12).cgColor)
    context.fillEllipse(in: CGRect(x: -80 * scale, y: -60 * scale, width: 440 * scale, height: 440 * scale))

    context.resetClip()
    context.addPath(rounded)
    context.setStrokeColor(NSColor(calibratedWhite: 1, alpha: 0.20).cgColor)
    context.setLineWidth(18 * scale)
    context.strokePath()

    let lensRect = CGRect(x: 250 * scale, y: 380 * scale, width: 360 * scale, height: 360 * scale)
    context.setStrokeColor(NSColor.white.cgColor)
    context.setLineWidth(76 * scale)
    context.setLineCap(.round)
    context.strokeEllipse(in: lensRect)
    context.move(to: CGPoint(x: 575 * scale, y: 365 * scale))
    context.addLine(to: CGPoint(x: 770 * scale, y: 170 * scale))
    context.strokePath()

    context.setStrokeColor(NSColor(calibratedRed: 0.02, green: 0.15, blue: 0.35, alpha: 0.25).cgColor)
    context.setLineWidth(26 * scale)
    context.strokeEllipse(in: lensRect.insetBy(dx: -3 * scale, dy: -3 * scale))

    let text = "M"
    let fontSize = max(1, 250 * scale)
    let paragraph = NSMutableParagraphStyle()
    paragraph.alignment = .center
    let attributes: [NSAttributedString.Key: Any] = [
        .font: NSFont.systemFont(ofSize: fontSize, weight: .black),
        .foregroundColor: NSColor.white.withAlphaComponent(0.92),
        .paragraphStyle: paragraph
    ]
    let string = NSAttributedString(string: text, attributes: attributes)
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(cgContext: context, flipped: false)
    string.draw(in: CGRect(x: 405 * scale, y: 350 * scale, width: 250 * scale, height: 280 * scale))
    NSGraphicsContext.restoreGraphicsState()

    guard let image = context.makeImage() else {
        fatalError("failed to create icon image")
    }
    return image
}
