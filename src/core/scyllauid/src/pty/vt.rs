//! libghostty-vt（固定 Ghostty commit 的 C API，`include/ghostty/vt/*.h`）的最小手写绑定与重连快照。
//! 移植自 S0 的 `keepd/src/vt.rs`（M3，docs/design.md 第 7 节「实现（M3）」）。与 S0 的差别：加了标题、OSC 7、
//! OSC 133 回调与「在提示符」读取（前台读取与关闭确认改由后台触发与判定），配色与回滚上限可由 App 设置
//! （`appearance.set`），快照自检不一致时只记日志、不把终端内容写到磁盘。
//!
//! 快照顺序与上游 formatter 的差别：上游把模式序列放在正文之前（`src/terminal/formatter.zig`
//! 第 408 行注释承认未细分），回放时关闭自动换行、插入模式等会破坏正文。这里改为
//! 「RIS →（备用屏时：主屏的同一套内容 → ?1049h）→ 调色板 / 制表位 / 正文 / 滚动区 / 键盘 / pwd → 模式 →
//! 光标等屏幕状态 → 标题 → 未完成序列」。模式与屏幕状态由多次格式化的前后缀相减取得（formatter 没有
//! 「只输出状态」选项）。formatter 只输出活动屏，主屏内容取自二进制 snapshot 复制出的副本切回主屏后的结果。

use std::ffi::c_void;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};

type Term = *mut c_void;
type Formatter = *mut c_void;
type GResult = i32;
const SUCCESS: GResult = 0;

#[repr(C)]
#[derive(Clone, Copy)]
struct GString {
    ptr: *const u8,
    len: usize,
}

