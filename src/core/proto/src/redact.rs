//! 写库前的机械脱敏（docs/design.md 10.5 节决策 11）：把常见密钥写法换成 `[REDACTED]`，保留前面的键名、分隔符与 `Bearer `。
//! 覆盖：赋值形式、Bearer、已知前缀、PEM 私钥、`github_pat_`、
//! `AIza…`、`sk_live_` / `rk_live_`、JWT、URL 中的密码与中文键名（密码、口令、密钥、令牌）。只匹配 ASCII 写法，
//! 按字节扫描，不依赖正则库；宁可多遮（如 `max_tokens=100`）也不漏。

pub const REDACTED: &str = "[REDACTED]";

/// 依次遮住 PEM 私钥、已知前缀的令牌、Bearer、赋值形式的值与 URL 中的密码；没有命中时原样返回。
pub fn redact(text: &str) -> String {
    let s = pem_blocks(text);
    let s = scan(&s, token_end);
    let s = scan(&s, bearer_value);
    let s = scan(&s, assignment_value);
    let s = scan(&s, chinese_value);
    scan(&s, url_password)
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn run(b: &[u8], from: usize, ok: impl Fn(u8) -> bool) -> usize {
    let mut j = from;
    while j < b.len() && ok(b[j]) {
        j += 1;
    }
    j
}

/// 逐字节找命中：`hit(b, i)` 返回要换掉的区间（起点 ≥ i），区间前后的内容保留。
fn scan(s: &str, hit: fn(&[u8], usize) -> Option<(usize, usize)>) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let (mut i, mut last) = (0, 0);
    while i < b.len() {
        match hit(b, i) {
            Some((from, to)) => {
                out.push_str(&s[last..from]);
                out.push_str(REDACTED);
                i = to.max(i + 1);
                last = to;
            }
            None => i += 1,
        }
    }
    out.push_str(&s[last..]);
    out
}

/// `-----BEGIN … PRIVATE KEY-----` 到对应的 END 整段。
fn pem_blocks(s: &str) -> String {
    fn marker(s: &str, from: usize, word: &str) -> Option<(usize, usize)> {
        let mut at = from;
        while let Some(k) = s[at..].find(word) {
            let start = at + k;
            let head = start + word.len();
            let len = s[head..].bytes().take_while(|c| c.is_ascii_uppercase() || *c == b' ').count();
            if s[head + len..].starts_with("-----") && s[head..head + len].ends_with("PRIVATE KEY") {
                return Some((start, head + len + 5));
            }
            at = head;
        }
        None
    }
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    while let Some((start, after)) = marker(s, last, "-----BEGIN ") {
        let Some((_, end)) = marker(s, after, "-----END ") else { break };
        out.push_str(&s[last..start]);
        out.push_str(REDACTED);
        last = end;
    }
    out.push_str(&s[last..]);
    out
}

/// 已知前缀的令牌（前面不能紧挨字母数字），整段换掉。
fn token_end(b: &[u8], i: usize) -> Option<(usize, usize)> {
    if i > 0 && is_word(b[i - 1]) {
        return None;
    }
    let rest = &b[i..];
    let alnum = |c: u8| c.is_ascii_alphanumeric();
    let url64 = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'-';
    let tail = |prefix: usize, min: usize, ok: &dyn Fn(u8) -> bool| {
        let e = run(b, i + prefix, ok);
        (e - i - prefix >= min).then_some((i, e))
    };
    if rest.starts_with(b"sk_live_") || rest.starts_with(b"rk_live_") {
        return tail(8, 16, &alnum);
    }
    if rest.starts_with(b"github_pat_") {
        return tail(11, 20, &|c| alnum(c) || c == b'_');
    }
    if rest.starts_with(b"sk-") {
        return tail(3, 12, &url64);
    }
    if rest.len() > 3 && rest.starts_with(b"gh") && b"pousr".contains(&rest[2]) && rest[3] == b'_' {
        return tail(4, 12, &alnum);
    }
    if rest.len() > 4 && rest.starts_with(b"xox") && b"baprs".contains(&rest[3]) && rest[4] == b'-' {
        return tail(5, 12, &|c| alnum(c) || c == b'-');
    }
    if rest.starts_with(b"AKIA") {
        let e = run(b, i + 4, |c| c.is_ascii_uppercase() || c.is_ascii_digit());
        return (e - i - 4 == 16).then_some((i, e));
    }
    if rest.starts_with(b"AIza") {
        return tail(4, 35, &url64);
    }
    if rest.starts_with(b"eyJ") {
        // JWT：三段 base64url，前两段都以 eyJ 开头。
        let a = run(b, i + 3, url64);
        if a - i - 3 < 8 || b.get(a) != Some(&b'.') || !b[a + 1..].starts_with(b"eyJ") {
            return None;
        }
        let p = run(b, a + 4, url64);
        if p - a - 4 < 8 || b.get(p) != Some(&b'.') {
            return None;
        }
        let s = run(b, p + 1, url64);
        return (s - p - 1 >= 8).then_some((i, s));
    }
    None
}

