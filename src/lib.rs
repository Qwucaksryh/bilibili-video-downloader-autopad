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

/// 抓取「第N话 / 第N集 / 第N期」（含日文写法「第N話」U+8A71、「第N巻」U+5DFB，
/// 以及中文简体「第N卷」U+5377、「第N單」等）中间的数字，容忍空格。
/// 宿主已按 Windows 文件名规则过滤过标题，不会有正则意义上的怪字符。
///
/// 注意：不在此处 `use std::sync::LazyLock`，否则会和 `export_plugin_v1!` 宏
/// 展开出来的同名导入冲突（E0252）。这里全路径引用即可。
///
/// 字符类里的两个易混字：
/// - `巻` U+5DFB（日文新字体「巻」）—— 与 `卷` U+5377（中文简体）**不是同一个字**，
///   两者都要收：B 站中文标题写「第1卷」，日文标题写「第1巻」；
/// - `單` U+55AE（繁体）、`集` U+96C6、`期` U+671F、`話` U+8A71。
static EPISODE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"第\s*(\d+)\s*[话話集期卷巻單]").expect("正则编译失败")
});

/// 没有「第N话」时的兜底：抓开头的纯数字，例如 `1`、`2.`、`1 - 标题`、`03、xxx`。
///
/// 这里刻意**不写** `(?!\d)` / `(?![A-Za-z])`：`regex` crate 不支持环视。
/// `\d+` 本身是贪婪的，已经会一次吃完整段数字；「后面不能是字母」的判断放在
/// [`pad_leading`] 里用代码完成。
///
/// 由于只补位不截断，`2024`、`1000` 这种本身就够长的数字也不会被改动。
static LEADING_NUM_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"^\s*(\d+)").expect("正则编译失败"));

/// 抓取 `EP1` / `EP01` / `Ep 1` / `ep.1` 形态的集数前缀（大小写不敏感，`EP` 与数字之间
/// 容忍空格与可选的 `.`）。
///
/// `regex` crate 不支持环视，「前面不能是单词内部」「后面不能跟字母」两条边界规则
/// 都放到 [`pad_ep`] 里用代码手工判断（照 [`pad_leading`] 的做法）。
static EP_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?i)ep\s*\.?\s*(\d+)").expect("正则编译失败"));

/// 抓取**文件名开头**的方括号编号：`[01]`、`【01】`（方括号内允许前导空白）。
///
/// 只认开头（`^` 锚定）；「方括号里是 `[1080p]` 这类规格值」的判断在 [`pad_bracket`] 里做。
static BRACKET_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"^(\s*[\[【]\s*)(\d+)").expect("正则编译失败"));

/// 抓取分P序号：字面量 `-P`（横杠 + 大写 P）+ 数字。
///
/// 数字长度上限与「后面必须是结尾/非字母数字」的守卫放在 [`pad_part`] 里用代码判断，
/// 避免误伤 `-P1080` 这类规格值，也不吃进后面的标题文字。
static PART_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"-P(\d+)").expect("正则编译失败"));

#[derive(Default)]
struct AutoPadPlugin;

