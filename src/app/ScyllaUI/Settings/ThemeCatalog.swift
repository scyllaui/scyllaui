// 设置「外观」的主题清单（M4a，docs/design.md 10.4 节决策 2）：只列 10 个配色家族，按各家族在 GitHub 上星数最多的
// 仓库排名（2026-10-23 查询），包内 657 个主题文件不删（用户的 Ghostty 配置仍可按名字使用）。有官方深浅两版的家族可选
// 「跟随系统 / 深色 / 浅色」，跟随系统写成 Ghostty 的 `light:<浅色>,dark:<深色>`。只用 Foundation，供单测直接编译。

import Foundation

struct ThemeFamily: Equatable {
    let name: String
    /// 官方浅色版在包内的主题名；只有深色版的家族为 nil。
    let light: String?
    let dark: String
}

enum ThemeMode: Equatable {
    case system, dark, light
}

/// 下拉框里的一个选择。
enum ThemeChoice: Equatable {
    /// 跟随 Ghostty 配置：不写 `theme`。
    case follow
    /// 清单中的家族（下标）与深浅。
    case family(Int, ThemeMode)
    /// 不在清单中的原有设置（M2 起可从 657 个里选），原样保留，换选后才覆盖。
    case other(String)
}

enum ThemeCatalog {
    static let families: [ThemeFamily] = [
        ThemeFamily(name: "Dracula", light: nil, dark: "Dracula"),
        ThemeFamily(name: "Catppuccin", light: "Catppuccin Latte", dark: "Catppuccin Mocha"),
        ThemeFamily(name: "Solarized", light: "iTerm2 Solarized Light", dark: "iTerm2 Solarized Dark"),
        ThemeFamily(name: "Gruvbox", light: "Gruvbox Light", dark: "Gruvbox Dark"),
        ThemeFamily(name: "Tomorrow", light: "Tomorrow", dark: "Tomorrow Night"),
        ThemeFamily(name: "Tokyo Night", light: "TokyoNight Day", dark: "TokyoNight"),
        ThemeFamily(name: "Nord", light: nil, dark: "Nord"),
        ThemeFamily(name: "Kanagawa", light: "Kanagawa Lotus", dark: "Kanagawa Wave"),
        ThemeFamily(name: "Everforest", light: "Everforest Light Med", dark: "Everforest Dark Med"),
        ThemeFamily(name: "Nightfox", light: "Dayfox", dark: "Nightfox"),
    ]

    /// `theme` 的值 → 选择。认得 Ghostty 的 `light:A,dark:B`（逗号与冒号两侧可有空白）。
    static func choice(for value: String?) -> ThemeChoice {
        guard let raw = value?.trimmingCharacters(in: .whitespaces), !raw.isEmpty else { return .follow }
        if let (light, dark) = pair(raw) {
            if let i = families.firstIndex(where: { $0.light == light && $0.dark == dark }) { return .family(i, .system) }
            return .other(raw)
        }
        if let i = families.firstIndex(where: { $0.dark == raw }) { return .family(i, .dark) }
        if let i = families.firstIndex(where: { $0.light == raw }) { return .family(i, .light) }
        return .other(raw)
    }

    /// 选择 → `theme` 的值；nil 表示不写。只有深色版的家族不论深浅都写深色版。
    static func value(for choice: ThemeChoice) -> String? {
        switch choice {
        case .follow: return nil
        case .other(let v): return v
        case .family(let i, let mode):
            let f = families[i]
            guard let light = f.light else { return f.dark }
            switch mode {
            case .system: return "light:\(light),dark:\(f.dark)"
            case .dark: return f.dark
            case .light: return light
            }
        }
    }

    private static func pair(_ v: String) -> (String, String)? {
        let parts = v.split(separator: ",", omittingEmptySubsequences: false)
        guard parts.count == 2 else { return nil }
        var light: String?, dark: String?
        for p in parts {
            let kv = p.split(separator: ":", maxSplits: 1).map { $0.trimmingCharacters(in: .whitespaces) }
            guard kv.count == 2 else { return nil }
            switch kv[0] {
            case "light": light = kv[1]
            case "dark": dark = kv[1]
            default: return nil
            }
        }
        guard let light, let dark else { return nil }
        return (light, dark)
    }
}

/// ScyllaUI 自己的外观覆盖文件 `<数据目录>/theme.conf`（文件名沿用 M2，内容为主题、终端字体与字号），在用户的 Ghostty
/// 配置之后加载；「跟随 Ghostty 配置」的项不写（docs/design.md 10.4 节决策 2）。
struct AppearanceOverrides: Equatable {
    var theme: String?
    var fontFamily: String?
    var fontSize: Double?

    /// 读 `key = value`，值两侧的引号去掉。`font-family` 是 Ghostty 的可重复项，取最后一个非空值（写入时先写空值清掉
    /// 用户配置里的设置）。
    static func parse(_ text: String) -> AppearanceOverrides {
        var o = AppearanceOverrides()
        for line in text.split(whereSeparator: \.isNewline) {
            let s = line.trimmingCharacters(in: .whitespaces)
            guard !s.hasPrefix("#") else { continue }
            let kv = s.split(separator: "=", maxSplits: 1).map { $0.trimmingCharacters(in: .whitespaces) }
            guard kv.count == 2 else { continue }
            var v = kv[1]
            if v.count >= 2, v.hasPrefix("\""), v.hasSuffix("\"") { v = String(v.dropFirst().dropLast()) }
            switch kv[0] {
            case "theme": o.theme = v.isEmpty ? nil : v
            case "font-family": if !v.isEmpty { o.fontFamily = v }
            case "font-size": o.fontSize = Double(v)
            default: break
            }
        }
        return o
    }

    func render() -> String {
        var text = "# ScyllaUI 设置中的外观覆盖（主题、终端字体与字号），在用户的 Ghostty 配置之后加载；「跟随 Ghostty 配置」的项不写。\n"
        if let theme { text += "theme = \"\(theme)\"\n" }
        if let fontFamily {
            text += "font-family = \"\"\n"
            text += "font-family = \"\(fontFamily)\"\n"
        }
        if let fontSize { text += "font-size = \(Self.number(fontSize))\n" }
        return text
    }

    /// 整数不带小数点。
    static func number(_ v: Double) -> String {
        v == v.rounded() ? String(Int(v)) : String(v)
    }
}
