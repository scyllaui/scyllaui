// Generates the approved ScyllaUI wordmark as vector PDFs, including pixel shadows.
// Usage: swift src/scripts/generate-wordmark.swift
//        swift src/scripts/generate-wordmark.swift cells   — only writes Resources/wordmark-cells.json for the splash
//        (stepped contours, pixel cells, shadow and tail rects, colours; M4a block 4), leaving the PDFs untouched.
import AppKit
// Plan B proportions (0.3.5): same glyphs, colours and pixel shadow; 1.3x taller, narrowed (226 pt at K = 1).
// K scales the whole drawing uniformly, so the style is unchanged: 1.18 makes the cap height 34.5 pt
// (chosen 2026-10-23).
let K: CGFloat = 1.18
let W: CGFloat = 266.75, H: CGFloat = 47.75
// Horizontal and vertical glyph units, and the pixel cell (stair steps, shadow offset and tails).
let UX: CGFloat = 1.81*K, UY: CGFloat = 1.875*1.3*K, pixel: CGFloat = 1.625*K
func color(_ hex:String)->NSColor {
    let n = UInt32(hex,radix:16)!
    return NSColor(srgbRed:CGFloat((n>>16)&255)/255,green:CGFloat((n>>8)&255)/255,blue:CGFloat(n&255)/255,alpha:1)
}
func rect(_ x:CGFloat,_ y:CGFloat,_ w:CGFloat,_ h:CGFloat)->NSRect { NSRect(x:x,y:CGFloat(H)-y-h,width:w,height:h) }
typealias Points = [(CGFloat,CGFloat)]
let contours:[(CGFloat,Points)] = [
    (0,[(0,2),(3,2),(8,7),(3,12),(0,12),(5,7)]),
    (9,[(0,11),(14,11),(14,12.5),(0,12.5)]),
    (26,[(2,0),(14,0),(14,2),(3,2),(3,4),(4,5),(11,5),(13,7),(13,10),(11,12),(0,12),(0,10),(10,10),(10,8),(9,7),(2,7),(0,5),(0,2)]),
    (42,[(2,3),(12,3),(12,5),(3,5),(2,6),(2,9),(3,10),(12,10),(12,12),(2,12),(0,10),(0,5)]),
    (56,[(0,3),(2.5,3),(2.5,9),(3.5,10),(9,10),(9,3),(11.5,3),(11.5,12),(9.5,14),(1,14),(1,12),(8,12),(9,11),(2,11),(0,9)]),
    (70,[(0,0),(2.5,0),(2.5,9),(3.5,10),(6,10),(6,12),(2,12),(0,10)]),
    (78,[(0,0),(2.5,0),(2.5,9),(3.5,10),(6,10),(6,12),(2,12),(0,10)]),
    (86,[(1,3),(10,3),(12,5),(12,12),(2,12),(0,10),(0,8),(2,6),(9.5,6),(9.5,5),(1,5)]),
    // Counter in the a is cut using even-odd fill.
    (86,[(3,8),(2.5,8.5),(2.5,9.5),(3,10),(9.5,10),(9.5,8)]),
    (103,[(0,4),(1.25,4),(1.25,10),(2,10.75),(6.5,10.75),(7.25,10),(7.25,4),(8.5,4),(8.5,10.5),(7,12),(1.5,12),(0,10.5)]),
    (114,[(0,4),(6,4),(6,5.25),(3.6,5.25),(3.6,10.75),(6,10.75),(6,12),(0,12),(0,10.75),(2.4,10.75),(2.4,5.25),(0,5.25)])
]
/// The stepped glyph outline at (x, y), and its vertices per contour (even-odd).
func glyph(_ x:CGFloat,_ y:CGFloat) -> (NSBezierPath,[[NSPoint]]) {
    let path=NSBezierPath(); path.windingRule = .evenOdd
    var polygons:[[NSPoint]] = []
    for (offset,points) in contours {
        let vertices = points.map { p in
            NSPoint(x:x+(offset+p.0+(12-p.1)*0.21)*UX,y:H-y-p.1*UY)
        }
        path.move(to: vertices[0])
        var polygon=[vertices[0]]
        for i in vertices.indices {
            let start = vertices[i], end = vertices[(i+1) % vertices.count]
            let dx = end.x-start.x, dy = end.y-start.y
            if abs(dx) < 0.01 || abs(dy) < 0.01 {
                path.line(to: end); polygon.append(end)
            } else {
                let steps = max(1, Int(ceil(max(abs(dx), abs(dy))/pixel)))
                for step in 1...steps {
                    let t = CGFloat(step)/CGFloat(steps)
                    let previous = CGFloat(step-1)/CGFloat(steps)
                    path.line(to: NSPoint(x:start.x+dx*t, y:start.y+dy*previous))
                    path.line(to: NSPoint(x:start.x+dx*t, y:start.y+dy*t))
                    polygon += [NSPoint(x:start.x+dx*t, y:start.y+dy*previous), NSPoint(x:start.x+dx*t, y:start.y+dy*t)]
                }
            }
        }
        path.close()
        polygons.append(Array(polygon.dropLast()))
    }
    return (path,polygons)
}
/// Pixel cells sampled at their centres (the shadow uses the same cells, offset two right and one down).
func cells(_ x:CGFloat,_ y:CGFloat,_ path:NSBezierPath) -> [(Int,Int)] {
    var out:[(Int,Int)] = []
    for py in 0..<Int(ceil(H/pixel)) {
        for px in 0..<Int(ceil(W/pixel)) {
            if path.contains(NSPoint(x:x+(CGFloat(px)+0.5)*pixel,y:CGFloat(H)-y-(CGFloat(py)+0.5)*pixel)) { out.append((px,py)) }
        }
    }
    return out
}
let tailEnds:[(CGFloat,CGFloat)] = [(23,12.5),(54,12),(120,12)]
func tails(_ x:CGFloat,_ y:CGFloat) -> [NSRect] {
    tailEnds.flatMap { endpoint -> [NSRect] in
        let tx=x+(endpoint.0+(12-endpoint.1)*0.21)*UX
        let ty=y+endpoint.1*UY
        return [rect(tx+2*pixel,ty+pixel,2*pixel,pixel), rect(tx+3*pixel,ty+2*pixel,pixel,pixel)]
    }
}
func mark(_ x:CGFloat,_ y:CGFloat,_ c:String) {
    let (path,_) = glyph(x,y)
    // One hard-edged shadow, offset two pixel cells right and one down.
    let shadow=NSBezierPath()
    for (px,py) in cells(x,y,path) {
        shadow.appendRect(rect(x+(CGFloat(px)+2)*pixel,y+(CGFloat(py)+1)*pixel,pixel,pixel))
    }
    let shadowColor=color(c == "237BC3" ? "154F83":"C58B6B")
    shadowColor.withAlphaComponent(c == "237BC3" ? 0.24 : 0.38).setFill(); shadow.fill()
    // Sparse single-step tails at three stroke ends, not decorative particles.
    shadowColor.withAlphaComponent(c == "237BC3" ? 0.156 : 0.247).setFill()
    for r in tails(x,y) { NSBezierPath(rect:r).fill() }
    color(c).setFill(); path.fill()
}

