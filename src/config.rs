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

/// 把当前进程的控制台输出代码页切成 UTF-8，让 `eprintln!` 里的中文不乱码。
///
/// 为什么需要：本插件的诊断日志全是中文，而 `eprintln!` 写出的是 **UTF-8 字节**。
/// Windows 控制台默认代码页是 GBK(936)，会把 UTF-8 字节按 GBK 解释 ——
/// 于是 `[autopad] 已读取配置` 显示成 `[autopad] 宸茶鍙栭厤缃`。
///
/// 只用 Win32 的 `SetConsoleOutputCP(65001)`，不引入任何 crate；
/// 非 Windows 平台是空实现（Unix 终端本就按字节直通，无此问题）。
///
/// **已知限制**：当 stderr 被**重定向**（管道 / 文件 / `2>&1`）时，Windows 不经
/// 控制台代码页而直接写字节，本调用无效——这是系统行为，插件侧无法绕过。
/// 那种场景下需要读取方自己按 UTF-8 解码（`tools/smoke-test.ps1` 已设置
/// `[Console]::OutputEncoding`）。真实运行（宿主 GUI / 直接开控制台）不受影响。
///
/// 失败时静默忽略 —— 日志能不能显示是体验问题，绝不能因此影响插件的主功能。
fn ensure_utf8_console() {
    #[cfg(windows)]
    {
        // 65001 = CP_UTF8。返回 0 表示失败，此处刻意忽略。
        const CP_UTF8: u32 = 65001;
        unsafe extern "system" {
            fn SetConsoleOutputCP(code_page: u32) -> i32;
        }
        // SAFETY: 调用无参数副作用的纯 Win32 API，无内存安全问题。
        unsafe {
            let _ = SetConsoleOutputCP(CP_UTF8);
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
        ensure_utf8_console();
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
    ///
    /// 已知 key 解析失败时**打日志**而不是静默吞掉：用户写 `enabled = ture`（拼错）
    /// 会保持默认值 true，如果连一条日志都没有，他会以为插件已经被关掉 ——
    /// 而环境变量路径（`AUTOPAD_ENABLED`）是会打日志的，两条路径行为必须一致。
    fn apply_pairs(&mut self, text: &str) {        for raw in text.lines() {
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
                "enabled" => self.enabled = apply_bool(self.enabled, key, value),
                "pad_filename" => {
                    self.pad_filename = apply_bool(self.pad_filename, key, value);
                }
                "pad_episode_dir" => {
                    self.pad_episode_dir = apply_bool(self.pad_episode_dir, key, value);
                }
                "fixed_width" => {
                    self.fixed_width = apply_bool(self.fixed_width, key, value);
                }
                "verbose" => self.verbose = apply_bool(self.verbose, key, value),
                "min_width" => self.min_width = apply_usize(self.min_width, key, value),
                "max_width" => self.max_width = apply_usize(self.max_width, key, value),
                // 未知 key 静默忽略是刻意的：向前兼容将来新增的配置项。
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

/// 解析布尔值；无法识别时保留 `current` 并打日志（静默 fallback 会让用户误以为配置生效）。
fn apply_bool(current: bool, key: &str, value: &str) -> bool {
    match parse_bool(value) {
        Some(b) => b,
        None => {
            eprintln!(
                "[autopad] 配置项 {key} 的值 {value:?} 无法识别（支持 1/0、true/false、yes/no、on/off），已保持 {current}"
            );
            current
        }
    }
}

/// 解析非负整数；无法识别时保留 `current` 并打日志（同上，避免静默忽略）。
fn apply_usize(current: usize, key: &str, value: &str) -> usize {
    match value.parse::<usize>() {
        Ok(n) => n,
        Err(_) => {
            eprintln!("[autopad] 配置项 {key} 的值 {value:?} 无法识别（需要非负整数），已保持 {current}");
            current
        }
    }
}

fn config_path() -> Option<PathBuf> {
    Some(crate::app_data_dir()?.join(CONFIG_FILE_NAME))
}

/// 首次加载时把默认配置写到磁盘，方便用户直接改。
///
/// 用 `create_new`（原子 `O_EXCL`）而不是「`exists()` 再 `write`」：
/// - `exists()` + `write` 之间存在 TOCTOU 窗口，宿主并发下载时多个 hook 会**同时**
///   通过检查并各写一次，`fs::write` 是 create+truncate+write，中途可能被
///   `Config::load` 的 `read_to_string` 读到半截内容（配置与预期不符）；
/// - `create_new` 由文件系统保证原子性：已存在直接返回 `AlreadyExists`，天然幂等。
///
/// 写失败时打日志而不是 `let _ =` 静默吞掉 —— 用户会以为配置已生成好等着去改。
pub fn write_default_config_if_absent() {
    let Some(path) = config_path() else { return };

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

    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => {
            // 全路径引用 `Write`，避免在模块顶层引入可能与宏展开冲突的名字。
            if let Err(err) = std::io::Write::write_all(&mut file, text.as_bytes()) {
                eprintln!("[autopad] 写入默认配置失败: {} ({err})", path.display());
            }
        }
        // 已存在 = 正常情况（用户改过或上次已生成），不是错误，静默返回。
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
        // 父目录不存在等：不是致命问题（本次仍会用默认值运行），但要让用户知道。
        Err(err) => {
            eprintln!(
                "[autopad] 无法创建默认配置文件 {}: {err}（本次仍按默认值运行）",
                path.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

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

    /// 回归：布尔值拼错时必须**保持原值**（而不是被静默吞掉后用户以为生效了）。
    /// 这里同时钉住「保持原值」这一行为契约，日志走 stderr 无法在断言里直接看。
    #[test]
    fn unparsable_known_keys_keep_current_value() {
        let mut cfg = Config {
            enabled: true,
            min_width: 2,
            max_width: 4,
            pad_filename: true,
            ..Config::default()
        };
        // `ture` 是 `true` 的常见拼错
        cfg.apply_pairs(
            "enabled = ture\n\
             pad_filename = flase\n\
             min_width = abc\n\
             max_width = 3.5\n\
             verbose = maybe\n",
        );
        assert!(cfg.enabled, "拼错不得改变默认 true");
        assert!(cfg.pad_filename, "拼错不得改变默认 true");
        assert_eq!(cfg.min_width, 2, "非法整数保持原值");
        assert_eq!(cfg.max_width, 4, "小数不是合法 usize，保持原值");
        assert!(!cfg.verbose, "`maybe` 无法识别，保持默认 false");

        // 合法值必须照常生效（确认上面不是「全部忽略」造成的假通过）
        let mut ok = Config::default();
        ok.apply_pairs("enabled = off\nmin_width = 3\nmax_width = 5\nverbose = yes\n");
        assert!(!ok.enabled);
        assert_eq!(ok.min_width, 3);
        assert_eq!(ok.max_width, 5);
        assert!(ok.verbose);
    }

    /// 回归：默认配置的创建必须是原子的「仅当不存在」。
    /// 旧实现用 `exists()` + `fs::write`，存在 TOCTOU 与并发双写。
    /// 直接验证底层语义：`create_new` 在文件已存在时必须返回 `AlreadyExists`
    /// 且**不覆盖**已有内容（即不会把用户改过的配置冲掉）。
    #[test]
    fn default_config_creation_is_atomic_and_never_overwrites() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp")
            .join(format!("cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("autopad.toml");

        // 第一次：create_new 成功
        let first = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path);
        assert!(first.is_ok(), "首次创建应成功: {first:?}");
        first.unwrap().write_all(b"enabled = false\n").unwrap();

        // 第二次：必须 AlreadyExists，且原内容不被覆盖
        let second = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path);
        match second {
            Err(e) => assert_eq!(
                e.kind(),
                std::io::ErrorKind::AlreadyExists,
                "已存在时必须是 AlreadyExists，供调用方静默返回"
            ),
            Ok(_) => panic!("已存在的文件不得被 create_new 打开并截断"),
        }
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "enabled = false\n",
            "用户已有配置必须原样保留"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