/// `Bearer <令牌>`（不分大小写）：只换令牌。
fn bearer_value(b: &[u8], i: usize) -> Option<(usize, usize)> {
    if (i > 0 && is_word(b[i - 1])) || b.len() < i + 7 || !b[i..i + 6].eq_ignore_ascii_case(b"bearer") {
        return None;
    }
    let v = run(b, i + 6, |c| c == b' ' || c == b'\t');
    if v == i + 6 {
        return None;
    }
    let e = run(b, v, |c| c.is_ascii_alphanumeric() || b"._~+/=-".contains(&c) || c == b'_');
    (e - v >= 8).then_some((v, e))
}

/// 键名含 API_KEY / APIKEY / API-KEY / TOKEN / PASSWORD / PASSWD / SECRET（不分大小写，可带引号）后接 `:` 或 `=`：只换值。
fn assignment_value(b: &[u8], i: usize) -> Option<(usize, usize)> {
    if i > 0 && is_word(b[i - 1]) {
        return None;
    }
    let mut k = i;
    if matches!(b.get(k), Some(b'"' | b'\'')) {
        k += 1;
    }
    let end = run(b, k, |c| is_word(c) || c == b'-');
    if end == k {
        return None;
    }
    let key = b[k..end].to_ascii_uppercase();
    let has = |n: &[u8]| key.windows(n.len()).any(|w| w == n);
    let named = if key.contains(&b'-') {
        has(b"API-KEY")
    } else {
        [&b"API_KEY"[..], b"APIKEY", b"TOKEN", b"PASSWORD", b"PASSWD", b"SECRET"].iter().any(|n| has(n))
    };
    if !named {
        return None;
    }
    let mut j = end;
    if matches!(b.get(j), Some(b'"' | b'\'')) {
        j += 1;
    }
    j = run(b, j, |c| c == b' ' || c == b'\t');
    if !matches!(b.get(j), Some(b':' | b'=')) {
        return None;
    }
    value(b, run(b, j + 1, |c| c == b' ' || c == b'\t'))
}

/// 值：引号括起的（同一行内，连引号一起换）或到空白、逗号、分号、引号为止的一段。
fn value(b: &[u8], v: usize) -> Option<(usize, usize)> {
    match b.get(v) {
        Some(&q @ (b'"' | b'\'')) => {
            let close = run(b, v + 1, |c| c != q && c != b'\n');
            (b.get(close) == Some(&q)).then_some((v, close + 1))
        }
        Some(_) => {
            let e = run(b, v, |c| !c.is_ascii_whitespace() && !b",;\"'".contains(&c));
            (e > v).then_some((v, e))
        }
        None => None,
    }
}

/// 中文键名：密码 / 口令 / 密钥 / 令牌，后接 `:`、`=` 或全角冒号：只换值（到空白或中英文标点为止）。
fn chinese_value(b: &[u8], i: usize) -> Option<(usize, usize)> {
    let rest = &b[i..];
    let key = ["密码", "口令", "密钥", "令牌"].iter().find(|k| rest.starts_with(k.as_bytes()))?;
    let mut j = run(b, i + key.len(), |c| c == b' ' || c == b'\t');
    if matches!(b.get(j), Some(b':' | b'=')) {
        j += 1;
    } else if b[j..].starts_with("：".as_bytes()) {
        j += "：".len();
    } else {
        return None;
    }
    let v = run(b, j, |c| c == b' ' || c == b'\t');
    // 值只取 ASCII 的一段：中文标点与汉字都是多字节，遇到即停。
    let e = run(b, v, |c| c.is_ascii_graphic() && !b",;\"'".contains(&c));
    (e > v).then_some((v, e))
}

