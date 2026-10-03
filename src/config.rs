//! 插件配置：可选文件 + 环境变量覆盖。
//!
//! 零依赖的极简 TOML / key=value 解析，避免为一个小插件引入完整 TOML 解析器。

use std::path::PathBuf;
use std::sync::LazyLock;

pub const CONFIG_FILE_NAME: &str = "autopad.toml";

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
                Err(_) => {
                    eprintln!(
                        "[autopad] 未找到配置文件，使用默认值（如需自定义请创建 {}）",
                        path.display()
                    );
                }
            }
        }

        // 环境变量优先级高于配置文件，方便临时压测
        if let Ok(v) = std::env::var("AUTOPAD_MIN_WIDTH") {
            if let Ok(n) = v.trim().parse::<usize>() {
                cfg.min_width = n;
            }
        }
        if let Ok(v) = std::env::var("AUTOPAD_ENABLED") {
            cfg.enabled = !matches!(v.trim(), "0" | "false" | "no");
        }

        if cfg.max_width < cfg.min_width {
            cfg.max_width = cfg.min_width;
        }

        cfg
    }

    /// 解析 `key = value` 形式的配置，忽略 `#` 注释与空行。
    fn apply_pairs(&mut self, text: &str) {
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
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
            return self.min_width.clamp(1, self.max_width);
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
