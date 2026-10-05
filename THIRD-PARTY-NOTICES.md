# 第三方声明（Third-Party Notices）

本仓库的 **`vendor/plugin-api/`** 与 **`vendor/plugin-sdk/`** 两个目录
取自 [lanyeeee/bilibili-video-downloader](https://github.com/lanyeeee/bilibili-video-downloader)，
依其 MIT 许可协议使用。

原始版权声明：

```
Copyright (c) 2025-2026 lanyeeee (https://github.com/lanyeeee)
```

许可协议正文见本仓库的 [LICENSE](LICENSE)（MIT）。

---

**为什么单独放一个文件？**

MIT 协议要求「上述版权声明与许可声明应包含在所有副本或实质性部分中」。
本文件保留**原始版权声明**，[LICENSE](LICENSE) 提供**许可正文**，两者共同满足该要求。

这样拆分还有个实际原因：GitHub 靠逐字匹配标准模板来识别开源协议，
如果把第三方声明直接写进 `LICENSE`，匹配会被打断，仓库首页就会显示成 `Other` 而不是 `MIT`。