#[repr(C)]
struct Writer {
    write: extern "C" fn(*mut c_void, *const u8, usize) -> bool,
    userdata: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ScreenExtra {
    size: usize,
    cursor: bool,
    style: bool,
    hyperlink: bool,
    protection: bool,
    kitty_keyboard: bool,
    charsets: bool,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct TerminalExtra {
    size: usize,
    palette: bool,
    modes: bool,
    scrolling_region: bool,
    tabstops: bool,
    pwd: bool,
    keyboard: bool,
    screen: ScreenExtra,
}

#[repr(C)]
struct FormatterOptions {
    size: usize,
    emit: i32,
    unwrap: bool,
    trim: bool,
    extra: TerminalExtra,
    selection: *const c_void,
}

#[repr(C)]
struct SizeReport {
    rows: u16,
    columns: u16,
    cell_width: u32,
    cell_height: u32,
}

#[repr(C)]
struct DaPrimary {
    conformance_level: u16,
    features: [u16; 64],
    num_features: usize,
}

#[repr(C)]
struct DaSecondary {
    device_type: u16,
    firmware_version: u16,
    rom_cartridge: u16,
}

#[repr(C)]
struct DaTertiary {
    unit_id: u32,
}

#[repr(C)]
struct DeviceAttributes {
    primary: DaPrimary,
    secondary: DaSecondary,
    tertiary: DaTertiary,
}

#[repr(C)]
struct Rgb {
    r: u8,
    g: u8,
    b: u8,
}

#[repr(C)]
struct SemanticPromptEvent {
    size: usize,
    kind: i32,
    prompt_kind: i32,
    has_exit_code: bool,
    exit_code: i32,
    command: GString,
    error: GString,
}

#[cfg(test)]
#[repr(C)]
struct ModeConfig {
    mode: u16,
    value: bool,
}

extern "C" {
    fn ghostty_terminal_new(alloc: *const c_void, out: *mut Term, cols: u16, rows: u16) -> GResult;
    fn ghostty_terminal_free(t: Term);
    fn ghostty_terminal_resize(t: Term, cols: u16, rows: u16, cw: u32, ch: u32) -> GResult;
    fn ghostty_terminal_set(t: Term, option: i32, value: *const c_void) -> GResult;
    fn ghostty_terminal_vt_write(t: Term, data: *const u8, len: usize);
    fn ghostty_terminal_get(t: Term, data: i32, out: *mut c_void) -> GResult;
    fn ghostty_terminal_continuation_write(t: Term, writer: Writer) -> GResult;
    fn ghostty_formatter_terminal_new(
        alloc: *const c_void,
        out: *mut Formatter,
        t: Term,
        options: FormatterOptions,
    ) -> GResult;
    fn ghostty_formatter_format(f: Formatter, writer: Writer) -> GResult;
    fn ghostty_formatter_free(f: Formatter);
    fn ghostty_snapshot_encode_alloc(
        t: Term,
        alloc: *const c_void,
        out_ptr: *mut *mut u8,
        out_len: *mut usize,
    ) -> GResult;
    fn ghostty_snapshot_decoder_new_buf(
        alloc: *const c_void,
        out: *mut *mut c_void,
        ptr: *const u8,
        len: usize,
    ) -> GResult;
    fn ghostty_snapshot_decoder_decode(decoder: *mut c_void, out: *mut Term) -> GResult;
    fn ghostty_snapshot_decoder_free(decoder: *mut c_void);
    fn ghostty_free(alloc: *const c_void, ptr: *mut u8, len: usize);
}

// GhosttyTerminalOption（terminal.h）
const OPT_USERDATA: i32 = 0;
const OPT_WRITE_PTY: i32 = 1;
const OPT_XTVERSION: i32 = 4;
const OPT_TITLE_CHANGED: i32 = 5;
const OPT_SIZE: i32 = 6;
const OPT_COLOR_SCHEME: i32 = 7;
const OPT_DEVICE_ATTRIBUTES: i32 = 8;
const OPT_COLOR_FOREGROUND: i32 = 11;
const OPT_COLOR_BACKGROUND: i32 = 12;
const OPT_PWD_CHANGED: i32 = 25;
const OPT_SCROLLBACK_MAX_BYTES: i32 = 27;
const OPT_CONTINUATION_MAX_BYTES: i32 = 31;
const OPT_TERMINFO_NAME: i32 = 37;
const OPT_SEMANTIC_PROMPT: i32 = 42;
// GhosttySemanticPromptKind
const SEMANTIC_PROMPT_START: i32 = 1;
const SEMANTIC_OUTPUT_START: i32 = 3;
const SEMANTIC_COMMAND_END: i32 = 4;
// GhosttyTerminalData
const DATA_CURSOR_X: i32 = 3;
const DATA_CURSOR_Y: i32 = 4;
const DATA_ACTIVE_SCREEN: i32 = 6;
#[cfg(test)]
const DATA_KITTY_KEYBOARD_FLAGS: i32 = 8;
const DATA_TITLE: i32 = 12;
const DATA_PWD: i32 = 13;
const DATA_TOTAL_ROWS: i32 = 14;
const DATA_SCROLLBACK_ROWS: i32 = 15;
#[cfg(test)]
const DATA_MODE: i32 = 37;
const DATA_CURSOR_AT_PROMPT: i32 = 39;
// GhosttyFormatterFormat
const FORMAT_PLAIN: i32 = 0;
const FORMAT_VT: i32 = 1;

/// XTVERSION 应答，与同 commit 的 Ghostty Surface 一致（`src/termio/stream_handler.zig` reportXtversion：
/// `ghostty <version_string>`；`ghostty +version` 实测为 1.3.2-HEAD+34f3900）。
const XTVERSION: &[u8] = b"ghostty 1.3.2-HEAD+34f3900";

/// 回滚上限：固定 commit 的 Ghostty 默认值（`Config.zig` scrollback-limit-bytes，50 MB，与 Surface 一致）。
pub const DEFAULT_SCROLLBACK: usize = 50_000_000;

/// 查询应答的去向与尺寸等，供 libghostty-vt 回调读取。回调在 `vt_write` 内同步执行（调用方持有会话锁）。
/// 标题、目录、命令结束只置标志，由调用方在写完之后取走并通知核心线程。
pub struct Sink {
    pub master: RawFd,
    /// 有客户端连接时，应答由客户端的终端（Surface）给出，这里丢弃 vt 的应答，保证同一查询只答一次。
    pub attached: AtomicBool,
    pub cols: AtomicU32,
    pub rows: AtomicU32,
    pub cell_w: AtomicU32,
    pub cell_h: AtomicU32,
    /// 0 未知（不应答），1 浅色，2 深色。
    pub color_scheme: AtomicU8,
    pub detached_replies: AtomicU64,
    pub dropped_replies: AtomicU64,
    pub title_changed: AtomicBool,
    pub pwd_changed: AtomicBool,
    pub command_end: AtomicBool,
    /// 提示符开始（OSC 133;A）与命令开始执行（OSC 133;C），供新终端的启动计时（mod.rs `LaunchTrace`）。
    pub prompt_start: AtomicBool,
    pub output_start: AtomicBool,
}

impl Sink {
    pub fn new(master: RawFd) -> Sink {
        Sink {
            master,
            attached: AtomicBool::new(false),
            cols: AtomicU32::new(80),
            rows: AtomicU32::new(24),
            cell_w: AtomicU32::new(0),
            cell_h: AtomicU32::new(0),
            color_scheme: AtomicU8::new(0),
            detached_replies: AtomicU64::new(0),
            dropped_replies: AtomicU64::new(0),
            title_changed: AtomicBool::new(false),
            pwd_changed: AtomicBool::new(false),
            command_end: AtomicBool::new(false),
            prompt_start: AtomicBool::new(false),
            output_start: AtomicBool::new(false),
        }
    }
}

extern "C" fn on_write_pty(_t: Term, ud: *mut c_void, data: *const u8, len: usize) {
    let sink = unsafe { &*(ud as *const Sink) };
    let bytes = unsafe { std::slice::from_raw_parts(data, len) };
    if sink.attached.load(Ordering::Acquire) {
        sink.dropped_replies.fetch_add(1, Ordering::Relaxed);
        return;
    }
    sink.detached_replies.fetch_add(1, Ordering::Relaxed);
    if sink.master >= 0 {
        write_all_fd(sink.master, bytes);
    }
}

extern "C" fn on_title_changed(_t: Term, ud: *mut c_void) {
    let sink = unsafe { &*(ud as *const Sink) };
    sink.title_changed.store(true, Ordering::Relaxed);
}

extern "C" fn on_pwd_changed(_t: Term, ud: *mut c_void) {
    let sink = unsafe { &*(ud as *const Sink) };
    sink.pwd_changed.store(true, Ordering::Relaxed);
}

extern "C" fn on_semantic_prompt(_t: Term, ud: *mut c_void, ev: *const SemanticPromptEvent) {
    let sink = unsafe { &*(ud as *const Sink) };
    match unsafe { (*ev).kind } {
        SEMANTIC_PROMPT_START => sink.prompt_start.store(true, Ordering::Relaxed),
        SEMANTIC_OUTPUT_START => sink.output_start.store(true, Ordering::Relaxed),
        SEMANTIC_COMMAND_END => sink.command_end.store(true, Ordering::Relaxed),
        _ => {}
    }
}

extern "C" fn on_xtversion(_t: Term, _ud: *mut c_void) -> GString {
    GString {
        ptr: XTVERSION.as_ptr(),
        len: XTVERSION.len(),
    }
}

extern "C" fn on_size(_t: Term, ud: *mut c_void, out: *mut SizeReport) -> bool {
    let sink = unsafe { &*(ud as *const Sink) };
    unsafe {
        *out = SizeReport {
            rows: sink.rows.load(Ordering::Relaxed) as u16,
            columns: sink.cols.load(Ordering::Relaxed) as u16,
            cell_width: sink.cell_w.load(Ordering::Relaxed),
            cell_height: sink.cell_h.load(Ordering::Relaxed),
        };
    }
    true
}

extern "C" fn on_color_scheme(_t: Term, ud: *mut c_void, out: *mut i32) -> bool {
    let sink = unsafe { &*(ud as *const Sink) };
    match sink.color_scheme.load(Ordering::Relaxed) {
        1 => unsafe { *out = 0 },
        2 => unsafe { *out = 1 },
        _ => return false,
    }
    true
}

/// 与 Ghostty Surface 相同（`stream_handler.zig` deviceAttributes，clipboard-write 默认 allow）：
/// DA1 `?62;22;52c`，DA2 `>1;10;0c`。Surface 不应答 DA3，libghostty-vt 会应答，差异记入实验记录。
extern "C" fn on_device_attributes(_t: Term, _ud: *mut c_void, out: *mut DeviceAttributes) -> bool {
    let mut features = [0u16; 64];
    features[0] = 22;
    features[1] = 52;
    unsafe {
        *out = DeviceAttributes {
            primary: DaPrimary {
                conformance_level: 62,
                features,
                num_features: 2,
            },
            secondary: DaSecondary {
                device_type: 1,
                firmware_version: 10,
                rom_cartridge: 0,
            },
            tertiary: DaTertiary { unit_id: 0 },
        };
    }
    true
}

extern "C" fn collect(ud: *mut c_void, data: *const u8, len: usize) -> bool {
    let out = unsafe { &mut *(ud as *mut Vec<u8>) };
    out.extend_from_slice(unsafe { std::slice::from_raw_parts(data, len) });
    true
}

const SCREEN_ALL: ScreenExtra = ScreenExtra {
    size: 0,
    cursor: true,
    style: true,
    hyperlink: true,
    protection: true,
    kitty_keyboard: true,
    charsets: true,
};

pub struct Vt {
    t: Term,
}

// 只在会话互斥锁内使用。
unsafe impl Send for Vt {}

pub struct Snapshot {
    pub bytes: Vec<u8>,
    pub alt_screen: bool,
    pub scrollback_rows: usize,
    /// 把快照回放到同尺寸的临时 vt，纯文本与光标是否与原终端一致。
    pub selfcheck_ok: bool,
    /// 模式或屏幕状态的前后缀相减不成立时退回上游顺序（应不发生，发生即记入日志）。
    pub fallback: bool,
}

impl Vt {
    /// `sink` 必须比 Vt 活得久（会话里用 Box 固定地址）。
    pub fn new(cols: u16, rows: u16, sink: *const Sink) -> Vt {
        let mut t: Term = std::ptr::null_mut();
        let rc = unsafe { ghostty_terminal_new(std::ptr::null(), &mut t, cols, rows) };
        assert!(
            rc == SUCCESS && !t.is_null(),
            "ghostty_terminal_new 失败：{rc}"
        );
        unsafe {
            ghostty_terminal_set(t, OPT_USERDATA, sink as *const c_void);
            ghostty_terminal_set(t, OPT_WRITE_PTY, on_write_pty as *const c_void);
            ghostty_terminal_set(t, OPT_XTVERSION, on_xtversion as *const c_void);
            ghostty_terminal_set(t, OPT_TITLE_CHANGED, on_title_changed as *const c_void);
            ghostty_terminal_set(t, OPT_PWD_CHANGED, on_pwd_changed as *const c_void);
            ghostty_terminal_set(t, OPT_SEMANTIC_PROMPT, on_semantic_prompt as *const c_void);
            ghostty_terminal_set(t, OPT_SIZE, on_size as *const c_void);
            ghostty_terminal_set(t, OPT_COLOR_SCHEME, on_color_scheme as *const c_void);
            ghostty_terminal_set(
                t,
                OPT_DEVICE_ATTRIBUTES,
                on_device_attributes as *const c_void,
            );
            let max: usize = 1 << 20;
            ghostty_terminal_set(
                t,
                OPT_CONTINUATION_MAX_BYTES,
                &max as *const usize as *const c_void,
            );
            let name = b"xterm-ghostty";
            let s = GString {
                ptr: name.as_ptr(),
                len: name.len(),
            };
            ghostty_terminal_set(t, OPT_TERMINFO_NAME, &s as *const GString as *const c_void);
        }
        let mut vt = Vt { t };
        // Ghostty 默认配色（`src/config/Config.zig`：background #282C34，foreground #FFFFFF），供 OSC 10/11 查询应答；
        // App 用 `appearance.set` 下发实际值后覆盖。
        vt.set_colors([0xff, 0xff, 0xff], [0x28, 0x2c, 0x34]);
        vt.set_scrollback(DEFAULT_SCROLLBACK);
        vt
    }

    pub fn set_colors(&mut self, fg: [u8; 3], bg: [u8; 3]) {
        let fg = Rgb { r: fg[0], g: fg[1], b: fg[2] };
        let bg = Rgb { r: bg[0], g: bg[1], b: bg[2] };
        unsafe {
            ghostty_terminal_set(self.t, OPT_COLOR_FOREGROUND, &fg as *const Rgb as *const c_void);
            ghostty_terminal_set(self.t, OPT_COLOR_BACKGROUND, &bg as *const Rgb as *const c_void);
        }
    }

    pub fn set_scrollback(&mut self, bytes: usize) {
        unsafe {
            ghostty_terminal_set(
                self.t,
                OPT_SCROLLBACK_MAX_BYTES,
                &bytes as *const usize as *const c_void,
            )
        };
    }

    /// 光标是否在 Shell 的提示符或输入区（OSC 133；备用屏一律为 false），即 Ghostty `cursorIsAtPrompt`。
    pub fn cursor_at_prompt(&self) -> bool {
        let mut v = false;
        unsafe {
            ghostty_terminal_get(
                self.t,
                DATA_CURSOR_AT_PROMPT,
                &mut v as *mut bool as *mut c_void,
            )
        };
        v
    }

    /// OSC 7 等设置的工作目录原文（未解码的 URL），没有时为空。
    pub fn pwd(&self) -> String {
        let mut s = GString { ptr: std::ptr::null(), len: 0 };
        let rc = unsafe { ghostty_terminal_get(self.t, DATA_PWD, &mut s as *mut GString as *mut c_void) };
        if rc != SUCCESS || s.ptr.is_null() {
            return String::new();
        }
        String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(s.ptr, s.len) }).into_owned()
    }

