//! `pty` 角色的二进制帧（M3，docs/protocol.md 第 3.1 节）：`scyllaui attach --terminal` 与 scyllauid 之间，
//! 在 JSON 的 `hello` / `hello_ok` 之后使用。帧格式沿用 S0 的 keep.rs：类型（u8）+ 负载长度（u32 小端）+ 负载，
//! 终端字节原样放在负载里，不转义、不改写。格式与 `hello` 一样承诺不改，只能增加类型。

use std::io::{self, Read, Write};

/// attach → 后台：键盘输入与终端应答，原样写入 PTY。
pub const INPUT: u8 = 2;
/// attach → 后台：JSON `{cols, rows, cell_w, cell_h}`。
pub const RESIZE: u8 = 3;
/// 后台 → attach：接回快照（VT 序列），紧跟 `hello_ok`、总在实时输出之前。
pub const SNAPSHOT: u8 = 10;
/// 后台 → attach：PTY 实时输出。
pub const OUTPUT: u8 = 11;
/// 后台 → attach：JSON `{code}`，Shell 已退出，之后连接关闭。
pub const EXIT: u8 = 12;

pub const MAX_FRAME: usize = 64 << 20;

pub fn write_frame(w: &mut impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    let mut buf = Vec::with_capacity(5 + payload.len());
    buf.push(kind);
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(payload);
    w.write_all(&buf)
}

pub fn read_frame(r: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut head = [0u8; 5];
    r.read_exact(&mut head)?;
    let len = u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "帧过大"));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok((head[0], payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, OUTPUT, b"\x1b[31mhi\x00").unwrap();
        write_frame(&mut buf, INPUT, b"").unwrap();
        let mut r = &buf[..];
        assert_eq!(read_frame(&mut r).unwrap(), (OUTPUT, b"\x1b[31mhi\x00".to_vec()));
        assert_eq!(read_frame(&mut r).unwrap(), (INPUT, Vec::new()));
        assert!(read_frame(&mut r).is_err());
    }
}