impl PluginV1 for AutoPadPlugin {
    /// **纯函数，禁止任何 IO。**
    ///
    /// 实测依据（`vendor/plugin-sdk/src/lib.rs`）：
    /// - `DESCRIPTOR_JSON_V1` 是 `LazyLock`，初始化体直接调 `instance.descriptor()`；
    /// - `descriptor_v1()` 是 `extern "C"` 导出，**外面没有 catch_unwind**
    ///   （全 SDK 只有 `on_hook_v1` 有 panic → 错误码 的保护）。
    ///
    /// 所以 descriptor 里一旦 panic，会顺着 `LazyLock::as_ptr()` 抛到
    /// `extern "C"` 边界 → Rust 直接 **abort** → 下载器进程崩溃，
    /// FailOpen 也救不了。原来这里的 `write_default_config_if_absent()`
    /// 做着真实磁盘 IO（`path.exists()` + `fs::write`），已挪到 `on_hook()`。
    fn descriptor(&self) -> PluginDescriptorV1 {
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
        // 必须在 `Config::get()` 之前落盘，这样第一次读配置就能读到默认值
        // （语义与旧实现等价：插件加载时 descriptor 先写、随后 hook 才读）。
        // 这里是 on_hook，已被 SDK 的 catch_unwind 覆盖，IO 出错最多记日志。
        config::write_default_config_if_absent();

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
/// 可识别的形态（依次尝试、可同时命中多条，例如 `第1话 标题-P2 分P名` 两条都要处理）：
/// 1. `第N话 / 第N集 / 第N期 / 第N話 / 第N巻` —— 就地补零，前后文字与扩展名原样保留
/// 2. `EP1 / EP01 / Ep 1 / ep.1` —— 大小写不敏感的 EP 前缀
/// 3. 文件名开头的 `[01]`、`【01】` 方括号编号
/// 4. 分P序号 `-P2`
/// 5. 以上都没命中时，取开头那段纯数字兜底（目录格式只写了 `{episode_order}`，
///    名字就是 `1`、`2.`、`1 - 标题`）
///
/// 所有情况都只补位、不截断：`2024`、`1000`、`-P1080` 这种本身就够长的数字一律不动。
fn pad_text(text: &str, width: usize) -> String {
    let mut matched = false;
    let mut out = text.to_string();

    // 注意：这里不能写成 `EPISODE_RE.is_match(&out)` —— 「正则能匹配」不等于
    // 「替换后真的有变化」。数字超出 u32 时 `splice_digits` 会静默返回原文，
    // 此时若仍把 matched 置 true，第 190 行的兜底 `pad_leading` 就会被跳过，
    // 出现「正则认了、但一个字符都没改」的静默漏补。改为按输出是否变化判定。
    let (next, hit) = replace_episode(&out, width);
    out = next;
    matched |= hit;
    let (next, hit) = pad_ep(&out, width);
    out = next;
    matched |= hit;
    let (next, hit) = pad_bracket(&out, width);
    out = next;
    matched |= hit;
    let (next, hit) = pad_part(&out, width);
    out = next;
    matched |= hit;

    if matched {
        out
    } else {
        pad_leading(&out, width)
    }
}

/// 把 `第N话` 里的 N 补零，返回 `(新文本, 是否真的发生了变化)`。
///
/// 返回「是否变化」而不是「正则是否命中」，是为了让 [`pad_text`] 的 `matched`
/// 标志忠实反映实际改动 —— 否则 u32 溢出等无法补零的情况会把 `matched` 置真，
/// 白白吃掉兜底 `pad_leading`。
fn replace_episode(text: &str, width: usize) -> (String, bool) {
    if !EPISODE_RE.is_match(text) {
        return (text.to_string(), false);
    }
    let out = EPISODE_RE
        .replace_all(text, |caps: &regex::Captures<'_>| {
            splice_digits(&caps[0], &caps[1], width)
        })
        .into_owned();
    let hit = out != text;
    (out, hit)
}

/// 把 `EP1 / EP01 / Ep 1 / ep.1` 里的数字补零，返回 `(新文本, 是否命中)`。
///
/// `regex` crate 不支持环视（lookaround），两条边界规则只能用代码手工判断
/// （同 [`pad_leading`] 的做法）：
/// - **匹配起点的前一个字符不能是 ASCII 字母/数字**：否则 `DEEP1`、`STEP3`、`keep9`、
///   `HDD2` 这类单词内部的 `ep` 会被误伤；
/// - **数字后面不能直接跟 ASCII 字母**：否则 `EP1080p` 这种规格值会被误伤。
///
/// 命中判定与 [`replace_episode`] 一致：按「输出是否真的变化」算，不看正则是否匹配。
fn pad_ep(text: &str, width: usize) -> (String, bool) {
    let mut out = String::with_capacity(text.len());
    let mut last = 0usize;
    let mut matched = false;

    for caps in EP_RE.captures_iter(text) {
        let Some(whole) = caps.get(0) else { continue };
        let (start, end) = (whole.start(), whole.end());
        let prev_is_word = text[..start]
            .chars()
            .next_back()
            .map_or(false, |c| c.is_ascii_alphanumeric());
        let next_is_alpha = text[end..].starts_with(|c: char| c.is_ascii_alphabetic());
        if prev_is_word || next_is_alpha {
            continue;
        }
        let replaced = splice_digits(whole.as_str(), &caps[1], width);
        if replaced == whole.as_str() {
            // 数字没变（例如 EP100 配 width 2、或 u32 溢出）：不算命中，
            // 让文本原样流过去，交给后面的规则 / 兜底处理。
            continue;
        }
        out.push_str(&text[last..start]);
        out.push_str(&replaced);
        last = end;
        matched = true;
    }

    if !matched {
        return (text.to_string(), false);
    }
    out.push_str(&text[last..]);
    (out, true)
}

/// 把文件名开头的 `[01]` / `【01】` 编号补零，返回 `(新文本, 是否命中)`。
///
/// 「方括号里是 `[1080p]` 这类规格值」的判断没法写成 `(?![A-Za-z])`（`regex` crate
/// 不支持环视），只能取出来用代码判断，同 [`pad_leading`]。
///
/// 命中判定与 [`replace_episode`] 一致：按「输出是否真的变化」算。
fn pad_bracket(text: &str, width: usize) -> (String, bool) {
    let Some(caps) = BRACKET_RE.captures(text) else {
        return (text.to_string(), false);
    };
    let Some(whole) = caps.get(0) else {
        return (text.to_string(), false);
    };
    let end = whole.end();
    if text[end..].starts_with(|c: char| c.is_ascii_alphabetic()) {
        return (text.to_string(), false);
    }
    let padded = pad_number(&caps[2], width);
    if padded == caps[2] {
        return (text.to_string(), false); // 数字没变：不算命中。
    }
    let mut out = String::with_capacity(text.len());
    out.push_str(&caps[1]);
    out.push_str(&padded);
    out.push_str(&text[end..]);
    (out, true)
}

/// 把分P序号 `-P2` 补零成 `-P02`，返回 `(新文本, 是否命中)`。
///
/// 「`regex` 不支持环视」的边界规则全部用代码手工判断：
/// - 只认字面量 `-P`（横杠 + 大写 P），`1080p`、`HDP` 里没有 `-P`，天然不命中；
/// - 数字长度 ≤ 2 才处理：分P序号极少超过 99，`-P1080`（4 位）直接跳过，不误伤规格值；
/// - 数字后面必须是结尾或非字母数字字符，防止吃进后面的标题文字；
/// - 只补位不截断：`-P2` 配 width 3 → `-P002`；`-P10` 配 width 2 → `-P10`（不动）。
fn pad_part(text: &str, width: usize) -> (String, bool) {
    let mut out = String::with_capacity(text.len());
    let mut last = 0usize;
    let mut matched = false;

    for caps in PART_RE.captures_iter(text) {
        let Some(whole) = caps.get(0) else { continue };
        let (start, end) = (whole.start(), whole.end());
        let digits = &caps[1];
        if digits.len() > 2 {
            continue;
        }
        let next_ok = text[end..].chars().next().map_or(true, |c| !c.is_alphanumeric());
        if !next_ok {
            continue;
        }
        let replaced = splice_digits(whole.as_str(), digits, width);
        if replaced == whole.as_str() {
            continue; // 数字没变：不算命中（同 replace_episode 的判定口径）。
        }
        out.push_str(&text[last..start]);
        out.push_str(&replaced);
        last = end;
        matched = true;
    }

    if !matched {
        return (text.to_string(), false);
    }
    out.push_str(&text[last..]);
    (out, true)
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

/// 把纯数字串补零到 `width` 位（只补位、不截断；解析失败原样返回）。
fn pad_number(digits: &str, width: usize) -> String {
    match digits.parse::<u32>() {
        Ok(order) => format!("{:0>width$}", order, width = width),
        Err(_) => digits.to_string(),
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

    #[test]
    fn pads_japanese_episode_markers() {
        let cases = [
            // 日文写法：第N話（U+8A71）、第N巻（U+5DFB）
            ("第1話 羽丘的不可思议女孩", 3, "第001話 羽丘的不可思议女孩"),
            ("第1巻 上", 2, "第01巻 上"),
            ("第12話 标题", 3, "第012話 标题"),
            ("第9巻", 2, "第09巻"),
            // 已补零 / 小数 / 够长的都不动
            ("第007話 已经补过了", 3, "第007話 已经补过了"),
            ("第1.5話 特别篇", 3, "第1.5話 特别篇"),
            ("第100話 新的开始", 2, "第100話 新的开始"),
        ];
        for (input, width, expected) in cases {
            assert_eq!(pad_text(input, width), expected, "input={input} width={width}");
        }
    }

    #[test]
    fn pads_ep_prefix() {
        let cases = [
            // 正向：EP1 / EP01 / Ep 1 / ep.1，补零但保留原有写法
            ("EP1 标题", 2, "EP01 标题"),
            ("EP01 标题", 2, "EP01 标题"),
            ("Ep 1", 2, "Ep 01"),
            ("ep.1", 2, "ep.01"),
            ("【EP2】", 3, "【EP002】"),
            // 只补不截
            ("EP100 标题", 2, "EP100 标题"),
            // 反向：单词内部的 "ep" 不能命中（前导字母边界判断）
            ("DEEP1", 2, "DEEP1"),
            ("STEP3", 2, "STEP3"),
            ("HDD2", 2, "HDD2"),
            ("keep9", 2, "keep9"),
            // 反向：数字后面跟 ASCII 字母的是规格值
            ("EP1080p", 2, "EP1080p"),
            // 主路径与 EP 边界共存：第N话 照常补零，step3 不被 EP 规则误伤
            ("第1话 step3 标题", 2, "第01话 step3 标题"),
        ];
        for (input, width, expected) in cases {
            assert_eq!(pad_text(input, width), expected, "input={input} width={width}");
        }
    }

    #[test]
    fn pads_leading_bracket_numbers() {
        let cases = [
            // 正向：开头的 [01] / 【01】
            ("[1] 标题", 3, "[001] 标题"),
            ("【2】标题", 3, "【002】标题"),
            ("[01] 标题", 3, "[001] 标题"),
            ("【002】 标题", 3, "【002】 标题"),
            // 反向：不在开头的方括号不动
            ("标题 [01]", 2, "标题 [01]"),
            // 反向：方括号里是规格值 / 字母（fansub 命名常见 [1080p][HEVC]）
            ("[1080p] 标题", 2, "[1080p] 标题"),
            ("[AB12] 标题", 2, "[AB12] 标题"),
        ];
        for (input, width, expected) in cases {
            assert_eq!(pad_text(input, width), expected, "input={input} width={width}");
        }
    }

    #[test]
    fn pads_part_order_suffix() {
        let cases = [
            // 正向：分P序号 -P2 -> -P02（可与第N话同时命中）
            ("第1话 标题-P2 分P名", 2, "第01话 标题-P02 分P名"),
            ("第1话 标题-P2", 3, "第001话 标题-P002"),
            ("EP3 标题-P9", 2, "EP03 标题-P09"),
            // 只补不截
            ("标题-P10", 2, "标题-P10"),
            // 反向：P1080 这类规格值不动（数字超过 2 位直接跳过）
            ("标题-P1080", 4, "标题-P1080"),
            ("标题-P1080p", 2, "标题-P1080p"),
            // 反向：只认大写 P
            ("标题-p2", 2, "标题-p2"),
        ];
        for (input, width, expected) in cases {
            assert_eq!(pad_text(input, width), expected, "input={input} width={width}");
        }
    }

    #[test]
    fn leaves_real_numberless_titles_untouched() {
        // 取自真实下载数据里完全没有集数的 10 个标题，任何规则都不许命中
        let titles = [
            "OAD 感冒综合征",
            "番外篇 比武招亲",
            "特别篇 拈花夜话",
            "原版",
            "正片",
            "中文",
            "卓易通下四款容器类应用使用分享(gspace、gbox、ourplay、元萝卜、google商店、月圆之夜、华为全家桶)",
        ];
        for title in titles {
            assert_eq!(pad_text(title, 3), title, "title={title}");
        }
    }

    /// 小数集号只补整数部分：数值没变，读起来仍然是 1.5 集。
    /// 这条是**钉住现状**的行为测试——`EP1.5` 会变成 `EP01.5`，我们认可这个结果。
    #[test]
    fn pads_only_integer_part_of_decimal_numbers() {
        let cases = [
            ("EP1.5 标题", 2, "EP01.5 标题"),
            ("1.5 特别篇", 2, "01.5 特别篇"),
            ("第1.5话 特别篇", 3, "第1.5话 特别篇"),
        ];
        for (input, width, expected) in cases {
            assert_eq!(pad_text(input, width), expected, "input={input} width={width}");
        }
    }

    /// 幂等性：补过零的结果再跑一次必须完全不变（否则会 第01话 → 第001话 叠加）。
    #[test]
    fn padding_is_idempotent() {
        let samples = [
            ("第1话 羽丘的不可思议女孩", 3),
            ("第007话 已经补过了", 3),
            ("1", 2),
            ("2.", 2),
            ("[1] 标题", 2),
            ("【2】标题", 3),
            ("EP3 标题-P9", 2),
            ("第1話 标题", 2),
            ("1080p", 2),
            ("正片", 3),
        ];
        for (input, width) in samples {
            let once = pad_text(input, width);
            let twice = pad_text(&once, width);
            assert_eq!(once, twice, "幂等性被破坏: input={input} width={width}");
        }
    }

    /// 多种写法叠在同一个标题里时，每一处都该各补各的。
    #[test]
    fn pads_all_formats_stacked_in_one_title() {
        assert_eq!(pad_text("[1] 第2话 EP3 -P4", 2), "[01] 第02话 EP03 -P04");
        assert_eq!(pad_text("[10] 第2話 标题", 3), "[010] 第002話 标题");
    }

    /// 回归：`matched` 必须按「输出是否真的变化」判定，而不是「正则是否匹配」。
    ///
    /// 旧实现在 `EPISODE_RE.is_match` 为真时无条件 `matched = true`；当数字超出 u32
    /// 时 `splice_digits` 会静默返回原文，于是「正则认了、却一个字符都没改」的情况下
    /// `matched` 仍为真，兜底 `pad_leading` 被白白跳过 —— 开头那段本该补零的数字
    /// 就这么漏掉了。修复后这些输入应正常走兜底路径。
    #[test]
    fn episode_match_without_change_falls_through_to_leading_fallback() {
        // 数字溢出 u32：`第N话` 补不了零，但开头还有一段可补的数字
        let cases = [
            // 开头 1 位数字 + 后文有超长「第N话」→ 开头必须补零
            ("1 第99999999999话", 2, "01 第99999999999话"),
            ("12 第4294967296话 标题", 3, "012 第4294967296话 标题"),
            // 开头已经是 2 位，width 2 时不变（幂等，不算回归）
            ("12 第4294967296话", 2, "12 第4294967296话"),
        ];
        for (input, width, expected) in cases {
            assert_eq!(pad_text(input, width), expected, "input={input} width={width}");
        }
    }

    /// 中文简体「卷」（U+5377）与日文「巻」（U+5DFB）是两个不同的字，都要认。
    #[test]
    fn pads_both_chinese_and_japanese_juan_markers() {
        assert_eq!('卷' as u32, 0x5377, "简体「卷」的码点");
        assert_eq!('巻' as u32, 0x5DFB, "日文「巻」的码点");
        let cases = [
            // 中文简体「卷」—— B 站中文标题常见，旧实现会漏补
            ("第1卷 上", 2, "第01卷 上"),
            ("第12卷 下", 3, "第012卷 下"),
            ("第1卷 序章", 3, "第001卷 序章"),
            // 日文「巻」保持原行为
            ("第1巻 上", 2, "第01巻 上"),
            // 繁体「單」
            ("第1單 上", 2, "第01單 上"),
            // 已有格式不受影响
            ("第1话 标题", 2, "第01话 标题"),
            ("第1話 标题", 2, "第01話 标题"),
            // 只补不截
            ("第100卷 终章", 2, "第100卷 终章"),
        ];
        for (input, width, expected) in cases {
            assert_eq!(pad_text(input, width), expected, "input={input} width={width}");
        }
    }

    /// `replace_episode` 的返回值必须忠实反映「是否真的改了」。
    #[test]
    fn replace_episode_reports_actual_change() {
        use super::replace_episode;
        // 命中且变更
        assert_eq!(replace_episode("第1话 标题", 2), ("第01话 标题".to_string(), true));
        // 命中但不变（已够长）→ hit 必须是 false
        assert_eq!(replace_episode("第10话 标题", 2), ("第10话 标题".to_string(), false));
        // 命中但溢出无法补 → hit 必须是 false
        assert_eq!(
            replace_episode("第4294967296话", 2),
            ("第4294967296话".to_string(), false)
        );
        // 完全不匹配 → hit 必须是 false
        assert_eq!(replace_episode("正片", 2), ("正片".to_string(), false));
    }

    /// 分P 视频用的是另一套目录格式（与普通视频分开）：
    /// `{collection_title}/{episode_title}/{episode_title}-P{part_order} {part_title}`
    ///
    /// 要求：集数只在**末级目录**和**文件名**里补零，合集目录（倒数第二级）一律不动，
    /// 否则文件夹名与模板对不上。
    #[test]
    fn pads_part_video_directory_layout() {
        let dir = Path::new(r"C:\dl\动画、游戏专辑\第1话 樱之诗OST");
        assert_eq!(
            pad_path(dir, 2),
            Path::new(r"C:\dl\动画、游戏专辑\第01话 樱之诗OST"),
            "合集目录不动，末级 episode_title 补零"
        );

        // 合集目录本身含数字也不许动
        let dir2 = Path::new(r"C:\dl\第2季\第1话 标题");
        assert_eq!(
            pad_path(dir2, 2),
            Path::new(r"C:\dl\第2季\第01话 标题"),
            "父级 第2季 必须原样"
        );

        // 文件名 = episode_title + `-P` + part_order + 空格 + part_title
        // 集数与分P 序号两处都补，且用同一个宽度，所以两处对齐
        assert_eq!(
            pad_text("第1话 樱之诗OST-P1 30.夢の歩みを見上げて", 2),
            "第01话 樱之诗OST-P01 30.夢の歩みを見上げて"
        );
        assert_eq!(
            pad_text("第1话 标题-P2 分P名", 3),
            "第001话 标题-P002 分P名"
        );
        // 分P 数字已够长时只补不截（官方示例 -P30 必须原样保留）
        assert_eq!(
            pad_text("【高音质】樱之诗OST-P30 30.夢の歩みを見上げて", 2),
            "【高音质】樱之诗OST-P30 30.夢の歩みを見上げて"
        );
        // part_title 的自身序号 `30.` 已经两位，不叠加
        assert_eq!(pad_text("01.イントロ", 2), "01.イントロ");
        assert_eq!(pad_text("1.イントロ", 2), "01.イントロ");
    }
}
