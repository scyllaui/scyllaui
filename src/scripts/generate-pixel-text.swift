// Generates Resources/pixel-text.json: the pixel cells of the splash's pixel lettering (subtitle, button, footer).
// Source font: Fusion Pixel Font 12px proportional zh_hans (TakWolf, OFL-1.1), release 2026.10.25,
// https://github.com/TakWolf/fusion-pixel-font/releases/download/2026.10.25/fusion-pixel-font-12px-proportional-otf-v2026.10.25.zip
// (fusion-pixel-12px-proportional-zh_hans.otf, SHA-256 2972782f05d7dd5f8414bc0215f69c181561172286b1f9823b5c279d9e5e6674).
// Only the characters below are kept, so the app ships a few kilobytes instead of the 5 MB font and never registers it.
// Usage: swift src/scripts/generate-pixel-text.swift <path to the .otf>
import CoreText
import Foundation

// Printable ASCII (subtitle, English button, version, copyright line) plus the symbols and the Chinese button label.
let charset = (0x20...0x7E).map { String(UnicodeScalar($0)!) } + ["©", "·", "→", "开", "始", "使", "用"]
let fontURL = URL(fileURLWithPath: CommandLine.arguments[1]) as CFURL
guard let descriptor = (CTFontManagerCreateFontDescriptorsFromURL(fontURL) as? [CTFontDescriptor])?.first else {
    fatalError("cannot read font")
}
// The font is drawn on a 12-unit grid, so at 12 pt one point is one pixel cell.
let font = CTFontCreateWithFontDescriptor(descriptor, 12, nil)
let ascent = Int(CTFontGetAscent(font).rounded()), descent = Int(CTFontGetDescent(font).rounded())
let pad = 4, height = ascent + descent + 2 * pad

var glyphs: [String: [Any]] = [:]
for ch in charset {
    var units = Array(ch.utf16), glyph = [CGGlyph](repeating: 0, count: units.count)
    guard CTFontGetGlyphsForCharacters(font, &units, &glyph, units.count) else { fatalError("font has no glyph for \(ch)") }
    var advance = CGSize.zero
    CTFontGetAdvancesForGlyphs(font, .horizontal, &glyph, &advance, 1)
    let width = Int(advance.width.rounded()) + 2 * pad
    let ctx = CGContext(data: nil, width: width, height: height, bitsPerComponent: 8, bytesPerRow: width,
                        space: CGColorSpaceCreateDeviceGray(), bitmapInfo: CGImageAlphaInfo.none.rawValue)!
    ctx.setShouldAntialias(false)
    ctx.setAllowsFontSmoothing(false)
    ctx.setFillColor(gray: 1, alpha: 1)
    var position = CGPoint(x: pad, y: pad + descent)
    CTFontDrawGlyphs(font, &glyph, &position, 1, ctx)
    let data = ctx.data!.assumingMemoryBound(to: UInt8.self)
    // Cells as flat [x, y, x, y, …]: x from the pen position, y up from the baseline.
    var cells: [Int] = []
    for row in 0..<height {
        for x in 0..<width where data[row * width + x] > 127 {
            cells += [x - pad, height - 1 - row - pad - descent]
        }
    }
    glyphs[ch] = [Int(advance.width.rounded()), cells]
}
let json: [String: Any] = [
    "comment": "由 src/scripts/generate-pixel-text.swift 生成，勿手改。Fusion Pixel Font 12px（OFL-1.1）的点阵；glyphs 为 [前进宽度, [x, y, …]]，单位为字体像素，y 自基线向上。",
    "ascent": ascent, "descent": descent, "glyphs": glyphs,
]
let out = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent()
    .appendingPathComponent("app/ScyllaUI/Resources/pixel-text.json")
try JSONSerialization.data(withJSONObject: json, options: [.sortedKeys]).write(to: out)
print("pixel-text.json: \(glyphs.count) glyphs, ascent \(ascent), descent \(descent)")
