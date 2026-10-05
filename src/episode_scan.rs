//! 集数探测：读取 `.下载任务/*.json`，按合集名找出已出现过的最大集数。
//!
//! 宿主的任务文件是「单集一份」的，但每份都带 `collection_title` 与 `episode_order`，
//! 因此把同合集的所有任务聚合起来取最大 `episode_order`，就等于该合集已知的集数。
//!
//! 性能策略（相对旧实现「每个合集各扫一遍全部任务文件」的 O(合集数 × 文件数)）：
//! 1. **一次遍历建立全局映射**：`read_dir` 一次，逐文件**同时**抽出 `collection_title`
//!    与 `episode_order`，建成「合集 → 最大集数」的 `HashMap`；之后任意合集 O(1) 查表。
//!    整个会话的文件读取量从 `合集数 × 文件数` 次降到至多 `文件数` 次。
//! 2. **指纹失效**：每次调用只做一趟「只看目录项」的轻量指纹（json 文件数 + 最新 mtime），
//!    指纹没变就直接复用旧映射、一个文件内容都不读；变了（补下新集 / 删除任务 / 改写文件）
//!    才重建。改写会把 mtime 推到最新、增删会改文件数，所以两类变化都能被抓住。
//! 3. **fixed_width 短路**：`fixed_width = true` 时 `Config::width_for` 根本用不到探测值
//!    （直接走 min_width 分支），直接返回 0，不碰磁盘、不查缓存。
//!
//! 锁：全程只取一次 `STATE` 锁，`refresh` 在锁内完成且不再取第二次锁，无重入死锁。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use std::time::SystemTime;

/// 目录指纹：json 文件数 + 最新 mtime，用于判断全局映射是否过期。
#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    json_files: usize,
    latest_mtime: Option<SystemTime>,
}

impl Stamp {
    /// 「没有可读任务目录」的指纹，也用作初始值：初始映射为空，
    /// 目录同样为空/缺失时无需重建，目录一出现文件指纹就会变化。
    const EMPTY: Self = Self {
        json_files: 0,
        latest_mtime: None,
    };
}

/// 一次全量扫描的结果 + 判定过期用的目录指纹。
struct ScanState {
    stamp: Stamp,
    /// 键是任务文件里 `collection_title` 的**转义原文**（与 [`json_escape`] 产物同形）。
    map: HashMap<String, i64>,
}

impl ScanState {
    fn new() -> Self {
        Self {
            stamp: Stamp::EMPTY,
            map: HashMap::new(),
        }
    }

    /// 任务目录不可读时退到「无任务」状态（下次目录恢复后指纹变化会重建）。
    fn clear(&mut self) {
        self.stamp = Stamp::EMPTY;
        self.map.clear();
    }
}

/// 全局「合集 → 最大集数」映射，附带判定失效的目录指纹。
static STATE: LazyLock<Mutex<ScanState>> = LazyLock::new(|| Mutex::new(ScanState::new()));

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

    let key = json_escape(collection_title);
    let mut state = state();
    refresh(&mut state);
    state.map.get(&key).copied().unwrap_or(0)
}

/// 指纹没变 → 直接复用旧映射；变了 → 全量读一遍重建全局映射。
/// 调用方必须已持有 `STATE` 锁；本函数不再取锁。
fn refresh(state: &mut ScanState) {
    let Some(dir) = crate::task_dir() else {
        state.clear();
        return;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        state.clear();
        return;
    };

    // 第一趟：只看目录项，收集 json 路径并计算指纹 —— 不读任何文件内容。
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut latest: Option<SystemTime> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Ok(mtime) = entry.metadata().and_then(|meta| meta.modified()) {
            latest = Some(match latest {
                Some(prev) if prev >= mtime => prev,
                _ => mtime,
            });
        }
        paths.push(path);
    }

    let stamp = Stamp {
        json_files: paths.len(),
        latest_mtime: latest,
    };
    if stamp == state.stamp {
        return; // 指纹未变：零文件内容读取，直接用旧映射。
    }

    // 第二趟：每个文件只读一次，同时抽出 collection_title 与 episode_order。
    let mut map: HashMap<String, i64> = HashMap::new();
    let mut scanned: usize = 0;
    for path in &paths {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        scanned += 1;
        let Some(title) = extract_json_string(&text, "\"collection_title\":") else {
            continue;
        };
        let Some(order) = extract_order(&text) else {
            continue;
        };
        let slot = map.entry(title.to_string()).or_insert(order);
        if order > *slot {
            *slot = order;
        }
    }

    if crate::config::Config::get().verbose {
        eprintln!(
            "[autopad] 重建集数映射: 扫描 {scanned} 个任务文件, 覆盖 {} 个合集",
            map.len()
        );
    }

    state.map = map;
    state.stamp = stamp;
}

/// 抽取 `"key":"..."` 的字符串值，返回**磁盘上的转义原文**（与 [`json_escape`] 同形），
/// 避免为 10KB 的任务文件做完整 JSON 解析。
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

/// JSON 字符串转义，只需处理会影响子串匹配的字符。
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out
}
