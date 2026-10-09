# bdp-autopad · 自动补零插件

给 **[哔哩哔哩视频下载器](https://github.com/lanyeeee/bilibili-video-downloader)**（`lanyeeee/bilibili-video-downloader`）写的插件：下载时自动把集数补零。

- `第1话 羽丘的不可思议女孩` → `第01话 羽丘的不可思议女孩`
- 目录格式只写 `{episode_order}` 时的裸数字 `1` → `01`
- **补几位不用你填**——插件自己从下载任务里推断合集集数

> **⚠️ 关于本仓库**
> 代码与文档由 **AI 撰写**，但**已经过实测可用**：
> 34 项单元测试全部通过 → `tools\smoke-test.ps1` 用 `P/Invoke` 真实加载 dll 跑通端到端 hook 测试（**29 项断言**，另含 `NoPad` / `Width3` 两组配置覆盖共 13+13 项）→
> 已在 bilibili-video-downloader **v0.2.1 中实际加载并正常运行**。
> 本轮的格式扩展与探测性能重写由 AI 智能体团队分工完成，改完后另有一轮独立只读审计。
> 审计发现的 21 项问题已按其优先级修复（详见 `审查报告` 提交），修复后测试从 26 项增至 34 项。
> 欢迎提 issue。

**不用编译，直接用**：[`dist/bdp_autopad.dll`](dist/bdp_autopad.dll) 就是编译好的成品，
也可以在 [Releases](https://github.com/Qwucaksryh/bilibili-video-downloader-autopad/releases) 里直接下载。

---

## 为什么必须写插件

下载器自带的目录格式模板做不到这件事。它用的 `strfmt` 格式化库**明确不支持零填充**：

```
{episode_order:03}  →  error: sign aware zero padding and Align '=' not yet supported
```

而 `{episode_title}` 只是普通字符串，模板引擎不会去解析里面的 `第1话` 再补零。所以在模板层是无解的。

好在宿主暴露了 `AfterPrepare` 钩子，会在**创建目录之前**把 `episode_dir` / `filename` 交给插件改写——本插件就是在这里下手。

---

## 安装（3 步）

1. 把 [`dist/bdp_autopad.dll`](dist/bdp_autopad.dll) 放到一个**不会被清理的固定目录**，例如
   `C:\Users\<你>\Videos\bilibili-plugin\bdp_autopad.dll`
   （后端要求**绝对路径**且文件必须存在，别放临时目录或下载文件夹）

2. 打开下载器 → **设置** → **插件**面板 → 点 **「添加插件」** → 选中该 dll

3. 确认状态显示 **「已加载」**，然后开始下载

> 插件出错时按 `FailOpen` 记录日志并继续，**不会中断你的下载任务**。

---

## 补零规则

### 位数由合集集数自动决定

插件读取 `%APPDATA%\com.lanyeeee.bilibili-video-downloader\.下载任务\*.json`，按 `collection_title` 聚合出 `episode_order` 的最大值：

| 合集集数 | 输出宽度 | 例 |
|---|---|---|
| ≤ 99 | 2 位 | `第01话` |
| ≤ 999 | 3 位 | `第001话` |
| ≤ 9999 | 4 位 | `第0001话` |

### 支持的写法

| 输入 | 12 集合集的输出 |
|---|---|
| `第1话 羽丘的不可思议女孩` | `第01话 羽丘的不可思议女孩` |
| `第12话 风吹浪打，亦不沉没` | `第12话 风吹浪打，亦不沉没`（够长就不动） |
| `第1話 羽丘的不可思议女孩` | `第01話 羽丘的不可思议女孩`（日文「話」） |
| `第1巻 序章` | `第01巻 序章`（日文「巻」） |
| `第1卷 序章` | `第01卷 序章`（简体「卷」） |
| `EP1 标题` / `Ep 1` / `ep.1` | `EP01 标题` / `Ep 01` / `ep.01` |
| `[1] 标题` / `【2】标题` | `[01] 标题` / `【02】标题` |
| `1`（纯数字） | `01` |
| `2.` | `02.` |
| `1 - 标题` | `01 - 标题` |
| `1080p` / `128kbps` / `2024` | 原样不动 |
| `正片` / `原版` / `第1.5话 特别篇` | 原样不动 |
| `OAD 感冒综合征` / `特别篇 拈花夜话` | 原样不动（没有集数） |
| `DEEP1` / `STEP3` / `HDD2` | 原样不动（数字后面还跟着字母） |
| `[1080p]` / `[AB12]` | 原样不动（方括号里不是集数） |
| `-P1080` / `-P1080p` / `-p2` | 原样不动（分P 守卫） |

- 主路径：`第N话` / `第N集` / `第N期` / `第N話` / `第N巻`
- 也支持 `EP1`、`Ep 1`、`ep.1` 前缀写法
- 也支持行首方括号编号 `[1]`、`【2】`
- 也支持纯数字开头的文件名（`1`、`2.`、`1 - 标题`）
- 也支持分P 序号后缀（`-P2` → `-P02`）
- `.mp4` 扩展名始终原样保留
- 只补位、不截断，补过零的不会叠加

> `regex` crate **不支持环视**，以上所有"不该动"的边界判断都是用代码手工做的（取匹配后看前后字符），
> 所以每条守卫都有对应测试覆盖，见下方 `tools/smoke-test.ps1`。

---

## 配置

首次加载会在下载器数据目录自动生成：

```
%APPDATA%\com.lanyeeee.bilibili-video-downloader\autopad.toml
```

```toml
enabled = true        # 总开关
min_width = 2         # 宽度下限（2 = 12 集的季番也写成 第01话）
max_width = 4         # 宽度上限
fixed_width = false   # true = 忽略自动探测，一律用 min_width
pad_filename = true   # 文件名里的集数是否补零
pad_episode_dir = true
verbose = false       # true = 往控制台打探测日志
```

**改完需要重启下载器**（配置只在进程启动后读一次）。
也可以用环境变量临时覆盖：`AUTOPAD_MIN_WIDTH=3`、`AUTOPAD_ENABLED=0`。

---

## 影响范围

| 维度 | 情况 |
|---|---|
| 运行位置 | 只在下载器进程内，靠 `AfterPrepare` 钩子调用 |
| 改什么 | 只改本次任务的 `episode_dir` / `filename` |
| 老文件 | 不会动已经下载好的文件（钩子只在下载流程中触发） |
| 其他软件 | 完全不碰，不注册全局钩子、不装服务、不开机自启 |

### 权限

**先说清楚：宿主的插件系统没有任何沙箱或权限约束**——插件是进程内动态库，与下载器**同权限**运行，官方 README 也明确警告过这一点。

所以下面这些**不是系统强制的最小权限，而是本插件代码自身的实际接触面**：

- 只声明了 3 个钩子中的 **1 个**（`AfterPrepare`），`BeforeVideoProcess` / `OnCompleted` 完全不参与
- **从未调用宿主提供的唯一 Host API**（`host::get_config()`），因此接触不到 `sessdata` 等敏感配置
- **静态导入表里没有网络组件**——没有 `ws2_32`（socket）、`winhttp`、`wininet`；也没有 `advapi32`（注册表）、`shell32`（COM/shell），只有 `KERNEL32` / `ntdll` / UCRT 系列 / `bcryptprimitives`
- 文件系统只碰三处：**读** `.下载任务\*.json`、**读** `autopad.toml`、**仅当配置不存在时**写一份默认 `autopad.toml`
- 环境变量只读 `APPDATA` / `HOME`，以及可选的 `AUTOPAD_MIN_WIDTH` / `AUTOPAD_ENABLED`

自查导入表：

```powershell
objdump -p dist/bdp_autopad.dll | findstr "DLL Name"
```

> 诚实补充：没有沙箱意味着技术上插件**可以**调用更多 API（例如动态 `LoadLibrary`），上表只说明**它现在没有**。要长期安心，请自行审阅 [src/](src) 源码——一共 3 个文件、约 1100 行（不含测试）。

---

## 自己编译

工具链：**Rust (GNU target)** + **mingw-w64**（提供 `gcc` / `as` / `dlltool`，缺了会在 `parking_lot_core` 处报 `dlltool could not create import library`）。

```powershell
cd bdp-autopad
cargo test --release     # 25 个单元测试
cargo build --release    # 产物 target\release\bdp_autopad.dll
```

### 端到端冒烟测试（不用开下载器 GUI）

`cargo test` 只能证明补零函数写对了，**证明不了宿主真的会这么调用**。
`tools\smoke-test.ps1` 用 P/Invoke 直接加载 dll，按宿主 v0.2.1 真实的
`extern "C"` 符号 + `HookInputV1` JSON 协议喂输入，逐项核对输出：

```powershell
# 必须 -ExecutionPolicy Bypass（系统默认禁止运行脚本）
powershell -NoProfile -ExecutionPolicy Bypass -File tools\smoke-test.ps1
# 或直接测刚编出来的：
powershell -NoProfile -ExecutionPolicy Bypass -File tools\smoke-test.ps1 -DllPath target\release\bdp_autopad.dll
```

三个断言组，对应三条不同的代码路径：

| `-Expect` | 验证什么 | 断言数 |
|---|---|---|
| `default`（不传） | 常规补零行为（依赖 `autopad.toml` 默认值） | 29 |
| `NoPad` | 设 `AUTOPAD_ENABLED=0` 后必须一个都不改 | 13 |
| `Width3` | 设 `AUTOPAD_MIN_WIDTH=3` 后宽度必须变 3 | 13 |

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File tools\smoke-test.ps1 -Expect NoPad
powershell -NoProfile -ExecutionPolicy Bypass -File tools\smoke-test.ps1 -Expect Width3
```

全程只读，不碰任何下载文件；环境变量只在子进程里生效，不会改你的 `autopad.toml`。

> 脚本文件头必须是 **UTF-8 带 BOM**，否则 Windows PowerShell 5.1 会按 GBK 解码，
> 中文字符串全乱（这是踩过的坑之一）。

### 工程结构

```
├── Cargo.toml            # cdylib；release 关掉了 panic=abort（见下）
├── src/
│   ├── lib.rs            # AfterPrepare 钩子 + 补零逻辑 + 单元测试
│   ├── config.rs         # autopad.toml 解析（零依赖 key=value）
│   └── episode_scan.rs   # 扫 .下载任务/*.json 聚合集数
├── tools/
│   └── smoke-test.ps1    # P/Invoke 端到端冒烟测试
├── vendor/               # 宿主 v0.2.1 的 plugin-api / plugin-sdk（MIT）
└── dist/bdp_autopad.dll  # 编译产物
```

### 踩过的坑（改代码前必读）

1. **`regex` crate 不支持环视**（`(?=...)`、`(?!...)`），写进去会直接编译失败。
   「数字后面不能是字母」这条规则只能取出来用代码判断。
2. **`Cargo.toml` 里绝不能写 `panic = "abort"`**。SDK 的 `export_plugin_v1!` 靠
   `catch_unwind` 把插件异常转成错误码，配合 `FailOpen` 让宿主记日志继续跑；
   改成 abort 会让插件里任何 panic **直接崩掉整个下载器进程**。
3. **不能 `use std::sync::LazyLock`**——`export_plugin_v1!` 宏展开时也导入了它，
   同作用域重复导入报 E0252，改用全路径引用。
4. **宿主只校验 `task_id` 不可改**；`episode_dir` / `filename` 可自由修改，
   而且 `create_dir_all` 在 hook 返回**之后**才执行，所以改动会真正生效。
5. 写 `plugin.json` 必须用**无 BOM 的 UTF-8**——宿主是
   `serde_json::from_str(...).unwrap_or_default()`，带 BOM 会静默解析失败变成空表，
   插件看着写进去了却根本不加载。

---

## 许可证

[MIT](LICENSE)，与原项目相同。

`vendor/` 目录下的 `plugin-api` / `plugin-sdk` 取自
[lanyeeee/bilibili-video-downloader](https://github.com/lanyeeee/bilibili-video-downloader)（MIT），
原始版权声明见 [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md)。

> 该插件系统是**实验性 v1**：插件以进程内动态库形式运行，**与宿主同权限、无沙箱**。
> 装第三方插件时请自行评估代码。
