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

    let radius = 210.0 * scale
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
            NSColor(calibratedRed: 0.05, green: 0.20, blue: 0.58, alpha: 1).cgColor,
            NSColor(calibratedRed: 0.00, green: 0.56, blue: 0.70, alpha: 1).cgColor
        ] as CFArray,
        locations: [0.0, 1.0]
    )!
    context.drawLinearGradient(
        gradient,
        start: CGPoint(x: 160 * scale, y: 880 * scale),
        end: CGPoint(x: 880 * scale, y: 140 * scale),
        options: []
    )

    context.setStrokeColor(NSColor(calibratedWhite: 1, alpha: 0.15).cgColor)
    context.setLineWidth(24 * scale)
    context.setLineCap(.round)
    for y in [270.0, 390.0, 510.0] {
        context.move(to: CGPoint(x: 185 * scale, y: y * scale))
        context.addLine(to: CGPoint(x: 500 * scale, y: y * scale))
        context.strokePath()
    }
    for point in [
        CGRect(x: 155 * scale, y: 240 * scale, width: 60 * scale, height: 60 * scale),
        CGRect(x: 155 * scale, y: 360 * scale, width: 60 * scale, height: 60 * scale),
        CGRect(x: 155 * scale, y: 480 * scale, width: 60 * scale, height: 60 * scale)
    ] {
        context.setFillColor(NSColor(calibratedWhite: 1, alpha: 0.22).cgColor)
        context.fillEllipse(in: point)
    }

    context.resetClip()
    context.addPath(rounded)
    context.setStrokeColor(NSColor(calibratedWhite: 1, alpha: 0.20).cgColor)
    context.setLineWidth(18 * scale)
    context.strokePath()

    let lensRect = CGRect(x: 322 * scale, y: 360 * scale, width: 370 * scale, height: 370 * scale)
    context.setShadow(offset: CGSize(width: 0, height: -18 * scale), blur: 34 * scale, color: NSColor.black.withAlphaComponent(0.20).cgColor)
    context.setStrokeColor(NSColor.white.cgColor)
    context.setLineWidth(74 * scale)
    context.setLineCap(.round)
    context.strokeEllipse(in: lensRect)
    context.move(to: CGPoint(x: 650 * scale, y: 345 * scale))
    context.addLine(to: CGPoint(x: 820 * scale, y: 175 * scale))
    context.strokePath()
    context.setShadow(offset: .zero, blur: 0, color: nil)

    context.setStrokeColor(NSColor(calibratedRed: 0.02, green: 0.15, blue: 0.35, alpha: 0.25).cgColor)
    context.setLineWidth(22 * scale)
    context.strokeEllipse(in: lensRect.insetBy(dx: -5 * scale, dy: -5 * scale))

    let text = "M"
    let fontSize = max(1, 230 * scale)
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
    string.draw(in: CGRect(x: 390 * scale, y: 410 * scale, width: 250 * scale, height: 260 * scale))
    NSGraphicsContext.restoreGraphicsState()

    guard let image = context.makeImage() else {
        fatalError("failed to create icon image")
    }
    return image
}
