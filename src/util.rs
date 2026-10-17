//! Small parsing and formatting helpers shared across modules.

use std::borrow::Cow;
use std::fmt::{self, Write as _};
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// A duration written like `"20s"`, `"500ms"`, `"2m"`, `"1h30m"`, or a bare
/// number of seconds (`20` or `"20"`). Zero is allowed; fields that accept it
/// document it as "disabled".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct HumanDuration(pub Duration);

impl HumanDuration {
    pub const fn from_secs(secs: u64) -> Self {
        Self(Duration::from_secs(secs))
    }

    pub fn is_zero(&self) -> bool {
        self.0.is_zero()
    }

    /// `None` when zero ("disabled"), the duration otherwise.
    pub fn non_zero(&self) -> Option<Duration> {
        (!self.0.is_zero()).then_some(self.0)
    }
}

impl fmt::Display for HumanDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&format_duration(self.0))
    }
}

impl FromStr for HumanDuration {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_duration(s).map(Self)
    }
}

impl Serialize for HumanDuration {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for HumanDuration {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl de::Visitor<'_> for Visitor {
            type Value = HumanDuration;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(r#"a duration like "20s", "500ms", "2m" or a number of seconds"#)
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(HumanDuration::from_secs(v))
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
                u64::try_from(v)
                    .map(HumanDuration::from_secs)
                    .map_err(|_| E::custom("duration can't be negative"))
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                parse_duration(v).map(HumanDuration).map_err(E::custom)
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

/// Parse `"20s"`, `"500ms"`, `"1h30m"`, `"1.5s"`, `"2d"` or a bare number of
/// seconds. Units: `ms`, `s`, `m`, `h`, `d`.
pub fn parse_duration(input: &str) -> Result<Duration, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("empty duration".into());
    }
    if s.bytes().all(|b| b.is_ascii_digit()) {
        return s
            .parse::<u64>()
            .map(Duration::from_secs)
            .map_err(|e| format!("invalid duration {input:?}: {e}"));
    }
    let mut total = Duration::ZERO;
    let mut rest = s;
    while !rest.is_empty() {
        let num_len = rest
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(rest.len());
        if num_len == 0 {
            return Err(format!("invalid duration {input:?}: expected a number"));
        }
        let (num, tail) = rest.split_at(num_len);
        let unit_len = tail
            .find(|c: char| c.is_ascii_digit())
            .unwrap_or(tail.len());
        let (unit, tail) = tail.split_at(unit_len);
        let value: f64 = num
            .parse()
            .map_err(|_| format!("invalid duration {input:?}: bad number {num:?}"))?;
        let scale = match unit.trim() {
            "ms" => 0.001,
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3600.0,
            "d" => 86400.0,
            "" => {
                return Err(format!(
                    "invalid duration {input:?}: missing unit after {num}"
                ));
            }
            other => {
                return Err(format!(
                    "invalid duration {input:?}: unknown unit {other:?} (use ms, s, m, h or d)"
                ));
            }
        };
        total += Duration::from_secs_f64(value * scale);
        rest = tail;
    }
    Ok(total)
}

/// The inverse of [`parse_duration`], for configs: `"0s"`, `"500ms"`,
/// `"20s"`, `"1h30m"`.
pub fn format_duration(d: Duration) -> String {
    if d.is_zero() {
        return "0s".into();
    }
    let ms = d.as_millis();
    if !ms.is_multiple_of(1000) {
        return format!("{ms}ms");
    }
    let mut secs = d.as_secs();
    let mut out = String::new();
    for (unit, size) in [("d", 86400), ("h", 3600), ("m", 60), ("s", 1)] {
        if secs >= size {
            let _ = write!(out, "{}{unit}", secs / size);
            secs %= size;
        }
    }
    out
}

/// Compact elapsed time for timings: `850ms`, `3.4s`, `12s`, `1m05s`, `1h02m`.
pub fn format_elapsed(d: Duration) -> String {
    let ms = d.as_millis();
    let secs = d.as_secs();
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 9950 {
        format!("{:.1}s", d.as_secs_f64())
    } else if secs < 60 {
        format!("{}s", d.as_secs_f64().round() as u64)
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// How long ago `then` was, relative to `now` (both unix seconds):
/// `just now`, `5m ago`, `3h ago`, `2d ago`, `3w ago`, `4mo ago`, `2y ago`.
/// Timestamps in the future read as `just now`.
pub fn format_ago(then: i64, now: i64) -> String {
    const MIN: i64 = 60;
    const HOUR: i64 = 60 * MIN;
    const DAY: i64 = 24 * HOUR;
    let secs = now.saturating_sub(then);
    if secs < 45 {
        "just now".into()
    } else if secs < HOUR {
        format!("{}m ago", (secs / MIN).max(1))
    } else if secs < DAY {
        format!("{}h ago", secs / HOUR)
    } else if secs < 7 * DAY {
        format!("{}d ago", secs / DAY)
    } else if secs < 30 * DAY {
        format!("{}w ago", secs / (7 * DAY))
    } else if secs < 365 * DAY {
        format!("{}mo ago", secs / (30 * DAY))
    } else {
        format!("{}y ago", secs / (365 * DAY))
    }
}

/// IEC size with one decimal: `512 B`, `1.5 KiB`, `331.9 MiB`.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Signed size change: `+1.5 MiB`, `-300.0 KiB`, `±0 B`.
pub fn format_size_delta(delta: i64) -> String {
    match delta.signum() {
        0 => "±0 B".into(),
        1 => format!("+{}", format_size(delta.unsigned_abs())),
        _ => format!("-{}", format_size(delta.unsigned_abs())),
    }
}

/// Standard base64 (RFC 4648, with padding).
pub fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// The OSC 52 escape sequence that asks the terminal to put `text` on the
/// system clipboard. Works over ssh and inside tmux (with `set-clipboard on`).
pub fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64_encode(text.as_bytes()))
}

/// Whole-string shell-style glob match: `*`, `?`, `[abc]`, `[a-z]`, `[!x]`,
/// and `\` to escape. An unterminated `[` matches itself.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    // Where to resume after the last `*`: (pattern index after it, text index).
    let mut backtrack: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() {
            match p[pi] {
                '*' => {
                    pi += 1;
                    backtrack = Some((pi, ti));
                    continue;
                }
                '?' => {
                    pi += 1;
                    ti += 1;
                    continue;
                }
                '[' => {
                    if let Some((matched, len)) = match_class(&p[pi..], t[ti]) {
                        if matched {
                            pi += len;
                            ti += 1;
                            continue;
                        }
                    } else if t[ti] == '[' {
                        pi += 1;
                        ti += 1;
                        continue;
                    }
                }
                '\\' if pi + 1 < p.len() => {
                    if p[pi + 1] == t[ti] {
                        pi += 2;
                        ti += 1;
                        continue;
                    }
                }
                c => {
                    if c == t[ti] {
                        pi += 1;
                        ti += 1;
                        continue;
                    }
                }
            }
        }
        match backtrack {
            Some((bp, bt)) => {
                pi = bp;
                ti = bt + 1;
                backtrack = Some((bp, bt + 1));
            }
            None => return false,
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// Match `c` against the bracket expression starting at `p[0] == '['`.
/// Returns `(matched, length of the expression)`, or `None` if unterminated.
fn match_class(p: &[char], c: char) -> Option<(bool, usize)> {
    let mut i = 1;
    let negated = matches!(p.get(i), Some('!' | '^'));
    if negated {
        i += 1;
    }
    let mut matched = false;
    let mut first = true;
    while i < p.len() {
        if p[i] == ']' && !first {
            return Some((matched != negated, i + 1));
        }
        first = false;
        if i + 2 < p.len() && p[i + 1] == '-' && p[i + 2] != ']' {
            if p[i] <= c && c <= p[i + 2] {
                matched = true;
            }
            i += 3;
        } else {
            if p[i] == c {
                matched = true;
            }
            i += 1;
        }
    }
    None
}

/// Truncate `s` to at most `max` terminal columns, ending in `…` when cut.
pub fn truncate(s: &str, max: usize) -> Cow<'_, str> {
    if s.width() <= max {
        return Cow::Borrowed(s);
    }
    if max == 0 {
        return Cow::Borrowed("");
    }
    let mut out = String::new();
    let mut width = 0;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if width + w + 1 > max {
            break;
        }
        width += w;
        out.push(c);
    }
    out.push('…');
    Cow::Owned(out)
}

