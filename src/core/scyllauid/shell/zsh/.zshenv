# ScyllaUI 终端里登录 zsh 的启动入口（M4b 块 5 P3，docs/design.md 10.6 节 D11）：scyllauid 把 ZDOTDIR 指到这里，
# 用户原来的 ZDOTDIR 在 GHOSTTY_ZSH_ZDOTDIR（Ghostty 的约定）。先原样执行同一个包里 Ghostty 的 .zshenv
# （还原 ZDOTDIR、读用户的 .zshenv、加载 Ghostty 的 Shell 集成），再在交互 Shell 里定义 codex、opencode 函数。
# 本文件可能在别名开着时被读：能加引号的都加，命令都加 builtin（同 Ghostty 的 .zshenv）。

# ${(%):-%x} 是本文件的路径，:A:h 取所在目录（包内 Contents/Resources/scyllaui-shell/zsh）。
'builtin' 'source' '--' "${${(%):-%x}:A:h}/../../ghostty/shell-integration/zsh/.zshenv"

# Codex 0.164 的交互界面默认连共享后台，Hook 在后台里执行、拿不到 SCYLLAUI_*，侧栏只能显示「未知」：补 --no-daemon，
# 让 Hook 在这个终端的 Codex 进程里执行。Codex 拒绝或用不到它的用法（agents、queue、--remote 等）原样运行。
# 用户的 .zshrc 在这之后读，自己定义了 codex 函数或别名时以用户的为准；想连共享后台用 `command codex`。
if [[ -o 'interactive' ]]; then
    function codex {
        'builtin' 'local' _scyllaui_arg
        for _scyllaui_arg in "$@"; do
            case "$_scyllaui_arg" in
                (agents|queue|remote-control|app-server|exec-server|--no-daemon|--remote|--remote=*)
                    'builtin' 'command' 'codex' "$@"
                    'builtin' 'return' ;;
            esac
        done
        'builtin' 'command' 'codex' '--no-daemon' "$@"
    }

    # OpenCode 2 默认连每个用户一个的共享后台，插件在后台里运行、拿不到这个终端的 SCYLLAUI_*（2026-10-31 实测 2.2.4）：
    # 交互界面与 run、mini 补 --standalone，用这个终端的私有服务。OpenCode 1 没有这个参数，原样运行；其他子命令、
    # 已带 --standalone 或 --server 的也原样运行。想连共享后台用 `command opencode`。
    function opencode {
        'builtin' 'local' _scyllaui_arg
        for _scyllaui_arg in "$@"; do
            case "$_scyllaui_arg" in
                (--standalone|--server|--server=*)
                    'builtin' 'command' 'opencode' "$@"
                    'builtin' 'return' ;;
            esac
        done
        case "$('builtin' 'command' 'opencode' '--version' 2>/dev/null)" in
            (*v[2-9].*|[2-9].*) ;;
            (*)
                'builtin' 'command' 'opencode' "$@"
                'builtin' 'return' ;;
        esac
        case "${1-}" in
            (run|mini)
                _scyllaui_arg="$1"
                'builtin' 'shift'
                'builtin' 'command' 'opencode' "$_scyllaui_arg" '--standalone' "$@" ;;
            (''|-*)
                'builtin' 'command' 'opencode' '--standalone' "$@" ;;
            (*)
                if [[ -d "$1" ]]; then
                    'builtin' 'command' 'opencode' '--standalone' "$@"
                else
                    'builtin' 'command' 'opencode' "$@"
                fi ;;
        esac
    }
fi
