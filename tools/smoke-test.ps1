<#
.SYNOPSIS
    bdp-autopad 插件端到端冒烟测试（无需打开下载器 GUI）。

.DESCRIPTION
    用 P/Invoke 直接 LoadLibrary 加载编译好的 dll，做两件事：
      1. 调 bilibili_video_downloader_plugin_descriptor_v1，核对 descriptor 关键字段
      2. 构造 HookInputV1 JSON 喂给 bilibili_video_downloader_plugin_on_hook_v1，
         核对返回的 filename / episode_dir 是否被正确补零

    它模拟的是宿主 v0.2.1 真实的调用方式（同一套 extern "C" 符号、同样的 JSON 协议），
    所以能验证「插件在宿主里到底会不会生效」，而不只是单元测试过了就算数。

    全程只读：不会修改任何下载文件，也不会写 .下载任务/ 下的内容。
    （插件自身首次加载时会生成 autopad.toml，那是它的正常行为。）

.EXAMPLE
    .\tools\smoke-test.ps1
    .\tools\smoke-test.ps1 -DllPath ..\target\release\bdp_autopad.dll

.NOTES
    期望宽度 = 2，依赖两点：
      - autopad.toml 保持默认（min_width = 2、fixed_width = false）
      - 测试用的合集「魔女之旅」在本机只有 12 集
    若你改过配置或删过任务数据，宽度预期可能变化，届时按实际调整脚本里的 $Expected。
#>
[CmdletBinding()]
param(
    [string]$DllPath,
    # 配置覆盖路径的专用断言组：
    #   default — 29 项常规断言（依赖 autopad.toml 默认值）
    #   NoPad   — 设 AUTOPAD_ENABLED=0，**所有输入必须原样返回**
    #   Width3  — 设 AUTOPAD_MIN_WIDTH=3，宽度必须变成 3
    [ValidateSet('default', 'NoPad', 'Width3')]
    [string]$Expect = 'default'
)

$ErrorActionPreference = 'Stop'
# $PSScriptRoot 在 Windows PowerShell 5.1 的 param 默认值里是空的，改用 $MyInvocation
if (-not $DllPath) {
    $scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
    $DllPath = Join-Path $scriptDir "..\dist\bdp_autopad.dll"
}
$DllPath = (Resolve-Path $DllPath -ErrorAction Stop).Path
Write-Host "被测 dll: $DllPath" -ForegroundColor Cyan

# ---- 1. 加载并读取 descriptor -------------------------------------------
$csharp = @"
using System;
using System.Runtime.InteropServices;
using System.Text;

public static class Smoke {
    private const string D = @"$DllPath";

    [DllImport(D, CallingConvention = CallingConvention.Cdecl)]
    public static extern IntPtr bilibili_video_downloader_plugin_descriptor_v1();

    [DllImport(D, CallingConvention = CallingConvention.Cdecl)]
    public static extern int bilibili_video_downloader_plugin_on_hook_v1(
        byte[] input, UIntPtr inputLen, out IntPtr outPtr, out UIntPtr outLen);

    [DllImport(D, CallingConvention = CallingConvention.Cdecl)]
    public static extern void bilibili_video_downloader_plugin_free_buffer_v1(IntPtr p, UIntPtr len);

    [DllImport(D, CallingConvention = CallingConvention.Cdecl)]
    public static extern IntPtr bilibili_video_downloader_plugin_last_error_v1();

    private static string CStr(IntPtr p) {
        if (p == IntPtr.Zero) return null;
        int n = 0;
        while (Marshal.ReadByte(p, n) != 0) n++;
        byte[] b = new byte[n];
        Marshal.Copy(p, b, 0, n);
        return Encoding.UTF8.GetString(b);
    }

    public static string Descriptor() {
        return CStr(bilibili_video_downloader_plugin_descriptor_v1());
    }

    // 结果通过静态字段回传，避免在 C# 里引用 System.Text.Json（Add-Type 默认不加载它）
    public static int LastRc = 0;
    public static string LastOut = "";