    pub fn write(&mut self, data: &[u8]) {
        unsafe { ghostty_terminal_vt_write(self.t, data.as_ptr(), data.len()) }
    }

    pub fn resize(&mut self, cols: u16, rows: u16, cell_w: u32, cell_h: u32) {
        unsafe { ghostty_terminal_resize(self.t, cols.max(1), rows.max(1), cell_w, cell_h) };
    }

    pub fn alt_screen(&self) -> bool {
        let mut v: i32 = 0;
        unsafe {
            ghostty_terminal_get(
                self.t,
                DATA_ACTIVE_SCREEN,
                &mut v as *mut i32 as *mut c_void,
            )
        };
        v == 1
    }

    pub fn cursor(&self) -> (u16, u16) {
        let (mut x, mut y) = (0u16, 0u16);
        unsafe {
            ghostty_terminal_get(self.t, DATA_CURSOR_X, &mut x as *mut u16 as *mut c_void);
            ghostty_terminal_get(self.t, DATA_CURSOR_Y, &mut y as *mut u16 as *mut c_void);
        }
        (x, y)
    }

    #[cfg(test)]
    pub fn kitty_flags(&self) -> u8 {
        let mut v = 0u8;
        unsafe {
            ghostty_terminal_get(
                self.t,
                DATA_KITTY_KEYBOARD_FLAGS,
                &mut v as *mut u8 as *mut c_void,
            )
        };
        v
    }