final class WordmarkCanvas: NSView {
    let ink: String
    init(ink: String) {
        self.ink = ink
        super.init(frame: NSRect(x: 0, y: 0, width: W, height: H))
    }
    required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }
    override func draw(_ dirtyRect: NSRect) { mark(1.25*K, 2.5*K, ink) }
}
if CommandLine.arguments.dropFirst().first == "cells" {
    let (x,y) = (1.25*K, 2.5*K)
    let (path,polygons) = glyph(x,y)
    func r(_ v:NSRect) -> [Double] { [v.minX,v.minY,v.width,v.height].map { Double($0) } }
    let json:[String:Any] = [
        "comment": "由 src/scripts/generate-wordmark.swift cells 生成，勿手改。字标空间 W × H，y 向上；格子为 [px, py, x, y]，边长 cell。",
        "size": [Double(W),Double(H)], "cell": Double(pixel),
        "contours": polygons.map { $0.map { [Double($0.x),Double($0.y)] } },
        "cells": cells(x,y,path).map { c -> [Double] in
            let v = rect(x+CGFloat(c.0)*pixel,y+CGFloat(c.1)*pixel,pixel,pixel)
            return [Double(c.0),Double(c.1),Double(v.minX),Double(v.minY)]
        },
        "shadow_offset": [2*Double(pixel),-Double(pixel)],
        "tails": tails(x,y).map(r),
        "colors": ["light": ["ink":"237BC3","shadow":"154F83","shadow_alpha":0.24,"tail_alpha":0.156],
                   "dark": ["ink":"D97757","shadow":"C58B6B","shadow_alpha":0.38,"tail_alpha":0.247]],
    ]
    let out = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent()
        .appendingPathComponent("app/ScyllaUI/Resources/wordmark-cells.json")
    try JSONSerialization.data(withJSONObject: json, options: [.sortedKeys]).write(to: out)
    print("wordmark-cells.json: \(cells(x,y,path).count) cells")
    exit(0)
}
let destination = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent()
    .appendingPathComponent("app/ScyllaUI/Brand.xcassets/ScyllaWordmark.imageset")
for (name, ink) in [("light", "237BC3"), ("dark", "D97757")] {
    let view = WordmarkCanvas(ink: ink)
    try view.dataWithPDF(inside: view.bounds).write(to: destination.appendingPathComponent("scylla-wordmark-\(name).pdf"))
}
print("ScyllaWordmark: \(W) x \(H) pt, light and dark PDFs")
