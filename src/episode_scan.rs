//! 集数探测：读取 `.下载任务/*.json`，按合集名找出已出现过的最大集数。
//!
//! 宿主的任务文件是「单集一份」的，但每份都带 `collection_title` 与 `episode_order`，
//! 因此把同合集的所有任务聚合起来取最大 `episode_order`，就等于该合集已知的集数。
//!
//! 性能策略（相对旧实现「每个合集各扫一遍全部任务文件」的 O(合集数 × 文件数)）：
//! 1. **一次遍历建立全局映射**：`read_dir` 一次，逐文件**同时**抽出 `collection_title`
//!    与 `episode_order`，建成「合集 → 最大集数」的 `HashMap`；之后任意合集 O(1) 查表。
//!    整个会话的文件读取量从 `合集数 × 文件数` 次降到至多 `文件数` 次。
//! 2. **指纹失效**：每次调用只做一趟「只看目录项」的轻量指纹 —— 把每个 json 文件的
//!    (文件名, 字节数, mtime) 折叠成一个 FNV-1a u64；指纹没变就直接复用旧映射、
//!    一个文件内容都不读。文件增删、改名、长度变化、内容改写(mtime 变化)都会改变指纹；
//!    覆盖不到的极端情况仅剩「内容被改写但长度与 mtime 被刻意保持不变」。
//! 3. **fixed_width 短路**：`fixed_width = true` 时 `Config::width_for` 根本用不到探测值
//!    （直接走 min_width 分支），直接返回 0，不碰磁盘、不查缓存。
//! 4. **读失败自愈**：单个任务文件 `read_to_string` 失败时，本次映射照常写入（宁可用
//!    不完整结果也不用过期结果），但**不写指纹**（盖 `Stamp::default()`），下次调用必然
//!    重扫；目录不可读而清空缓存时打日志（仅「有数据 → 被清空」的状态跃迁，避免刷屏）。
//!
//! 键的两侧都是**解码态**：payload 侧由宿主反序列化得到，磁盘侧抽出转义原文后经
//! [`json_unescape`] 反转义 —— 不依赖 serde_json 的转义风格（`\b`/`\f`/`\uXXXX` 大小写等）。
//!
//! 锁：全程只取一次 `STATE` 锁，`refresh` 在锁内完成且不再取第二次锁，无重入死锁。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

/// FNV-1a 乘数常量。
const FNV_PRIME: u64 = 0x1000_0000_01b3;

/// 目录指纹：对每个 json 文件的 (文件名, 字节数, mtime) 做 FNV-1a 折叠得到的 u64。
/// `Stamp::default()` / `Stamp::EMPTY`(0) 表示「无数据 / 指纹无效，必须重扫」。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct Stamp {
    fingerprint: u64,
}

impl Stamp {
    /// 折叠 0 字节的结果：空目录的指纹恰好是它，也用作「无数据」哨兵。
    const EMPTY: Self = Stamp { fingerprint: 0 };
}

/// 一次全量扫描的结果 + 判定过期用的目录指纹。
struct ScanState {
    stamp: Stamp,
    /// 键是任务文件里 `collection_title` 的**解码态**（抽出后经 [`json_unescape`] 反转义，
    /// 与 payload 侧的键同形）。
    map: HashMap<String, i64>,
}

impl ScanState {
    fn new() -> Self {
        Self {
            stamp: Stamp::EMPTY,
            map: HashMap::new(),
        }
    }

    /// 任务目录不可读时退到「无任务」状态。
    /// 只在「之前有数据 → 现在被清空」的状态跃迁时打日志（避免每次调用刷屏），
    /// `verbose` 时总是打；否则位数会静默回落 `min_width` 而无人知晓。
    fn clear(&mut self, why: &str, verbose: bool) {
        let had_data = !self.map.is_empty();
        self.stamp = Stamp::EMPTY;
        self.map.clear();
        if had_data || verbose {
            eprintln!(
                "[autopad] {why}：已清空缓存的集数映射，探测值回落 0（补零位数将退到 min_width）"
            );
        }
    }
}

/// 全局「合集 → 最大集数」映射，附带判定失效的目录指纹。
/// 注意：不 `use std::sync::LazyLock`（会与 `export_plugin_v1!` 宏冲突），全路径引用。
static STATE: std::sync::LazyLock<Mutex<ScanState>> =
    std::sync::LazyLock::new(|| Mutex::new(ScanState::new()));

