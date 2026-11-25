// 右侧栏的自动刷新（docs/design.md 5.12 节）：对项目根开一条 FSEvents 流（系统接口），延迟 1 秒合并一批变化，在主线程报给
// 工作区。报的是有变化的目录（目录级事件）。右侧栏开着「文件」或「Diff」时才开，关栏、换根就停。

import CoreServices
import Foundation

final class FolderWatch {
    let root: String
    private var stream: FSEventStreamRef?
    private let onChange: ([String]) -> Void

    init?(root: String, onChange: @escaping ([String]) -> Void) {
        self.root = root
        self.onChange = onChange
        var context = FSEventStreamContext(version: 0, info: nil, retain: nil, release: nil, copyDescription: nil)
        context.info = Unmanaged.passUnretained(self).toOpaque()
        let callback: FSEventStreamCallback = { _, info, count, paths, _, _ in
            guard let info else { return }
            let me = Unmanaged<FolderWatch>.fromOpaque(info).takeUnretainedValue()
            let list = Unmanaged<CFArray>.fromOpaque(paths).takeUnretainedValue() as? [String] ?? []
            me.onChange(Array(list.prefix(count)))
        }
        guard let s = FSEventStreamCreate(kCFAllocatorDefault, callback, &context, [root] as CFArray,
                                          FSEventStreamEventId(kFSEventStreamEventIdSinceNow), 1.0,
                                          FSEventStreamCreateFlags(kFSEventStreamCreateFlagUseCFTypes)) else { return nil }
        stream = s
        FSEventStreamSetDispatchQueue(s, .main)
        FSEventStreamStart(s)
    }

    func stop() {
        guard let s = stream else { return }
        stream = nil
        FSEventStreamStop(s)
        FSEventStreamInvalidate(s)
        FSEventStreamRelease(s)
    }

    deinit { stop() }
}
