//! bdp-autopad —— 为「第N话」自动补零的 bilibili-video-downloader 插件。
//!
//! 宿主的 `dir_fmt` 用的是 strfmt 0.2.4，而 strfmt 明确不支持零填充
//! （`{episode_order:03}` 会撞上 "sign aware zero padding and Align '=' not yet supported"），
//! 且 `{episode_title}` 里的 `第1话` 只是普通字符串，模板层无法补零。
//!
//! 因此本插件在 `AfterPrepare` 钩子里直接改宿主已经渲染好的 `episode_dir` / `filename`：
//! 宿主是在 hook 返回**之后**才 `create_dir_all`，所以改动会真正生效。
//! 补零宽度由该合集的已知集数自动推导（读取 `.下载任务/*.json` 聚合 `episode_order`）。

mod config;
mod episode_scan;

use std::path::{Path, PathBuf};

use bilibili_video_downloader_plugin_sdk::{
    AfterPreparePayloadV1, HookInputV1, HookOutputV1, HookPayloadV1, HookPointV1,
    PluginDescriptorV1, PluginFailurePolicy, PluginV1, SDK_API_VERSION, export_plugin_v1,
    eyre,
};
use regex::Regex;

/// 抓取「第N话 / 第N集 / 第N期」中间的数字，容忍空格。
/// 宿主已按 Windows 文件名规则过滤过标题，不会有正则意义上的怪字符。
///
/// 注意：不在此处 `use std::sync::LazyLock`，否则会和 `export_plugin_v1!` 宏
/// 展开出来的同名导入冲突（E0252）。这里全路径引用即可。
static EPISODE_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"第\s*(\d+)\s*[话集期]").expect("正则编译失败"));

/// 没有「第N话」时的兜底：抓开头的纯数字，例如 `1`、`2.`、`1 - 标题`、`03、xxx`。
///
/// 这里刻意**不写** `(?!\d)` / `(?![A-Za-z])`：`regex` crate 不支持环视。
/// `\d+` 本身是贪婪的，已经会一次吃完整段数字；「后面不能是字母」的判断放在
/// [`pad_leading`] 里用代码完成。
///
/// 由于只补位不截断，`2024`、`1000` 这种本身就够长的数字也不会被改动。
static LEADING_NUM_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"^\s*(\d+)").expect("正则编译失败"));

#[derive(Default)]
struct AutoPadPlugin;

impl PluginV1 for AutoPadPlugin {
    fn descriptor(&self) -> PluginDescriptorV1 {
        config::write_default_config_if_absent();
        PluginDescriptorV1 {
            sdk_api_version: SDK_API_VERSION,
            id: "bdp-autopad".to_string(),
            name: "自动补零 第01话".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            hooks: vec![HookPointV1::AfterPrepare],
            failure_policy: PluginFailurePolicy::FailOpen,
            description: "把「第1话」补零成各合集所需位数的「第01话」，集数自动从下载任务推导（可配置）"
                .to_string(),
        }
    }

    fn on_hook(&mut self, input: HookInputV1) -> eyre::Result<HookOutputV1> {
        let cfg = config::Config::get();
        if !cfg.enabled {
            return Ok(HookOutputV1 {
                payload: input.payload,
            });
        }

        // 只处理我们声明过的钩子点
        if input.hook_point != HookPointV1::AfterPrepare {
            return Ok(HookOutputV1 {
                payload: input.payload,
            });
        }

        let HookPayloadV1::AfterPrepare(AfterPreparePayloadV1 { mut progress }) = input.payload
        else {
            return Err(eyre::eyre!("hook_point 与 payload 不匹配"));
        };

        let detected = episode_scan::detect_max_order(&progress.collection_title);
        let max_order = detected.max(progress.episode_order);
        let width = cfg.width_for(max_order);

        if cfg.verbose {
            eprintln!(
                "[autopad] {} | 合集「{}」集数={} 宽度={} | dir={} | file={}",
                progress.episode_title,
                progress.collection_title,
                max_order,
                width,
                progress.episode_dir.display(),
                progress.filename
            );
        }

        if cfg.pad_episode_dir {
            progress.episode_dir = pad_path(&progress.episode_dir, width);
        }
        if cfg.pad_filename {
            progress.filename = pad_text(&progress.filename, width);
        }

        Ok(HookOutputV1 {
            payload: HookPayloadV1::AfterPrepare(AfterPreparePayloadV1 { progress }),
        })
    }
}

/// 只替换路径最后一级的名字，保留父级不变。
fn pad_path(path: &Path, width: usize) -> PathBuf {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return path.to_path_buf();
    };
    let padded = pad_text(name, width);
    if padded == name {
        return path.to_path_buf();
    }
    match path.parent() {
        Some(parent) => parent.join(padded),
        None => PathBuf::from(padded),
    }
}

/// 把集数补零到指定位数。
///
/// 两类形态：
/// 1. `第N话 / 第N集 / 第N期` —— 就地补零，前后文字与扩展名原样保留
/// 2. 根本没有「第N话」时（目录格式只写了 `{episode_order}`，名字就是 `1`、`2.`、`1 - 标题`），
///    取开头那段纯数字补零
///
/// 两种情况都只补位、不截断：`2024`、`1000` 这种本身就够长的数字一律不动。
fn pad_text(text: &str, width: usize) -> String {
    if EPISODE_RE.is_match(text) {
        return replace_episode(text, width);
    }
    pad_leading(text, width)
}

/// 把 `第N话` 里的 N 补零。
fn replace_episode(text: &str, width: usize) -> String {
    EPISODE_RE
        .replace_all(text, |caps: &regex::Captures<'_>| {
            splice_digits(&caps[0], &caps[1], width)
        })
        .into_owned()
}

