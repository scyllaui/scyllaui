// 上次意外退出的提示（M4b 块 7，docs/design.md 10.6 节「块 7 实现」）：启动时读到上次留下的心跳即上次没走正常退出
// （`applicationWillTerminate` 会删掉它）。判定与正文是纯函数，可单测。

import Foundation

struct UnexpectedExit: Equatable {
    /// 第一份快照里终端的去向。
    enum Terminals: Equatable {
        /// 后台仍保管着终端，已经接回。
        case held
        /// 后台也已结束，有丢失的终端，按设置恢复。
        case lost
        /// 没有终端。
        case none
    }

    /// 上次最后一次心跳的时间。
    let lastAlive: Date
    let terminals: Terminals

    /// `lastAlive`：启动时读到的上次心跳，没有即上次正常退出（或第一次运行），不提示。
    init?(lastAlive: Date?, held: Int, lost: Int) {
        guard let lastAlive else { return nil }
        self.lastAlive = lastAlive
        terminals = held > 0 ? .held : lost > 0 ? .lost : .none
    }

    func title(formatter: (Date) -> String = { $0.formatted(date: .abbreviated, time: .shortened) }) -> String {
        String(localized: "ScyllaUI 上次意外退出（约 \(formatter(lastAlive))）")
    }

    var message: String {
        let report = String(localized: "反馈时点「报告问题…」，诊断包会带上日志、崩溃报告和这个时间。")
        switch terminals {
        case .held: return String(localized: "终端和任务都在后台保管着，已经接回。") + "\n\n" + report
        case .lost: return String(localized: "后台也已结束，终端按设置恢复。") + "\n\n" + report
        case .none: return report
        }
    }
}
