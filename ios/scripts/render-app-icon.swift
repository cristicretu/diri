// Render the existing Diri terminal mark as an opaque, full-bleed iOS icon.
// iOS supplies the outer mask; do not bake macOS-style rounded corners into it.
// Run from ios/: xcrun swift scripts/render-app-icon.swift
import AppKit
import ImageIO
import UniformTypeIdentifiers

let size = 1024
let context = CGContext(
  data: nil, width: size, height: size, bitsPerComponent: 8,
  bytesPerRow: 0, space: CGColorSpace(name: CGColorSpace.sRGB)!,
  bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)!
context.setFillColor(NSColor(srgbRed: 0.07, green: 0.08, blue: 0.10, alpha: 1).cgColor)
context.fill(CGRect(x: 0, y: 0, width: size, height: size))
context.setStrokeColor(NSColor(srgbRed: 0.84, green: 0.46, blue: 0.32, alpha: 1).cgColor)
context.setLineCap(.round)
context.setLineJoin(.round)
context.setLineWidth(76)
context.move(to: CGPoint(x: 310, y: 650))
context.addLine(to: CGPoint(x: 490, y: 512))
context.addLine(to: CGPoint(x: 310, y: 374))
context.strokePath()
context.move(to: CGPoint(x: 565, y: 374))
context.addLine(to: CGPoint(x: 745, y: 374))
context.strokePath()
let destination = URL(fileURLWithPath: "DiriPhone/Assets.xcassets/AppIcon.appiconset/AppIcon.png")
let imageDestination = CGImageDestinationCreateWithURL(
  destination as CFURL, UTType.png.identifier as CFString, 1, nil)!
CGImageDestinationAddImage(imageDestination, context.makeImage()!, nil)
guard CGImageDestinationFinalize(imageDestination) else { fatalError("Could not write icon") }
print("Rendered opaque 1024px Diri app icon.")