/// 没有「第N话」时，把开头的纯数字补零。
///
/// `regex` crate 不支持环视（lookaround），所以「数字后面不能跟字母」这条规则
/// 没法写成 `(?![A-Za-z])`，只能取出来再用代码判断。
fn pad_leading(text: &str, width: usize) -> String {
    let Some(caps) = LEADING_NUM_RE.captures(text) else {
        return text.to_string();
    };
    let whole = caps.get(0).map_or("", |m| m.as_str());
    let rest = &text[whole.len()..];
    // `1080p`、`128kbps` 这类以数字开头的规格名要跳过
    if rest.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return text.to_string();
    }
    format!("{}{}", splice_digits(whole, &caps[1], width), rest)
}

/// 把 `whole` 中的数字串替换成补零后的形式，其余字符原样保留。
fn splice_digits(whole: &str, digits: &str, width: usize) -> String {
    let Ok(order) = digits.parse::<u32>() else {
        return whole.to_string();
    };
    // `{:0>N}` 只补位、不截断，所以 `100` 配 width 2 仍是 `100`
    let padded = format!("{:0>width$}", order, width = width);
    match whole.find(digits) {
        Some(idx) => format!("{}{}{}", &whole[..idx], padded, &whole[idx + digits.len()..]),
        None => whole.to_string(),
    }
}

/// 数据目录：`%APPDATA%\com.lanyeeee.bilibili-video-downloader`
pub(crate) fn app_data_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .map(|p| p.join("com.lanyeeee.bilibili-video-downloader"))
    } else {
        std::env::var_os("HOME").map(|h| {
            PathBuf::from(h)
                .join(".local")
                .join("share")
                .join("com.lanyeeee.bilibili-video-downloader")
        })
    }
}

/// 下载任务目录：`<app_data_dir>/.下载任务`
pub(crate) fn task_dir() -> Option<PathBuf> {
    Some(app_data_dir()?.join(".下载任务"))
}

export_plugin_v1!(AutoPadPlugin);

#[cfg(test)]
mod tests {
    use super::{pad_path, pad_text};
    use std::path::Path;

    #[test]
    fn pads_real_titles() {
        // 取自真实下载数据
        let cases = [
            ("第1话 羽丘的不可思议女孩", 3, "第001话 羽丘的不可思议女孩"),
            ("第1话 太子新娘", 2, "第01话 太子新娘"),
            ("第9话 决战", 2, "第09话 决战"),
            ("第10话 EMMA", 2, "第10话 EMMA"),
            ("第95话 大结局", 2, "第95话 大结局"),
            ("第100话 新的开始", 2, "第100话 新的开始"),
            ("第12集 标题", 3, "第012集 标题"),
            ("第3期 特别节目", 3, "第003期 特别节目"),
            // 已经补过零的不许叠加
            ("第007话 已经补过了", 3, "第007话 已经补过了"),
            ("第007话 现在要两位", 2, "第07话 现在要两位"),
            // 不匹配的保持原样
            ("原版", 3, "原版"),
            ("正片", 3, "正片"),
            // 小数集号：数字后面不是「话」字，不匹配就保持原样，不猜
            ("第1.5话 特别篇", 3, "第1.5话 特别篇"),
        ];
        for (input, width, expected) in cases {
            assert_eq!(pad_text(input, width), expected, "input={input} width={width}");
        }
    }

    #[test]
    fn pads_pure_numeric_names() {
        // 目录格式没有「第N话」时，名字就是裸数字
        let cases = [
            ("1", 2, "01"),
            ("2.", 2, "02."),
            ("9", 3, "009"),
            ("1 - 标题", 2, "01 - 标题"),
            ("03、标题", 3, "003、标题"),
            ("10", 3, "010"),
            ("99", 3, "099"),
            ("100", 3, "100"),
            // 只补不截：够长的一律不动
            ("2024", 2, "2024"),
            ("1080p", 2, "1080p"),
            ("128kbps", 4, "128kbps"),
            ("AB12", 4, "AB12"),
        ];
        for (input, width, expected) in cases {
            assert_eq!(pad_text(input, width), expected, "input={input} width={width}");
        }
    }

    #[test]
    fn preserves_extension_in_paths() {
        // 极端情况：filename 意外带了扩展名，扩展名必须原样留下
        assert_eq!(pad_path(Path::new(r"C:\dl\1.mp4"), 3), Path::new(r"C:\dl\001.mp4"));
        assert_eq!(pad_path(Path::new(r"C:\dl\2..mkv"), 2), Path::new(r"C:\dl\02..mkv"));
        assert_eq!(pad_path(Path::new(r"C:\dl\1080p.mkv"), 2), Path::new(r"C:\dl\1080p.mkv"));
    }

    #[test]
    fn pads_only_last_path_component() {
        let path = Path::new(r"C:\Users\Admin\Videos\bilibili Download\第1话 羽丘的不可思议女孩");
        let got = pad_path(path, 3);
        assert_eq!(
            got,
            Path::new(r"C:\Users\Admin\Videos\bilibili Download\第001话 羽丘的不可思议女孩")
        );
    }

    #[test]
    fn width_from_detected_episodes() {
        let cfg = crate::config::Config::default();
        assert_eq!(cfg.width_for(8), 2, "8 集 -> 至少 2 位");
        assert_eq!(cfg.width_for(95), 2, "95 集 -> 2 位");
        assert_eq!(cfg.width_for(100), 3, "100 集 -> 3 位");
        assert_eq!(cfg.width_for(1000), 4, "1000 集 -> 4 位");
        assert_eq!(cfg.width_for(0), 2, "探测不到时退回下限");
        assert_eq!(cfg.width_for(99999), 4, "受 max_width 限制");
    }
}