    #[cfg(test)]
    /// `mode` 按 `ghostty_mode_new` 编码：低 15 位为编号，最高位为 ANSI 标志。
    pub fn mode(&self, mode: u16) -> bool {
        let mut cfg = ModeConfig { mode, value: false };
        unsafe {
            ghostty_terminal_get(
                self.t,
                DATA_MODE,
                &mut cfg as *mut ModeConfig as *mut c_void,
            )
        };
        cfg.value
    }

    pub fn title(&self) -> Vec<u8> {
        let mut s = GString {
            ptr: std::ptr::null(),
            len: 0,
        };
        let rc = unsafe {
            ghostty_terminal_get(self.t, DATA_TITLE, &mut s as *mut GString as *mut c_void)
        };
        if rc != SUCCESS || s.ptr.is_null() {
            return Vec::new();
        }
        unsafe { std::slice::from_raw_parts(s.ptr, s.len) }.to_vec()
    }

    fn total_rows(&self) -> usize {
        let mut v = 0usize;
        unsafe {
            ghostty_terminal_get(self.t, DATA_TOTAL_ROWS, &mut v as *mut usize as *mut c_void)
        };
        v
    }

    pub fn scrollback_rows(&self) -> usize {
        let mut v = 0usize;
        unsafe {
            ghostty_terminal_get(
                self.t,
                DATA_SCROLLBACK_ROWS,
                &mut v as *mut usize as *mut c_void,
            )
        };
        v
    }