/// `scheme://user:password@host`：只换密码。
fn url_password(b: &[u8], i: usize) -> Option<(usize, usize)> {
    if !b[i..].starts_with(b"://") {
        return None;
    }
    let start = i + 3;
    let end = run(b, start, |c| !c.is_ascii_whitespace() && !b"/?#@".contains(&c));
    if b.get(end) != Some(&b'@') {
        return None;
    }
    let colon = b[start..end].iter().position(|&c| c == b':')? + start;
    (end > colon + 1).then_some((colon + 1, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assignments_keep_key_and_separator() {
        assert_eq!(redact("OPENAI_API_KEY=abc123"), "OPENAI_API_KEY=[REDACTED]");
        assert_eq!(redact("export GITHUB_TOKEN = 'xyz 789'"), "export GITHUB_TOKEN = [REDACTED]");
        assert_eq!(redact(r#"{"password": "hunter2", "user": "a"}"#), r#"{"password": [REDACTED], "user": "a"}"#);
        assert_eq!(redact("x-api-key: k1, next"), "x-api-key: [REDACTED], next");
        assert_eq!(redact("db_passwd:secret;"), "db_passwd:[REDACTED];");
        assert_eq!(redact("client_secret=s3cr3t\nnext line"), "client_secret=[REDACTED]\nnext line");
    }

    #[test]
    fn known_tokens_are_replaced_whole() {
        let cases = [
            "sk-ant-api03-AbCdEfGhIjKlMn",
            "sk-proj-0123456789abcdef",
            "ghp_0123456789abcdefABCD",
            "github_pat_11ABCDEFG0123456789_abcdefghij",
            "xoxb-1234567890-abcdefghij",
            "AKIAIOSFODNN7EXAMPLE",
            "AIzaSyA-1234567890abcdefghijklmnopqrstu",
            "sk_live_0123456789abcdefABCD",
            "rk_live_0123456789abcdefABCD",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
        ];
        for t in cases {
            assert_eq!(redact(&format!("用这个 {t} 试试")), "用这个 [REDACTED] 试试", "{t}");
        }
    }

    #[test]
    fn bearer_pem_url_and_chinese_keys() {
        assert_eq!(redact("Authorization: Bearer abcdef0123456789"), "Authorization: Bearer [REDACTED]");
        assert_eq!(redact("curl -H 'authorization: bearer abc.def-ghi_jkl'"), "curl -H 'authorization: bearer [REDACTED]'");
        let pem = "前\n-----BEGIN OPENSSH PRIVATE KEY-----\nAAAA\nBBBB\n-----END OPENSSH PRIVATE KEY-----\n后";
        assert_eq!(redact(pem), "前\n[REDACTED]\n后");
        assert_eq!(redact("git clone https://alex:p4ss-w0rd@example.com/r.git"), "git clone https://alex:[REDACTED]@example.com/r.git");
        assert_eq!(redact("数据库密码：Hunter2!，记得改"), "数据库密码：[REDACTED]，记得改");
        assert_eq!(redact("令牌 = abc123 已过期"), "令牌 = [REDACTED] 已过期");
    }

    #[test]
    fn ordinary_text_is_untouched() {
        for t in [
            "修复登录失败的问题",
            "把 risk-free 的方案写进文档",
            "sk-short",
            "Bearer abc",
            "eyJhbGciOi 不是令牌",
            "AKIA12345",
            "打开 https://example.com/path?q=1",
            "用户名：alex",
            "token 用量见状态栏",
            "密码学入门",
            "",
        ] {
            assert_eq!(redact(t), t, "{t}");
        }
    }

    #[test]
    fn already_redacted_text_is_stable() {
        let once = redact("API_KEY=abc sk-0123456789abcdef https://u:p@h");
        assert_eq!(redact(&once), once);
    }
}