/// 取锁，容忍前一次调用留下的中毒状态（锁内不会 panic，但仍兜一手）。
/// 一次调用只在此取一次锁，[`refresh`] 拿的是 `&mut ScanState`，不会再 lock。
fn state() -> std::sync::MutexGuard<'static, ScanState> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// 找出 `collection_title` 已知的最大 `episode_order`（探测不到时为 0）。
pub fn detect_max_order(collection_title: &str) -> i64 {
    if collection_title.trim().is_empty() {
        return 0;
    }

    // fixed_width 短路：width_for 固定走 min_width 分支，探测值用不上。
    if crate::config::Config::get().fixed_width {
        return 0;
    }

    let mut state = state();
    refresh(&mut state);
    // 两侧都是解码态：payload 侧由宿主反序列化得到，磁盘侧经 json_unescape。
    state.map.get(collection_title).copied().unwrap_or(0)
}

/// 指纹没变 → 直接复用旧映射；变了 → 全量读一遍重建全局映射。
/// 调用方必须已持有 `STATE` 锁；本函数不再取锁。
fn refresh(state: &mut ScanState) {
    let verbose = crate::config::Config::get().verbose;

    let Some(dir) = crate::task_dir() else {
        state.clear("任务目录不可用（APPDATA 未设置）", verbose);
        return;
    };
    let Some((paths, stamp)) = list_json_files(&dir) else {
        state.clear(&format!("任务目录读取失败: {}", dir.display()), verbose);
        return;
    };

    if stamp == state.stamp {
        return; // 指纹未变：零文件内容读取，直接用旧映射。
    }

    let (map, scanned, read_failed) = build_map(&paths);

    if verbose {
        eprintln!(
            "[autopad] 重建集数映射: 目录候选 {} 个, 成功读取 {scanned} 个, 读失败 {read_failed} 个, 覆盖 {} 个合集",
            paths.len(),
            map.len()
        );
    }

    // 不完整映射照常写入 —— 宁可用不完整结果，也不要用过期结果；
    // 但读失败时不盖有效指纹，下次调用必然重扫自愈。
    state.map = map;
    state.stamp = stamp_after(stamp, read_failed);
}

/// 读失败后该盖什么指纹：任何单文件读失败都让指纹失效（`Stamp::default()`），
/// 迫使下次调用重扫；零失败才保留本次扫描的指纹。
fn stamp_after(scan: Stamp, read_failed: usize) -> Stamp {
    if read_failed > 0 {
        Stamp::default()
    } else {
        scan
    }
}

/// 第一趟：只看目录项，收集 json 文件路径并按 (文件名, 字节数, mtime) 计算
/// FNV-1a 指纹 —— 不读任何文件内容。目录不可读时返回 `None`（调用方负责清缓存并打日志）。
fn list_json_files(dir: &Path) -> Option<(Vec<PathBuf>, Stamp)> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut paths: Vec<PathBuf> = Vec::new();
    // 空目录折叠 0 字节 → 指纹 0，恰为 Stamp::EMPTY，与初始状态一致（无需重建）。
    let mut fingerprint: u64 = Stamp::EMPTY.fingerprint;

    for entry in entries.flatten() {
        // 只收普通文件：过滤与 *.json 同名的目录（其 read_to_string 必失败）。
        if !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        // 扩展名大小写不敏感：`.JSON` 也要收。
        if !path_is_json(&entry.path()) {
            continue;
        }

        fingerprint = fnv1a(fingerprint, entry.file_name().to_string_lossy().as_bytes());
        match entry.metadata() {
            Ok(meta) => {
                fingerprint = fnv1a(fingerprint, &meta.len().to_le_bytes());
                match meta.modified() {
                    Ok(mtime) => match mtime.duration_since(UNIX_EPOCH) {
                        Ok(d) => {
                            fingerprint = fnv1a(fingerprint, &d.as_secs().to_le_bytes());
                            fingerprint = fnv1a(fingerprint, &d.subsec_nanos().to_le_bytes());
                        }
                        Err(e) => {
                            // 早于 Unix 纪元的时间戳：折叠「负向标记 + 绝对值」，仍保持确定性。
                            let d = e.duration();
                            fingerprint = fnv1a(fingerprint, &[0xFD]);
                            fingerprint = fnv1a(fingerprint, &d.as_secs().to_le_bytes());
                            fingerprint = fnv1a(fingerprint, &d.subsec_nanos().to_le_bytes());
                        }
                    },
                    // mtime 取不到：折叠一个标记字节，使可得性变化也会改变指纹。
                    Err(_) => fingerprint = fnv1a(fingerprint, &[0xFC]),
                }
            }
            // metadata 取不到：折叠一个标记字节。
            Err(_) => fingerprint = fnv1a(fingerprint, &[0xFB]),
        }
        paths.push(entry.path());
    }

    Some((paths, Stamp { fingerprint }))
}