    fn format(&self, emit: i32, extra: TerminalExtra) -> Vec<u8> {
        let mut extra = extra;
        extra.size = std::mem::size_of::<TerminalExtra>();
        extra.screen.size = std::mem::size_of::<ScreenExtra>();
        let opts = FormatterOptions {
            size: std::mem::size_of::<FormatterOptions>(),
            emit,
            unwrap: true,
            trim: false,
            extra,
            selection: std::ptr::null(),
        };
        let mut f: Formatter = std::ptr::null_mut();
        let rc = unsafe { ghostty_formatter_terminal_new(std::ptr::null(), &mut f, self.t, opts) };
        assert!(rc == SUCCESS, "ghostty_formatter_terminal_new 失败：{rc}");
        let mut out = Vec::new();
        let w = Writer {
            write: collect,
            userdata: &mut out as *mut Vec<u8> as *mut c_void,
        };
        let rc = unsafe { ghostty_formatter_format(f, w) };
        unsafe { ghostty_formatter_free(f) };
        assert!(rc == SUCCESS, "ghostty_formatter_format 失败：{rc}");
        out
    }

    /// 纯文本（含回滚区），供快照自检与测试比较。
    pub fn plain(&self) -> Vec<u8> {
        self.format(FORMAT_PLAIN, TerminalExtra::default())
    }

    #[cfg(test)]
    /// 不带任何附加状态的 VT 正文（含颜色、样式），供测试比较。
    pub fn styled(&self) -> Vec<u8> {
        self.format(FORMAT_VT, TerminalExtra::default())
    }

    fn continuation(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let w = Writer {
            write: collect,
            userdata: &mut out as *mut Vec<u8> as *mut c_void,
        };
        unsafe { ghostty_terminal_continuation_write(self.t, w) };
        out
    }

    fn dims(&self) -> (u16, u16) {
        let (mut cols, mut rows) = (0u16, 0u16);
        unsafe {
            ghostty_terminal_get(self.t, 1, &mut cols as *mut u16 as *mut c_void);
            ghostty_terminal_get(self.t, 2, &mut rows as *mut u16 as *mut c_void);
        }
        (cols, rows)
    }

    /// 同尺寸、不连 PTY 的临时 vt，用于计算补行与自检。
    fn scratch(&self, bytes: &[u8]) -> (Box<Sink>, Vt) {
        let (cols, rows) = self.dims();
        let sink = Box::new(Sink::new(-1));
        let mut t = Vt::new(cols.max(1), rows.max(1), &*sink);
        t.write(bytes);
        (sink, t)
    }

    /// 用二进制 snapshot 复制出一个独立终端（不带回调，应答一律丢弃）。
    fn duplicate(&self) -> Option<Vt> {
        let (mut ptr, mut len) = (std::ptr::null_mut(), 0usize);
        if unsafe { ghostty_snapshot_encode_alloc(self.t, std::ptr::null(), &mut ptr, &mut len) }
            != SUCCESS
        {
            return None;
        }
        let mut dec = std::ptr::null_mut();
        let mut t: Term = std::ptr::null_mut();
        let ok = unsafe {
            ghostty_snapshot_decoder_new_buf(std::ptr::null(), &mut dec, ptr, len) == SUCCESS
                && ghostty_snapshot_decoder_decode(dec, &mut t) == SUCCESS
        };
        unsafe {
            ghostty_snapshot_decoder_free(dec);
            ghostty_free(std::ptr::null(), ptr, len);
        }
        (ok && !t.is_null()).then_some(Vt { t })
    }

    /// 活动屏的回放字节：调色板 / 制表位 / 正文 / 补行 / 滚动区 / 键盘 / pwd / 模式 / 光标等屏幕状态。
    /// `prefix` 是此前已输出的字节，用于在临时 vt 中计算补行。拆分不成立时返回 None。
    fn screen_body(&self, prefix: &[u8]) -> Option<Vec<u8>> {
        let content = self.format(FORMAT_VT, TerminalExtra::default());
        // 每一段状态单独格式化，再与纯正文相减，得到正文前 / 后的状态字节。
        let strip_suffix = |b: Vec<u8>| {
            b.ends_with(&content)
                .then(|| b[..b.len() - content.len()].to_vec())
        };
        let strip_prefix =
            |b: Vec<u8>| b.starts_with(&content).then(|| b[content.len()..].to_vec());
        let modes = strip_suffix(self.format(
            FORMAT_VT,
            TerminalExtra {
                modes: true,
                ..Default::default()
            },
        ))?;
        let before = strip_suffix(self.format(
            FORMAT_VT,
            TerminalExtra {
                palette: true,
                tabstops: true,
                ..Default::default()
            },
        ))?;
        let after = strip_prefix(self.format(
            FORMAT_VT,
            TerminalExtra {
                scrolling_region: true,
                keyboard: true,
                pwd: true,
                ..Default::default()
            },
        ))?;
        let state = strip_prefix(self.format(
            FORMAT_VT,
            TerminalExtra {
                screen: SCREEN_ALL,
                ..Default::default()
            },
        ))?;

        let mut out = before;
        out.extend_from_slice(&content);
        // formatter 不输出末尾的空白行：有回滚区时回放后整屏会上移，光标落到最后一行文字上。
        // 按总行数差补换行，使回放后的行布局与原终端一致。
        let mut probe = prefix.to_vec();
        probe.extend_from_slice(&out);
        let (_s, t) = self.scratch(&probe);
        let pad = self.total_rows().saturating_sub(t.total_rows());
        out.extend(std::iter::repeat_n(b"\r\n".as_slice(), pad).flatten());
        out.extend_from_slice(&after);
        for seq in split_csi(&modes) {
            // 备用屏另行切换；其余模式放在正文之后、光标之前。
            if !matches!(seq, b"\x1b[?47h" | b"\x1b[?1047h" | b"\x1b[?1049h") {
                out.extend_from_slice(seq);
            }
        }
        out.extend_from_slice(&state);
        Some(out)
    }

