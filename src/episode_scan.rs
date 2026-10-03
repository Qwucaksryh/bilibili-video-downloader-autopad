//! 集数探测：读取 `.下载任务/*.json`，按合集名找出已出现过的最大集数。
//!
//! 宿主的任务文件是「单集一份」的，但每份都带 `collection_title` 与 `episode_order`，
//! 因此把同合集的所有任务聚合起来取最大 `episode_order`，就等于该合集已知的集数。

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

/// 合集名 -> 已探测到的最大集数。探测一次后缓存，避免每集都扫几百个文件。
static CACHE: LazyLock<Mutex<HashMap<String, i64>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// 取锁，容忍前一次调用留下的中毒状态（锁内不会 panic，但仍兜一手）。
fn cache() -> std::sync::MutexGuard<'static, HashMap<String, i64>> {
    CACHE.lock().unwrap_or_else(|e| e.into_inner())
}

/// 只抽取 `"collection_title":"..."` 和 `"episode_order":N`，避免为 10KB 的任务文件做完整 JSON 解析。
pub fn detect_max_order(collection_title: &str) -> i64 {
    if collection_title.trim().is_empty() {
        return 0;
    }

    if let Some(hit) = cache().get(collection_title).copied() {
        return hit;
    }

    let detected = scan_disk(collection_title);
    cache().insert(collection_title.to_string(), detected);
    detected
}

fn scan_disk(collection_title: &str) -> i64 {
    let Some(dir) = crate::task_dir() else { return 0 };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };

    let needle = format!("\"collection_title\":\"{}\"", json_escape(collection_title));
    let mut max_order: i64 = 0;
    let mut scanned: usize = 0;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        scanned += 1;
        if !text.contains(&needle) {
            continue;
        }
        if let Some(order) = extract_order(&text) {
            if order > max_order {
                max_order = order;
            }
        }
    }

    if crate::config::Config::get().verbose {
        eprintln!(
            "[autopad] 探测合集「{collection_title}」: 扫描 {scanned} 个任务文件, 最大集数 = {max_order}"
        );
    }
    max_order
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