    public static int RunHook(string json) {
        byte[] inBytes = Encoding.UTF8.GetBytes(json);
        IntPtr outPtr; UIntPtr outLen;
        int rc = bilibili_video_downloader_plugin_on_hook_v1(
            inBytes, (UIntPtr)inBytes.Length, out outPtr, out outLen);
        if (rc != 0) {
            LastRc = rc;
            LastOut = CStr(bilibili_video_downloader_plugin_last_error_v1());
            return rc;
        }
        int len = (int)outLen;
        byte[] buf = new byte[len];
        Marshal.Copy(outPtr, buf, 0, len);
        bilibili_video_downloader_plugin_free_buffer_v1(outPtr, outLen);
        LastRc = 0;
        LastOut = Encoding.UTF8.GetString(buf);
        return 0;
    }
}
"@
Add-Type -TypeDefinition $csharp -ErrorAction Stop

$script:pass = 0
$script:fail = 0
function Assert-Equal($name, $actual, $expected) {
    if ("$actual" -eq "$expected") {
        $script:pass++
        Write-Host ("  [PASS] {0}" -f $name) -ForegroundColor Green
    } else {
        $script:fail++
        Write-Host ("  [FAIL] {0}" -f $name) -ForegroundColor Red
        Write-Host ("         期望: {0}" -f $expected) -ForegroundColor Yellow
        Write-Host ("         实际: {0}" -f $actual) -ForegroundColor Yellow
    }
}

Write-Host "`n=== 1. descriptor ===" -ForegroundColor Cyan
$desc = [Smoke]::Descriptor() | ConvertFrom-Json
Assert-Equal "sdk_api_version"  $desc.sdk_api_version 1
Assert-Equal "id"               $desc.id              "bdp-autopad"
Assert-Equal "hooks"            ($desc.hooks -join ',') "AfterPrepare"
Assert-Equal "failure_policy"   $desc.failure_policy  "FailOpen"
if ($desc.id -ne "bdp-autopad") { Write-Host "descriptor 解析异常，无法继续" -ForegroundColor Red; exit 1 }

# ---- 2. 构造 hook 输入 ----------------------------------------------------
function New-HookInput {
    param(
        [string]$Collection,
        [int]$Order,
        [string]$EpisodeTitle,
        [string]$EpisodeDir,
        [string]$Filename
    )
    $progress = [ordered]@{
        task_id           = "smoke-test"
        episode_type      = "Bangumi"
        aid               = 1
        bvid              = "BV1smoke"
        cid               = 1
        ep_id             = 1
        duration          = 1441
        pub_ts            = 1608298201
        collection_title  = $Collection
        part_title        = $null
        part_order        = $null
        episode_title     = $EpisodeTitle
        episode_order     = $Order
        up_name           = $null
        up_uid            = $null
        up_avatar         = $null
        episode_dir       = $EpisodeDir
        filename          = $Filename
        video_task        = @{}
        audio_task        = @{}
        video_process_task= @{}
        subtitle_task     = @{}
        danmaku_task      = @{}
        cover_task        = @{}
        nfo_task          = @{}
        json_task         = @{}
        create_ts         = 1608298201
        completed_ts      = $null
        is_drm            = $false
        is_preview        = $false
    }
    return (@{
        hook_point   = "AfterPrepare"
        payload      = @{ AfterPrepare = @{ progress = $progress } }
        readonly_meta= @{ app_version = "0.2.1"; os = "windows"; arch = "x86_64"; process_id = 1 }
    } | ConvertTo-Json -Depth 8 -Compress)
}

function Invoke-Pad {
    param([string]$InputJson)
    $rc = [Smoke]::RunHook($InputJson)
    if ($rc -ne 0) { throw "hook 返回错误 rc=$rc : $([Smoke]::LastOut)" }
    $out = ([Smoke]::LastOut | ConvertFrom-Json)
    return $out.payload.AfterPrepare.progress
}