    pub fn snapshot(&self) -> Snapshot {
        let alt = self.alt_screen();
        let mut bytes = b"\x1bc".to_vec();
        // 备用屏时先恢复主屏：副本执行 ?1049l 回到主屏（同时恢复 ?1049h 时保存的光标），回放其内容后再切到备用屏。
        let primary = if alt {
            self.duplicate().and_then(|mut p| {
                p.write(b"\x1b[?1049l");
                p.screen_body(&bytes)
            })
        } else {
            Some(Vec::new())
        };
        let mut fallback = primary.is_none();
        if let Some(primary) = primary {
            bytes.extend_from_slice(&primary);
            if alt {
                bytes.extend_from_slice(b"\x1b[?1049h\x1b[H");
            }
            match self.screen_body(&bytes) {
                Some(body) => bytes.extend_from_slice(&body),
                None => fallback = true,
            }
        }
        if fallback {
            log!("快照拆分不成立，退回上游顺序");
            bytes = b"\x1bc".to_vec();
            if alt {
                bytes.extend_from_slice(b"\x1b[?1049h");
            }
            let all = TerminalExtra {
                palette: true,
                modes: true,
                scrolling_region: true,
                tabstops: true,
                pwd: true,
                keyboard: true,
                screen: SCREEN_ALL,
                ..Default::default()
            };
            bytes.extend_from_slice(&self.format(FORMAT_VT, all));
        }
        let title = self.title();
        if !title.is_empty() {
            bytes.extend_from_slice(b"\x1b]2;");
            bytes.extend_from_slice(&title);
            bytes.extend_from_slice(b"\x1b\\");
        }
        let (_s, mut check) = self.scratch(&bytes);
        let mut selfcheck_ok = same_view(self, &check, "active");
        if alt && selfcheck_ok {
            // 备用屏时同时核对主屏：两边都切回主屏后比较。
            if let Some(mut orig) = self.duplicate() {
                orig.write(b"\x1b[?1049l");
                check.write(b"\x1b[?1049l");
                selfcheck_ok = same_view(&orig, &check, "primary");
            }
        }
        bytes.extend_from_slice(&self.continuation());
        Snapshot {
            bytes,
            alt_screen: alt,
            scrollback_rows: self.scrollback_rows(),
            fallback,
            selfcheck_ok,
        }
    }
}

impl Drop for Vt {
    fn drop(&mut self) {
        unsafe { ghostty_terminal_free(self.t) }
    }
}

/// 每行去掉末尾空白。formatter 用带背景色的空格重画「以背景色擦除」的格子（Codex 输入框实测），
/// 显示相同但格子内容不同，自检时视为等价。
fn trim_lines(b: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(b);
    text.lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .into_bytes()
}

/// 比较纯文本与光标。
fn same_view(orig: &Vt, replay: &Vt, which: &str) -> bool {
    let (a, b) = (orig.plain(), replay.plain());
    let (ca, cb) = (orig.cursor(), replay.cursor());
    if a == b && ca == cb {
        return true;
    }
    if ca == cb && trim_lines(&a) == trim_lines(&b) {
        return true;
    }
    // 不把终端内容写到磁盘或日志（docs/design.md 4.6 节「不保存终端正文」），只记行数与光标。
    log!(
        "快照自检不一致（{which}，光标 {ca:?} / {cb:?}，行数 {} / {}）",
        a.split(|&c| c == b'\n').count(),
        b.split(|&c| c == b'\n').count()
    );
    false
}

/// 把 formatter 输出的模式段（只含 `CSI [?] N h|l`）切成单条序列。
fn split_csi(b: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    for i in 0..b.len() {
        if b[i] == b'h' || b[i] == b'l' {
            out.push(&b[start..=i]);
            start = i + 1;
        }
    }
    out
}

pub fn write_all_fd(fd: RawFd, mut data: &[u8]) {
    while !data.is_empty() {
        let n = unsafe { libc::write(fd, data.as_ptr() as *const c_void, data.len()) };
        if n < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        data = &data[n as usize..];
    }
}

