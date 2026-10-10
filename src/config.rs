//! 插件配置：可选文件 + 环境变量覆盖。
//!
//! 零依赖的极简 TOML / key=value 解析，避免为一个小插件引入完整 TOML 解析器。

use std::path::PathBuf;
use std::sync::LazyLock;

pub const CONFIG_FILE_NAME: &str = "autopad.toml";

/// 宽度硬顶：`min_width` / `max_width` 都被强制夹到 `1..=WIDTH_LIMIT`。
///
/// 存在的理由是安全而不是功能——宽度会直接进 `format!("{:0>width$}")`，
/// 一个离谱的值（比如 `AUTOPAD_MIN_WIDTH=999999999`）会把文件名撑到病态长度，
/// 进而溢出 panic 甚至 OOM。64 已远超真实需要（补零宽度最多也就 4 位）。
pub const WIDTH_LIMIT: usize = 64;

#[derive(Debug, Clone)]
pub struct Config {
    /// 开关，关掉后插件只记录日志、不改任何路径。
    pub enabled: bool,
    /// 补零宽度下限。默认 2，即 12 集的季番也会写成 `第01话`。
    pub min_width: usize,
    /// 补零宽度上限，防止异常数据把文件名撑爆。默认 4。
    pub max_width: usize,
    /// 为 true 时忽略自动探测的集数，一律使用 min_width。
    pub fixed_width: bool,
    /// 是否把文件名里的 `第N话` 也一起补零。
    pub pad_filename: bool,
    /// 是否把 episode_dir 末级目录名里的 `第N话` 也一起补零。
    pub pad_episode_dir: bool,
    /// 详细日志。
    pub verbose: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            min_width: 2,
            max_width: 4,
            fixed_width: false,
            pad_filename: true,
            pad_episode_dir: true,
            verbose: false,
        }
    }
}