if ($Expect -ne 'default') {
    if ($Expect -eq 'NoPad') {
        $env:AUTOPAD_ENABLED = '0'
        Write-Host "`n=== [配置覆盖] AUTOPAD_ENABLED=0：必须一个都不改 ===" -ForegroundColor Cyan
    } else {
        $env:AUTOPAD_MIN_WIDTH = '3'
        Write-Host "`n=== [配置覆盖] AUTOPAD_MIN_WIDTH=3：宽度必须变 3 ===" -ForegroundColor Cyan
    }
    $base2 = "C:\Users\Admin\Videos\bilibili Download\魔女之旅"
    $cases = @(
        "第1话 羽丘的不可思议女孩",
        "第1話 序章",
        "1",
        "2.",
        "EP1 标题",
        "[1] 标题",
        "第1话 标题-P2 分P名",
        "1080p",
        "正片"
    )
    foreach ($one in $cases) {
        $p = Invoke-Pad (New-HookInput "魔女之旅" 1 "第1话 x" $base2 $one)
        if ($Expect -eq 'NoPad') {
            Assert-Equal "禁用后不动: $one" $p.filename $one
        } else {
            # 期望值逐字手写，不用实现去算——否则就是自己证明自己
            $exp = switch -Regex ($one) {
                '^第1话 羽丘的不可思议女孩$' { '第001话 羽丘的不可思议女孩' }
                '^第1話 序章$'               { '第001話 序章' }
                '^1$'                        { '001' }
                '^2\.$'                      { '002.' }
                '^EP1 标题$'                 { 'EP001 标题' }
                '^\[1\] 标题$'               { '[001] 标题' }
                '^第1话 标题-P2 分P名$'      { '第001话 标题-P002 分P名' }
                default                      { $one }
            }
            Assert-Equal "宽度3: $one" $p.filename $exp
        }
    }
    Write-Host ""
    Write-Host ("通过 {0} / 失败 {1}" -f $script:pass, $script:fail) -ForegroundColor $(if ($script:fail -eq 0) { "Green" } else { "Red" })
    if ($script:fail -eq 0) { Write-Host "配置覆盖冒烟通过 ✅" -ForegroundColor Green; exit 0 }
    Write-Host "配置覆盖冒烟存在失败 ❌" -ForegroundColor Red
    exit 1
}

Write-Host "`n=== 2. 补零行为（宽度=2，合集「魔女之旅」共 12 集）===" -ForegroundColor Cyan
$base = "C:\Users\Admin\Videos\bilibili Download\魔女之旅"

# 第N话：主路径（真实数据里 97.6% 是这个）
$p = Invoke-Pad (New-HookInput "魔女之旅" 1 "第1话 羽丘的不可思议女孩" $base "第1话 羽丘的不可思议女孩")
Assert-Equal "第1话 -> 第01话"      $p.filename "第01话 羽丘的不可思议女孩"

$p = Invoke-Pad (New-HookInput "魔女之旅" 12 "第12话 风吹浪打，亦不沉没" $base "第12话 风吹浪打，亦不沉没")
Assert-Equal "第12话 够长不动"       $p.filename "第12话 风吹浪打，亦不沉没"

# 纯数字（目录格式只写 {episode_order} 时）
$p = Invoke-Pad (New-HookInput "魔女之旅" 1 "第1话 x" $base "1")
Assert-Equal "裸数字 1 -> 01"        $p.filename "01"

$p = Invoke-Pad (New-HookInput "魔女之旅" 2 "第2话 x" $base "2.")
Assert-Equal "带点 2. -> 02."        $p.filename "02."

# 不该误伤
$p = Invoke-Pad (New-HookInput "魔女之旅" 1 "第1话 x" $base "1080p")
Assert-Equal "1080p 不动"            $p.filename "1080p"

$p = Invoke-Pad (New-HookInput "魔女之旅" 1 "第1话 x" $base "正片")
Assert-Equal "正片不动"              $p.filename "正片"

$p = Invoke-Pad (New-HookInput "魔女之旅" 1 "第1话 x" $base "OAD 感冒综合征")
Assert-Equal "OAD 标题不动"          $p.filename "OAD 感冒综合征"

$p = Invoke-Pad (New-HookInput "魔女之旅" 1 "第1话 x" $base "特别篇 拈花夜话")
Assert-Equal "特别篇不动"            $p.filename "特别篇 拈花夜话"

# 已补过零的不叠加
$p = Invoke-Pad (New-HookInput "魔女之旅" 7 "第7话 x" $base "第007话 已补过")
Assert-Equal "已补零不叠加"          $p.filename "第07话 已补过"