/// Pad `s` with spaces to `width` terminal columns (no-op if already wider).
pub fn pad(s: &str, width: usize) -> String {
    let w = s.width();
    if w >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - w))
    }
}

/// Seconds since the unix epoch (0 if the clock is before it).
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Make untrusted text (remote output, names from a flake) safe to print:
/// C0 controls other than `\t` and `\n`, DEL, C1 controls and Unicode bidi
/// overrides/isolates are replaced by their `\u{…}` escape, so they can't move
/// the cursor, retitle the terminal or write the clipboard.
pub fn sanitize_terminal(s: &str) -> Cow<'_, str> {
    fn unsafe_char(c: char) -> bool {
        (c.is_control() && c != '\t' && c != '\n')
            || matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}')
    }
    if !s.chars().any(unsafe_char) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if unsafe_char(c) {
            let _ = write!(out, "\\u{{{:x}}}", c as u32);
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

/// 64-bit FNV-1a: a tiny stable hash for file names and cache keys (not
/// for security).
pub fn fnv1a64(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// Memory available for new processes, in bytes: `MemAvailable` from
/// `/proc/meminfo` on Linux, half of `hw.memsize` on macOS; `None` if unknown.
pub fn mem_available_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
        parse_meminfo_available(&meminfo)
    }
    #[cfg(target_os = "macos")]
    {
        let mut size: u64 = 0;
        let mut len = std::mem::size_of::<u64>();
        let name = c"hw.memsize";
        // SAFETY: `name` is NUL-terminated; `size`/`len` describe a u64 buffer.
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                (&mut size as *mut u64).cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        (rc == 0).then_some(size / 2)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

/// `MemAvailable:  12345 kB` → bytes.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_meminfo_available(meminfo: &str) -> Option<u64> {
    let line = meminfo.lines().find(|l| l.starts_with("MemAvailable:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_durations() {
        let ok = |s: &str| parse_duration(s).unwrap();
        assert_eq!(ok("20"), Duration::from_secs(20));
        assert_eq!(ok("20s"), Duration::from_secs(20));
        assert_eq!(ok(" 500ms "), Duration::from_millis(500));
        assert_eq!(ok("2m"), Duration::from_secs(120));
        assert_eq!(ok("1h30m"), Duration::from_secs(5400));
        assert_eq!(ok("1.5s"), Duration::from_millis(1500));
        assert_eq!(ok("1d"), Duration::from_secs(86400));
        assert_eq!(ok("0"), Duration::ZERO);
        assert_eq!(ok("0s"), Duration::ZERO);
        assert_eq!(ok("10 s"), Duration::from_secs(10));
    }

    #[test]
    fn rejects_bad_durations() {
        for bad in ["", "s", "10x", "1.2.3s", "-5s", "5 parsecs", "5s3"] {
            assert!(parse_duration(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn duration_roundtrips_through_its_format() {
        for secs in [0, 1, 59, 60, 61, 3600, 5400, 86400, 90061] {
            let d = Duration::from_secs(secs);
            assert_eq!(parse_duration(&format_duration(d)).unwrap(), d, "{secs}s");
        }
        assert_eq!(format_duration(Duration::from_millis(1500)), "1500ms");
        assert_eq!(format_duration(Duration::from_secs(5400)), "1h30m");
    }

    #[test]
    fn human_duration_deserializes_strings_and_integers() {
        #[derive(Deserialize)]
        struct T {
            a: HumanDuration,
            b: HumanDuration,
        }
        let t: T = toml::from_str("a = \"2m\"\nb = 15").unwrap();
        assert_eq!(t.a, HumanDuration::from_secs(120));
        assert_eq!(t.b, HumanDuration::from_secs(15));
        assert!(toml::from_str::<T>("a = \"2x\"\nb = 1").is_err());
        assert!(toml::from_str::<T>("a = -1\nb = 1").is_err());
        assert_eq!(HumanDuration::from_secs(0).non_zero(), None);
        assert_eq!(
            HumanDuration::from_secs(3).non_zero(),
            Some(Duration::from_secs(3))
        );
    }

    #[test]
    fn formats_elapsed() {
        assert_eq!(format_elapsed(Duration::from_millis(0)), "0ms");
        assert_eq!(format_elapsed(Duration::from_millis(850)), "850ms");
        assert_eq!(format_elapsed(Duration::from_millis(3420)), "3.4s");
        assert_eq!(format_elapsed(Duration::from_millis(9960)), "10s");
        assert_eq!(format_elapsed(Duration::from_secs(12)), "12s");
        assert_eq!(format_elapsed(Duration::from_secs(65)), "1m05s");
        assert_eq!(format_elapsed(Duration::from_secs(3720)), "1h02m");
    }

    #[test]
    fn formats_ago() {
        let now = 1_000_000_000;
        assert_eq!(format_ago(now, now), "just now");
        assert_eq!(format_ago(now + 100, now), "just now");
        assert_eq!(format_ago(now - 44, now), "just now");
        assert_eq!(format_ago(now - 45, now), "1m ago");
        assert_eq!(format_ago(now - 5 * 60, now), "5m ago");
        assert_eq!(format_ago(now - 3 * 3600, now), "3h ago");
        assert_eq!(format_ago(now - 2 * 86400, now), "2d ago");
        assert_eq!(format_ago(now - 15 * 86400, now), "2w ago");
        assert_eq!(format_ago(now - 100 * 86400, now), "3mo ago");
        assert_eq!(format_ago(now - 800 * 86400, now), "2y ago");
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1536), "1.5 KiB");
        assert_eq!(format_size(348_000_000), "331.9 MiB");
        assert_eq!(format_size(5 * 1024 * 1024 * 1024), "5.0 GiB");
        assert_eq!(format_size_delta(0), "±0 B");
        assert_eq!(format_size_delta(1536), "+1.5 KiB");
        assert_eq!(format_size_delta(-307_200), "-300.0 KiB");
        assert_eq!(format_size_delta(i64::MIN), "-8192.0 PiB");
    }

    #[test]
    fn base64_matches_rfc4648_vectors() {
        let cases = [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ];
        for (input, want) in cases {
            assert_eq!(base64_encode(input.as_bytes()), want, "{input:?}");
        }
        assert_eq!(osc52("hi"), "\x1b]52;c;aGk=\x07");
    }

    #[test]
    fn globs() {
        assert!(glob_match("bigmem", "bigmem"));
        assert!(!glob_match("bigmem", "bigmem2"));
        assert!(glob_match("al2-*", "al2-x86_64"));
        assert!(glob_match("al2*", "al2023-x86_64"));
        assert!(!glob_match("al2-*", "al2023-x86_64"));
        assert!(glob_match("*", ""));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*x86*", "al2-x86_64"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"));
        assert!(glob_match("al2-[ax]*", "al2-aarch64"));
        assert!(glob_match("al2-[a-z]*", "al2-x86_64"));
        assert!(!glob_match("al2-[!a]*", "al2-aarch64"));
        assert!(glob_match("[]]", "]"));
        assert!(glob_match("a[", "a["));
        assert!(glob_match("a\\*", "a*"));
        assert!(!glob_match("a\\*", "ab"));
        assert!(glob_match("*a*b*c*", "xxaxxbxxcxx"));
        assert!(!glob_match("*a*b*c*", "xxaxxcxxbxx"));
        assert!(glob_match("mäc-*", "mäc-m1"));
    }

    #[test]
    fn truncates_by_display_width() {
        assert_eq!(truncate("hello", 5), "hello");
        assert_eq!(truncate("hello", 4), "hel…");
        assert_eq!(truncate("hello", 1), "…");
        assert_eq!(truncate("hello", 0), "");
        assert_eq!(truncate("日本語", 5), "日本…");
        assert_eq!(pad("ab", 4), "ab  ");
        assert_eq!(pad("abcdef", 4), "abcdef");
        assert_eq!(pad("日本", 5), "日本 ");
    }

    #[test]
    fn sanitizes_terminal_escapes() {
        assert_eq!(sanitize_terminal("plain\ttext\nok"), "plain\ttext\nok");
        assert!(matches!(sanitize_terminal("plain"), Cow::Borrowed(_)));
        assert_eq!(
            sanitize_terminal("web1\u{1b}]52;c;Y3VybA==\u{7}"),
            "web1\\u{1b}]52;c;Y3VybA==\\u{7}"
        );
        assert_eq!(
            sanitize_terminal("a\rb\u{7f}c\u{9b}d"),
            "a\\u{d}b\\u{7f}c\\u{9b}d"
        );
        assert_eq!(sanitize_terminal("x\u{202e}y"), "x\\u{202e}y");
        assert_eq!(sanitize_terminal("ünïcödé ✓"), "ünïcödé ✓");
    }

    #[test]
    fn fnv_is_stable() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn parses_meminfo() {
        let sample = "MemTotal:       516767472 kB\nMemFree:        477000000 kB\nMemAvailable:   505757432 kB\n";
        assert_eq!(parse_meminfo_available(sample), Some(505_757_432 * 1024));
        assert_eq!(parse_meminfo_available("MemTotal: 1 kB\n"), None);
    }
}
