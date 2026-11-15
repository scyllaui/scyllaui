# 24 小时长跑：来回 cd 的终端（开发用，docs/design.md 10.6 节「24 小时长跑方案」）。
# 在 ScyllaUI 的终端里用 source 运行，cd 发生在交互 Shell 本身，每次都会上报当前目录（覆盖「找 .git 死循环、非 Git 目录卡死」这类问题）。
# 每 30 秒换一个目录；仓库 output/longrun/stop-cd 或 stop-all 存在时停止。Ctrl+C 也可结束。
_lr_base="$HOME/scyllaui-longrun"
_lr_out="${${(%):-%x}:A:h:h:h:h}/output/longrun"
_lr_dirs=(
  "$_lr_base/repo-a"
  "$HOME"
  /
  /tmp
  "$_lr_base/plain"
  "$_lr_base/中文 目录 with space"
  "$_lr_base/repo.with.dots/deep/a/b/c"
  "$_lr_base/loop/x/x/x/x"
  /private/var/folders
  "$_lr_base/repo.with.dots"
)
while [[ ! -e "$_lr_out/stop-cd" && ! -e "$_lr_out/stop-all" ]]; do
  for _lr_d in "${_lr_dirs[@]}"; do
    [[ -e "$_lr_out/stop-cd" || -e "$_lr_out/stop-all" ]] && break
    cd "$_lr_d" 2>/dev/null && print -r -- "$(date +%H:%M:%S) cd ${PWD/#$HOME/~}"
    sleep 30
  done
done
cd "$_lr_base"