impl Config {
    /// 应用运行时的配置快照，进程内只解析一次。
    pub fn get() -> &'static Self {
        static CONFIG: LazyLock<Config> = LazyLock::new(Config::load);
        &CONFIG
    }

    fn load() -> Self {
        let mut cfg = Config::default();

        if let Some(path) = config_path() {
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    cfg.apply_pairs(&text);
                    eprintln!("[autopad] 已读取配置: {}", path.display());
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    eprintln!(
                        "[autopad] 未找到配置文件，使用默认值（如需自定义请创建 {}）",
                        path.display()
                    );
                }
                Err(err) => {
                    eprintln!("[autopad] 读取配置文件失败，使用默认值: {err}");
                }
            }
        }

        // 环境变量优先级高于配置文件，方便临时压测
        if let Ok(v) = std::env::var("AUTOPAD_MIN_WIDTH") {
            match v.trim().parse::<usize>() {
                Ok(n) => cfg.min_width = n,
                Err(_) => eprintln!("[autopad] AUTOPAD_MIN_WIDTH 无法识别: {v:?}（需要非负整数）"),
            }
        }
        if let Ok(v) = std::env::var("AUTOPAD_ENABLED") {
            // 必须复用 parse_bool，否则 `AUTOPAD_ENABLED=off` 会被当成「开启」
            // （旧实现只认 0/false/no，与配置文件侧的 off → false 不一致）。
            match parse_bool(v.trim()) {
                Some(b) => cfg.enabled = b,
                None => eprintln!(
                    "[autopad] AUTOPAD_ENABLED 无法识别: {v:?}（支持 1/0、true/false、yes/no、on/off）"
                ),
            }
        }

        cfg.normalize();
        cfg
    }

    /// 归一化到 `1..=WIDTH_LIMIT` 且保证 `min <= max`。
    /// 顺序不能反：先把 min 夹到上限以内，再让 max 追上 min。
    ///
    /// 这一步同时堵掉两个已确认的问题：
    /// 1. 旧代码只做 `max < min → max = min`，当 `min = max = 0` 时条件不触发，
    ///    随后 `width_for` 的 fixed_width 分支会算出 `clamp(1, 0)` → panic；
    /// 2. 「抬高 max 到 min」等于废掉了 max_width 原本的硬顶作用，
    ///    巨大的 min_width（如离谱的 `AUTOPAD_MIN_WIDTH=999999999`）会一路传到
    ///    `format!("{:0>width$}")`，把文件名撑到病态长度甚至 OOM。
    pub(crate) fn normalize(&mut self) {
        self.min_width = self.min_width.clamp(1, WIDTH_LIMIT);
        self.max_width = self.max_width.clamp(self.min_width, WIDTH_LIMIT);
    }

    /// 解析 `key = value` 形式的配置，忽略 `#` 注释与空行。
    fn apply_pairs(&mut self, text: &str) {
        for raw in text.lines() {
            // `#` 只有出现在行首或前面是空白时才算注释；
            // 否则 `min_width = 2 #说明` 之外，像 `值#123` 这种含 `#` 的值会被拦腰截断。
            let cut = raw
                .find('#')
                .filter(|&i| {
                    i == 0 || raw[..i].chars().next_back().is_some_and(char::is_whitespace)
                })
                .unwrap_or(raw.len());
            let line = raw[..cut].trim();
            if line.is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim();
            let value = value.trim().trim_matches('"').trim_matches('\'');

            match key {
                "enabled" => self.enabled = parse_bool(value).unwrap_or(self.enabled),
                "pad_filename" => self.pad_filename = parse_bool(value).unwrap_or(self.pad_filename),
                "pad_episode_dir" => {
                    self.pad_episode_dir = parse_bool(value).unwrap_or(self.pad_episode_dir);
                }
                "fixed_width" => self.fixed_width = parse_bool(value).unwrap_or(self.fixed_width),
                "verbose" => self.verbose = parse_bool(value).unwrap_or(self.verbose),
                "min_width" => {
                    if let Ok(n) = value.parse::<usize>() {
                        self.min_width = n;
                    }
                }
                "max_width" => {
                    if let Ok(n) = value.parse::<usize>() {
                        self.max_width = n;
                    }
                }
                _ => {}
            }
        }
    }

    /// 依据探测到的合集集数算出补零宽度。
    pub fn width_for(&self, detected_max_order: i64) -> usize {
        if self.fixed_width {
            // 防御式：即便未来有人绕过 load() 直接构造 Config（min > max 或 max = 0），
            // 这里也绝不能让 `clamp(1, 0)` panic —— panic 在插件里等于整插件静默失效。
            return self.min_width.clamp(1, self.max_width.max(1));
        }
        let digits = if detected_max_order <= 0 {
            1
        } else {
            detected_max_order.ilog10() as usize + 1
        };
        digits.clamp(self.min_width, self.max_width)
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn config_path() -> Option<PathBuf> {
    Some(crate::app_data_dir()?.join(CONFIG_FILE_NAME))
}

/// 首次加载时把默认配置写到磁盘，方便用户直接改。
pub fn write_default_config_if_absent() {
    let Some(path) = config_path() else { return };
    if path.exists() {
        return;
    }
    let text = "# bdp-autopad 配置（改完需要重启下载器生效）\n\
                enabled = true\n\
                \n\
                # 补零宽度下限：2 表示 12 集的季番也写成 第01话\n\
                min_width = 2\n\
                # 补零宽度上限\n\
                max_width = 4\n\
                # true = 忽略自动探测的集数，一律使用 min_width\n\
                fixed_width = false\n\
                \n\
                # 文件名里的 第N话 是否也补零\n\
                pad_filename = true\n\
                pad_episode_dir = true\n\
                verbose = false\n";
    let _ = std::fs::write(&path, text);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回归：min = max = 0 时旧代码会在 width_for 的 fixed_width 分支 panic。
    #[test]
    fn normalize_repairs_zero_widths() {
        let mut cfg = Config {
            min_width: 0,
            max_width: 0,
            ..Config::default()
        };
        cfg.normalize();
        assert_eq!((cfg.min_width, cfg.max_width), (1, 1));
        assert_eq!(cfg.width_for(0), 1, "fixed_width 分支不得 panic");
    }

    /// 回归：即便绕过 load() 手工构造非法 Config，width_for 也绝不能 panic。
    #[test]
    fn fixed_width_never_panics_on_inverted_range() {
        let cfg = Config {
            fixed_width: true,
            min_width: 5,
            max_width: 0,
            ..Config::default()
        };
        assert_eq!(cfg.width_for(0), 1);
        let cfg2 = Config {
            fixed_width: true,
            min_width: 5,
            max_width: 9,
            ..Config::default()
        };
        assert_eq!(cfg2.width_for(0), 5, "合法区间仍按 min_width");
    }

    /// 回归：宽度必须有硬顶，否则 format!("{:0>width$}") 会被撑爆。
    #[test]
    fn normalize_enforces_width_limit() {
        let mut cfg = Config {
            min_width: usize::MAX,
            max_width: usize::MAX,
            ..Config::default()
        };
        cfg.normalize();
        assert_eq!(cfg.min_width, WIDTH_LIMIT);
        assert_eq!(cfg.max_width, WIDTH_LIMIT);
        assert!(cfg.width_for(999999999) <= WIDTH_LIMIT);
        assert_eq!(cfg.width_for(1), WIDTH_LIMIT, "下限被抬到 64 时 1 位数也要补到 64");
    }

    /// 回归：AUTOPAD_ENABLED=off 必须与配置文件侧的 off → false 一致。
    #[test]
    fn parse_bool_accepts_on_off() {
        assert_eq!(parse_bool("off"), Some(false));
        assert_eq!(parse_bool("ON"), Some(true));
        assert_eq!(parse_bool("no"), Some(false));
        assert_eq!(parse_bool("garbage"), None);
    }

    #[test]
    fn comments_only_start_at_line_begin_or_after_space() {
        let mut cfg = Config::default();
        cfg.apply_pairs("min_width = 3 # 行尾注释\nmax_width = 5\n");
        assert_eq!((cfg.min_width, cfg.max_width), (3, 5));

        // `#` 前面是空白 → 注释；整行 `#` → 忽略
        let mut cfg2 = Config::default();
        cfg2.apply_pairs("# 整行注释\nmin_width = 4\t# 制表符也算空白");
        assert_eq!(cfg2.min_width, 4);
    }
}