Write-Host "`n=== 3. episode_dir 只改末级、父级保持 ===" -ForegroundColor Cyan
$p = Invoke-Pad (New-HookInput "魔女之旅" 1 "第1话 x" "$base\1" "1")
Assert-Equal "子目录 1 -> 01"        $p.episode_dir "$base\01"
$p = Invoke-Pad (New-HookInput "魔女之旅" 1 "第1话 x" $base "第1话 x")
Assert-Equal "父级合集名不动"        $p.episode_dir $base

Write-Host "`n=== 4. 跨合集位数推导（95 集合集同样 2 位）===" -ForegroundColor Cyan
$long = "C:\Users\Admin\Videos\bilibili Download\成龙历险记 中文配音"
$p = Invoke-Pad (New-HookInput "成龙历险记 中文配音" 95 "第95话 终章" $long "第95话 终章")
Assert-Equal "第95话 够长不动"       $p.filename "第95话 终章"
$p = Invoke-Pad (New-HookInput "成龙历险记 中文配音" 3 "第3话 终章" $long "第3话 终章")
Assert-Equal "第3话 -> 第03话"       $p.filename "第03话 终章"

Write-Host "`n=== 5. 本轮新增格式（宽度=2）===" -ForegroundColor Cyan
$p = Invoke-Pad (New-HookInput "魔女之旅" 1 "第1話 羽丘的不可思议女孩" $base "第1話 羽丘的不可思议女孩")
Assert-Equal "第1話 -> 第01話"      $p.filename "第01話 羽丘的不可思议女孩"

$p = Invoke-Pad (New-HookInput "魔女之旅" 1 "第1巻 序章" $base "第1巻 序章")
Assert-Equal "第1巻 -> 第01巻"      $p.filename "第01巻 序章"

$p = Invoke-Pad (New-HookInput "魔女之旅" 1 "EP1 标题" $base "EP1 标题")
Assert-Equal "EP1 -> EP01"          $p.filename "EP01 标题"

$p = Invoke-Pad (New-HookInput "魔女之旅" 1 "ep.1 标题" $base "ep.1 标题")
Assert-Equal "ep.1 -> ep.01"        $p.filename "ep.01 标题"

$p = Invoke-Pad (New-HookInput "魔女之旅" 1 "第1话 x" $base "[1] 标题")
Assert-Equal "[1] -> [01]"          $p.filename "[01] 标题"

$p = Invoke-Pad (New-HookInput "魔女之旅" 2 "第2话 x" $base "【2】标题")
Assert-Equal "【2】 -> 【02】"      $p.filename "【02】标题"

$p = Invoke-Pad (New-HookInput "魔女之旅" 1 "第1话 x" $base "第1话 标题-P2 分P名")
Assert-Equal "-P2 -> -P02"          $p.filename "第01话 标题-P02 分P名"

# 新格式的反向守卫：regex crate 无环视，全靠手工边界判断，最怕误伤
$p = Invoke-Pad (New-HookInput "魔女之旅" 1 "第1话 x" $base "DEEP1")
Assert-Equal "DEEP1 不动"           $p.filename "DEEP1"

$p = Invoke-Pad (New-HookInput "魔女之旅" 1 "第1话 x" $base "[1080p]")
Assert-Equal "[1080p] 不动"         $p.filename "[1080p]"

$p = Invoke-Pad (New-HookInput "魔女之旅" 1 "第1话 标题-P1080" $base "第1话 标题-P1080")
Assert-Equal "-P1080 不动"          $p.filename "第01话 标题-P1080"

$p = Invoke-Pad (New-HookInput "魔女之旅" 1 "第1话 标题-P1080p" $base "第1话 标题-P1080p")
Assert-Equal "-P1080p 不动"         $p.filename "第01话 标题-P1080p"

$p = Invoke-Pad (New-HookInput "魔女之旅" 1 "EP1080p 标题" $base "EP1080p 标题")
Assert-Equal "EP1080p 不动"         $p.filename "EP1080p 标题"

# ---- 汇总 -----------------------------------------------------------------
Write-Host ""
Write-Host ("通过 {0} / 失败 {1}" -f $script:pass, $script:fail) -ForegroundColor $(if ($script:fail -eq 0) { "Green" } else { "Red" })
if ($script:fail -eq 0) {
    Write-Host "冒烟测试全部通过 ✅" -ForegroundColor Green
    exit 0
} else {
    Write-Host "冒烟测试存在失败项 ❌" -ForegroundColor Red
    exit 1
}