/// 扩展名是否为 json（大小写不敏感）。
fn path_is_json(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some(e) if e.eq_ignore_ascii_case("json")
    )
}

/// FNV-1a：`h ^= b; h = h.wrapping_mul(PRIME)`。
fn fnv1a(mut h: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// 第二趟：每个文件只读一次，同时抽出 `collection_title`（反转义成解码态）与
/// `episode_order`。返回 `(映射, 成功读取数, 读失败数)`。
fn build_map(paths: &[PathBuf]) -> (HashMap<String, i64>, usize, usize) {
    let mut map: HashMap<String, i64> = HashMap::new();
    let mut scanned: usize = 0;
    let mut read_failed: usize = 0;

    for path in paths {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(_) => {
                read_failed += 1;
                continue;
            }
        };
        scanned += 1;
        let Some(raw_title) = extract_json_string(&text, "\"collection_title\":") else {
            continue;
        };
        let Some(order) = extract_order(&text) else {
            continue;
        };
        let title = json_unescape(raw_title);
        let slot = map.entry(title).or_insert(order);
        if order > *slot {
            *slot = order;
        }
    }

    (map, scanned, read_failed)
}

/// 抽取 `"key":"..."` 的字符串值，返回**磁盘上的转义原文**（配合 [`json_unescape`]
/// 反转义成解码态），避免为 10KB 的任务文件做完整 JSON 解析。
fn extract_json_string<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let start = text.find(key)? + key.len();
    let rest = text.get(start..)?.strip_prefix('"')?;
    let mut escaped = false;
    for (idx, b) in rest.bytes().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        match b {
            b'\\' => escaped = true,
            b'"' => return Some(&rest[..idx]),
            _ => {}
        }
    }
    None
}

/// 找到 `"episode_order":` 后面的整数。
fn extract_order(text: &str) -> Option<i64> {
    let key = "\"episode_order\":";
    let start = text.find(key)? + key.len();
    let rest = text.get(start..)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse::<i64>().ok()
}

/// 把磁盘 JSON 里抽出的转义原文反转义成**解码态**，让映射键两侧（磁盘侧 / payload 侧）
/// 都是解码态，不再依赖 serde_json 的转义风格（`\b`、`\f`、`\uXXXX` 大小写等）。
///
/// 覆盖 `\"` `\\` `\/` `\b` `\f` `\n` `\r` `\t` 与 `\uXXXX`（大小写十六进制都认，
/// 合法代理对拼成一个 `char`）；非法或不完整的转义（含孤立代理项、未知转义、
/// 尾部孤立反斜杠）**原样保留**，绝不 panic，也不引入 UTF-8 边界问题
/// （按 `char` 索引，不按字节切片）。
fn json_unescape(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_string(); // 常见情况：无转义，零解析。
    }
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c != '\\' {
            out.push(c);
            i += 1;
            continue;
        }
        let Some(&esc) = chars.get(i + 1) else {
            out.push('\\'); // 尾部孤立反斜杠原样保留。
            i += 1;
            continue;
        };
        match esc {
            '"' => { out.push('"'); i += 2; }
            '\\' => { out.push('\\'); i += 2; }
            '/' => { out.push('/'); i += 2; }
            'b' => { out.push('\u{8}'); i += 2; }
            'f' => { out.push('\u{C}'); i += 2; }
            'n' => { out.push('\n'); i += 2; }
            'r' => { out.push('\r'); i += 2; }
            't' => { out.push('\t'); i += 2; }
            'u' => {
                // 需要 '\\' 'u' + 恰好 4 个十六进制位；不够则截断、原样保留。
                if i + 6 > chars.len() {
                    out.push('\\');
                    i += 1;
                    continue;
                }
                let Some(first) = parse_hex4(&chars[i + 2..i + 6]) else {
                    out.push('\\'); // 非法十六进制：反斜杠原样保留，后续字符正常处理。
                    i += 1;
                    continue;
                };
                if (0xD800..0xDC00).contains(&first) {
                    // 高代理：必须紧跟合法的 `\uXXXX` 低代理才能拼成一个 char。
                    let low = if i + 12 <= chars.len()
                        && chars[i + 6] == '\\'
                        && chars[i + 7] == 'u'
                    {
                        parse_hex4(&chars[i + 8..i + 12])
                    } else {
                        None
                    };
                    match low {
                        Some(low) if (0xDC00..0xE000).contains(&low) => {
                            let code = 0x10_000
                                + ((u32::from(first) - 0xD800) << 10)
                                + (u32::from(low) - 0xDC00);
                            out.push(char::from_u32(code).unwrap_or('\u{FFFD}'));
                            i += 12;
                        }
                        _ => {
                            push_raw_u_escape(&mut out, &chars[i..i + 6]); // 孤立高代理原样保留。
                            i += 6;
                        }
                    }
                } else if (0xDC00..0xE000).contains(&first) {
                    push_raw_u_escape(&mut out, &chars[i..i + 6]); // 孤立低代理原样保留。
                    i += 6;
                } else {
                    // 普通 BMP 字符；非代理 u16 必然能转成 char。
                    out.push(char::from_u32(u32::from(first)).unwrap_or('\u{FFFD}'));
                    i += 6;
                }
            }
            // 未知转义（如 `\x`）：反斜杠原样保留，后续字符正常处理。
            _ => {
                out.push('\\');
                i += 1;
            }
        }
    }
    out
}