#[cfg(test)]
pub fn escape(b: &[u8]) -> String {
    b.iter()
        .map(|&c| match c {
            0x1b => "\\e".to_string(),
            0x20..=0x7e => (c as char).to_string(),
            _ => format!("\\x{c:02x}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DECAWM: u16 = 7;
    const ANSI: u16 = 0x8000;

    fn pair(cols: u16, rows: u16, input: &[u8]) -> (Box<Sink>, Vt, Box<Sink>, Vt) {
        let s1 = Box::new(Sink::new(-1));
        let mut a = Vt::new(cols, rows, &*s1);
        a.write(input);
        let snap = a.snapshot();
        assert!(!snap.fallback);
        assert!(snap.selfcheck_ok);
        let s2 = Box::new(Sink::new(-1));
        let mut b = Vt::new(cols, rows, &*s2);
        b.write(&snap.bytes);
        (s1, a, s2, b)
    }

    fn assert_same(a: &Vt, b: &Vt, modes: &[u16]) {
        assert_eq!(
            String::from_utf8_lossy(&a.plain()),
            String::from_utf8_lossy(&b.plain()),
            "纯文本不同"
        );
        assert_eq!(escape(&a.styled()), escape(&b.styled()), "带样式正文不同");
        assert_eq!(a.cursor(), b.cursor(), "光标不同");
        assert_eq!(a.alt_screen(), b.alt_screen(), "活动屏不同");
        assert_eq!(a.kitty_flags(), b.kitty_flags(), "Kitty 键盘标志不同");
        assert_eq!(a.title(), b.title(), "标题不同");
        for &m in modes {
            assert_eq!(a.mode(m), b.mode(m), "模式 {m:#x} 不同");
        }
    }

    const MODES: &[u16] = &[
        1,
        7,
        25,
        1000,
        1002,
        1003,
        1004,
        1006,
        2004,
        1049,
        4 | ANSI,
        20 | ANSI,
    ];

    #[test]
    fn restores_text_styles_cursor_and_modes() {
        let input =
            "\x1b]2;任务标题\x07\x1b[31m红色\x1b[0m 普通 \x1b[1;4;38;2;10;200;30m真彩\x1b[0m\r\n\
            宽字符：中文テスト\r\n\
            \x1b]8;;https://example.com\x1b\\链接\x1b]8;;\x1b\\\r\n\
            \x1b[?2004h\x1b[?1000h\x1b[?1006h\x1b[?1004h\x1b[>1u\x1b[?25l\x1b[5;7H光标";
        let (_s1, a, _s2, b) = pair(40, 10, input.as_bytes());
        assert_same(&a, &b, MODES);
        assert_eq!(a.kitty_flags(), 1);
    }

    #[test]
    fn cursor_on_blank_bottom_row_with_scrollback() {
        // 回归：formatter 不输出末尾空白行，曾导致回放后整屏上移一行、下一行输出覆盖最后一行文字。
        let mut input = String::new();
        for i in 1..=70 {
            input.push_str(&format!("n {i}\r\n"));
        }
        let (_s1, mut a, _s2, mut b) = pair(60, 10, input.as_bytes());
        assert_same(&a, &b, MODES);
        a.write(b"n 71\r\n");
        b.write(b"n 71\r\n");
        assert_same(&a, &b, MODES);
    }

    #[test]
    fn background_erased_cells_are_equivalent() {
        // 以背景色擦除的格子回放后变成带背景色的空格：显示相同，纯文本行尾多出空格，自检视为一致。
        let input =
            "top\r\n\x1b[48;5;240m\x1b[K> input\x1b[0m\r\n\x1b[48;5;240m\x1b[K\x1b[0m\x1b[2;3H";
        let s1 = Box::new(Sink::new(-1));
        let mut a = Vt::new(20, 5, &*s1);
        a.write(input.as_bytes());
        let snap = a.snapshot();
        assert!(snap.selfcheck_ok);
        let s2 = Box::new(Sink::new(-1));
        let mut b = Vt::new(20, 5, &*s2);
        b.write(&snap.bytes);
        assert_ne!(a.plain(), b.plain(), "场景未复现：严格比较应只差行尾空白");
        assert_eq!(trim_lines(&a.plain()), trim_lines(&b.plain()));
        assert_eq!(a.cursor(), b.cursor());
    }

    #[test]
    fn soft_wrap_and_scrollback_survive() {
        let mut input = String::new();
        for i in 0..60 {
            input.push_str(&format!("line {i:03} {}\r\n", "x".repeat(i % 37)));
        }
        input.push_str(&"长行".repeat(30)); // 跨多行的软换行
        let (_s1, a, _s2, mut b) = pair(30, 8, input.as_bytes());
        assert!(a.scrollback_rows() > 0);
        assert_same(&a, &b, MODES);
        // 软换行保持为软换行：加宽后重排结果一致。
        let mut a = a;
        a.resize(50, 8, 0, 0);
        b.resize(50, 8, 0, 0);
        assert_same(&a, &b, MODES);
    }

    #[test]
    fn content_affecting_modes_are_applied_after_content() {
        // 自动换行关闭、插入模式、滚动区：上游顺序下回放会破坏正文。
        let input = "第一行\r\n第二行很长很长很长很长很长很长很长很长很长很长\r\n\x1b[?7l\x1b[4h\x1b[2;5r\x1b[3;1H";
        let (_s1, mut a, _s2, mut b) = pair(20, 8, input.as_bytes());
        assert!(!a.mode(DECAWM));
        assert_same(&a, &b, MODES);
        // 滚动区与插入模式在后续输出中行为一致。
        let more = b"\x1b[5;1HAAA\nBBB\nCCC\x1b[3;1Hins";
        a.write(more);
        b.write(more);
        assert_same(&a, &b, MODES);
    }

    #[test]
    fn upstream_order_would_break_content() {
        // 对照：上游 formatter 一次输出全部状态（模式在正文之前），同一输入回放后正文不同。
        let input = "第一行\r\n第二行很长很长很长很长很长很长很长很长很长很长\r\n\x1b[?7l\x1b[4h\x1b[2;5r\x1b[3;1H";
        let s1 = Box::new(Sink::new(-1));
        let mut a = Vt::new(20, 8, &*s1);
        a.write(input.as_bytes());
        let screen = ScreenExtra {
            cursor: true,
            style: true,
            hyperlink: true,
            protection: true,
            kitty_keyboard: true,
            charsets: true,
            ..Default::default()
        };
        let all = TerminalExtra {
            palette: true,
            modes: true,
            scrolling_region: true,
            tabstops: true,
            pwd: true,
            keyboard: true,
            screen,
            ..Default::default()
        };
        let mut upstream = b"\x1bc".to_vec();
        upstream.extend_from_slice(&a.format(FORMAT_VT, all));
        let s2 = Box::new(Sink::new(-1));
        let mut b = Vt::new(20, 8, &*s2);
        b.write(&upstream);
        assert_ne!(
            String::from_utf8_lossy(&a.plain()),
            String::from_utf8_lossy(&b.plain())
        );
    }

    #[test]
    fn alternate_screen_keeps_primary_and_saved_cursor() {
        // 回归：formatter 只输出活动屏，曾导致备用屏下重连后、CLI 退出时主屏内容（含回滚区）丢失。
        let mut input = String::new();
        for i in 1..=20 {
            input.push_str(&format!("shell {i}\r\n"));
        }
        input.push_str("$ claude\x1b[?1049h\x1b[H\x1b[2J全屏界面\x1b[3;3H*\x1b[?2004h");
        let (_s1, mut a, _s2, mut b) = pair(30, 6, input.as_bytes());
        assert!(a.alt_screen());
        assert_same(&a, &b, MODES);
        a.write(b"\x1b[?1049l");
        b.write(b"\x1b[?1049l");
        assert!(String::from_utf8_lossy(&b.plain()).contains("shell 1\n"));
        assert_same(&a, &b, MODES);
        a.write(b"\r\nback\r\n");
        b.write(b"\r\nback\r\n");
        assert_same(&a, &b, MODES);
    }

    #[test]
    fn prompt_title_pwd_and_command_end_are_reported() {
        let s = Box::new(Sink::new(-1));
        let mut t = Vt::new(40, 6, &*s);
        assert!(!t.cursor_at_prompt(), "没有 OSC 133 时不算在提示符");
        // zsh 集成的一轮：提示符开始 → 输入开始 → 命令开始 → 命令结束。
        t.write(b"\x1b]133;A\x07$ \x1b]133;B\x07");
        assert!(t.cursor_at_prompt());
        t.write(b"sleep 9\r\n\x1b]133;C\x07");
        assert!(!t.cursor_at_prompt(), "命令运行中不在提示符");
        assert!(!s.command_end.load(Ordering::Relaxed));
        t.write(b"\x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07");
        assert!(s.command_end.swap(false, Ordering::Relaxed));
        assert!(t.cursor_at_prompt());
        // 备用屏一律不在提示符（与 Ghostty cursorIsAtPrompt 相同）。
        t.write(b"\x1b[?1049h");
        assert!(!t.cursor_at_prompt());
        t.write(b"\x1b[?1049l");
        t.write(b"\x1b]2;\xe6\xa0\x87\xe9\xa2\x98\x07\x1b]7;file://localhost/tmp/a%20b\x07");
        assert!(s.title_changed.load(Ordering::Relaxed));
        assert!(s.pwd_changed.load(Ordering::Relaxed));
        assert_eq!(t.pwd(), "file://localhost/tmp/a%20b");
    }

    #[test]
    fn unfinished_sequence_is_replayed() {
        // 快照落在 SGR 序列中间：后续字节在两边的效果一致。
        let (_s1, mut a, _s2, mut b) = pair(30, 6, b"abc\x1b[38;2;255");
        a.write(b";0;0mred");
        b.write(b";0;0mred");
        assert_same(&a, &b, MODES);
        // UTF-8 多字节字符被切开。
        let (_s1, mut a, _s2, mut b) = pair(30, 6, &"中".as_bytes()[..2]);
        a.write(&"中".as_bytes()[2..]);
        b.write(&"中".as_bytes()[2..]);
        assert_same(&a, &b, MODES);
    }
}