/// 把 6 个字符的 `\uXXXX` 原样写回（用于非法代理对等保留场景）。
fn push_raw_u_escape(out: &mut String, slice: &[char]) {
    out.push('\\');
    out.push('u');
    out.extend(slice[2..6].iter().copied());
}

/// 解析恰好 4 个十六进制位（大小写都认）。
fn parse_hex4(slice: &[char]) -> Option<u16> {
    if slice.len() != 4 {
        return None;
    }
    let mut v: u16 = 0;
    for &c in slice {
        let d = c.to_digit(16)?;
        v = v.wrapping_mul(16).wrapping_add(d as u16);
    }
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个用例独立的临时目录（tag + pid，避免并行冲突）。
    /// 放在 `target/` 下（已被 .gitignore 忽略），不依赖系统 TEMP：本环境里测试子进程
    /// 在系统 TEMP 建目录会被拒绝（os error 5），而工作区内可写，测试也因此更可移植。
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp")
            .join(format!("scan-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            panic!("创建临时目录失败 {}: {e}", dir.display());
        }
        dir
    }

    #[test]
    fn json_unescape_decodes_all_escape_forms() {
        assert_eq!(json_unescape(r#"A\"B\\C\/D"#), "A\"B\\C/D");
        assert_eq!(json_unescape(r#"a\nb\rc\td"#), "a\nb\rc\td");
        assert_eq!(json_unescape(r#"x\by\fz"#), "x\u{8}y\u{C}z");
        // \uXXXX 大小写十六进制都认
        assert_eq!(json_unescape(r#"\u4E2D"#), "中");
        assert_eq!(json_unescape(r#"\u4e2d"#), "中");
        assert_eq!(json_unescape(r#"\u00E9"#), "é");
        // 合法代理对拼成一个 char
        assert_eq!(json_unescape(r#"\uD83D\uDE00"#), "😀");
        // 无转义原样返回
        assert_eq!(json_unescape("普通标题"), "普通标题");
    }

    #[test]
    fn json_unescape_preserves_invalid_sequences() {
        let cases = [
            r#"bad\uZZZZ"#,    // 非法十六进制
            r#"\uD83D"#,       // 孤立高代理
            r#"\uDE00"#,       // 孤立低代理
            r#"\uD83D\uD83D"#, // 高 + 高（非低代理）
            r#"\u12"#,         // \u 后不足 4 位
            r#"\x"#,           // 未知转义
            "trailing\\",      // 尾部孤立反斜杠
            "\\",              // 单个反斜杠
        ];
        for input in cases {
            assert_eq!(json_unescape(input), input, "input={input:?} 必须原样保留");
        }
    }

    #[test]
    fn extracts_decoded_title_and_order_from_forged_json() {
        // 伪造宿主任务文件：标题含引号、反斜杠、控制符、\uXXXX 与代理对
        let json =
            r#"{"collection_title":"A\"B\\C\b\f\n\r\t\u4E2D😀","episode_order":42,"pad":true}"#;
        let raw = extract_json_string(json, "\"collection_title\":").expect("应抽出转义原文");
        assert_eq!(json_unescape(raw), "A\"B\\C\u{8}\u{c}\n\r\t中😀");
        assert_eq!(extract_order(json), Some(42));
    }

    #[test]
    fn list_skips_directories_and_case_insensitive_json_ext() {
        let dir = temp_dir("list");
        std::fs::write(dir.join("a.json"), r#"{"collection_title":"X","episode_order":1}"#)
            .unwrap();
        std::fs::write(dir.join("b.JSON"), r#"{"collection_title":"Y","episode_order":2}"#)
            .unwrap();
        std::fs::create_dir(dir.join("trap.json")).unwrap(); // 同名目录必须被过滤
        std::fs::write(dir.join("ignore.txt"), "x").unwrap();

        let (paths, stamp) = list_json_files(&dir).expect("应能列出目录");
        let names: Vec<String> = paths
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names.len(),
            2,
            "只收 a.json 与 b.JSON，目录/其他扩展名排除: {names:?}"
        );
        assert!(names.iter().any(|n| n == "a.json"));
        assert!(names.iter().any(|n| n == "b.JSON"));
        assert_ne!(stamp, Stamp::EMPTY);

        // 目录不存在 → None（调用方负责清缓存并打日志）
        assert!(list_json_files(&dir.join("no-such-dir")).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fingerprint_tracks_name_len_and_mtime() {
        let dir = temp_dir("fp");
        std::fs::write(dir.join("a.json"), "abc").unwrap();
        let (_, fp1) = list_json_files(&dir).unwrap();

        // 等长改写：只有 mtime 变化（留 20ms 确保时间戳推进）
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(dir.join("a.json"), "xyz").unwrap();
        let (_, fp2) = list_json_files(&dir).unwrap();
        assert_ne!(fp1, fp2, "等长改写必须靠 mtime 让指纹失效");

        // 长度变化
        std::fs::write(dir.join("a.json"), "longer-content").unwrap();
        let (_, fp3) = list_json_files(&dir).unwrap();
        assert_ne!(fp2, fp3, "长度变化必须让指纹失效");

        // 数量守恒的增删：改名 a→b（文件数与 mtime 都不变，只有名字变）
        std::fs::rename(dir.join("a.json"), dir.join("b.json")).unwrap();
        let (_, fp4) = list_json_files(&dir).unwrap();
        assert_ne!(fp3, fp4, "文件名参与指纹：数量守恒的增删也要失效");

        // 删除 → 空目录
        std::fs::remove_file(dir.join("b.json")).unwrap();
        let (paths, fp5) = list_json_files(&dir).unwrap();
        assert!(paths.is_empty());
        assert_eq!(fp5, Stamp::EMPTY, "空目录折叠为 0，恰与 Stamp::EMPTY 相等");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_map_counts_read_failures_and_forces_rescan() {
        let dir = temp_dir("readfail");
        std::fs::write(dir.join("good.json"), r#"{"collection_title":"甲","episode_order":7}"#)
            .unwrap();
        std::fs::write(dir.join("good2.json"), r#"{"collection_title":"甲","episode_order":12}"#)
            .unwrap();
        // 非法 UTF-8：read_to_string 必失败
        std::fs::write(dir.join("bad.json"), [0xFFu8, 0xFE, 0x00, 0x80]).unwrap();

        let (paths, stamp) = list_json_files(&dir).unwrap();
        assert_eq!(paths.len(), 3);
        let (map, scanned, read_failed) = build_map(&paths);
        assert_eq!(read_failed, 1, "坏文件必须计入读失败");
        assert_eq!(scanned, 2);
        assert_eq!(map.get("甲"), Some(&12), "不完整映射照常保留（同合集取最大集数）");

        // P1 核心：读失败 → 盖无效指纹，下次调用必重扫自愈；零失败才保留本次指纹
        assert_eq!(stamp_after(stamp, read_failed), Stamp::default());
        assert_eq!(stamp_after(stamp, 0), stamp);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clear_resets_stamp_and_map() {
        let mut state = ScanState::new();
        state.map.insert("k".to_string(), 3);
        state.stamp = Stamp { fingerprint: 9 };
        state.clear("测试：目录不可读", false); // 有数据 → 被清空（跃迁，会打一条日志）
        assert!(state.map.is_empty());
        assert_eq!(state.stamp, Stamp::EMPTY);
        state.clear("测试：目录不可读", false); // 已无数据且非 verbose → 静默
        assert!(state.map.is_empty());
        assert_eq!(state.stamp, Stamp::EMPTY);
    }
}
