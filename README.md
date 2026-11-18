# NoWatermark

去除 Windows 桌面右下角的水印（例如 Insider 预览版的“评估副本 / Insider Preview / Build xxxxx”），**不修改任何系统文件、不常驻后台、不需要管理员权限**。

Remove the Windows desktop watermark (e.g. "Evaluation copy / Insider Preview") without modifying system files, without a resident process, and without admin rights.

---

## 特性

- 🪶 **不常驻**：每次登录运行一次，打完补丁约 5 秒内自动退出
- 🛡️ **不改系统文件**：只修改当前用户 `explorer.exe` 进程内存，`sfc` / 系统更新都不受影响
- 🔑 **无需管理员**：安装到 `%LOCALAPPDATA%`，通过当前用户的 `Run` 注册表项开机运行
- 🔄 **自动适配系统更新**：函数地址从微软公共符号服务器按 shell32 版本自动解析并缓存，Insider 换版本无需改代码
- 🧹 **随时可逆**：卸载后重启资源管理器，水印即恢复

## 使用

1. 从 [Releases](https://github.com/MiChongs/no-watermark/releases/latest) 下载 `NoWatermark.exe`，或从源码编译（见下文）
2. 双击运行，弹出菜单：
   - **是** —— 安装：复制到 `%LOCALAPPDATA%\NoWatermark\`，立即去除水印，之后每次登录自动去除
   - **否** —— 卸载：移除开机启动项，可选择立即重启资源管理器恢复水印
   - **取消** —— 退出

### 命令行

| 参数 | 作用 |
| --- | --- |
| （无） | 交互菜单（安装 / 卸载） |
| `--install` | 安装并立即生效 |
| `--uninstall` | 卸载（重启资源管理器后水印恢复） |
| `--patch` | 只对当前运行的资源管理器去除一次水印 |
| `--silent` | 登录模式：等待桌面就绪后去除水印，失败自动重试，然后退出 |

日志：`%LOCALAPPDATA%\NoWatermark\log.txt`

## 原理

桌面水印由 `explorer.exe` 加载的 `shell32.dll` 中的 `CDesktopWatermark::s_DesktopBuildPaint` 绘制。NoWatermark 会：

1. 找到当前会话中所有 `explorer.exe` 进程，以及其中 `shell32.dll` 的加载基址
2. 从内存中的 PE 调试目录读取 CodeView（RSDS）记录，得到 PDB 的 GUID/Age
3. 从 `https://msdl.microsoft.com/download/symbols` 下载对应的 `shell32.pdb`，在公共符号中找到 `?s_DesktopBuildPaint@CDesktopWatermark@@...` 的 RVA，并缓存到 `%LOCALAPPDATA%\NoWatermark\symbols.txt`（同一版本只下载一次）
4. 校验目标地址位于可执行页后，把函数第一个字节改为 `ret`（`0xC3`），使其不再绘制任何内容
5. 水印是一次性渲染进缓存图层的，普通重绘不会刷新，所以再向桌面窗口 `Progman`（仅此窗口，不广播）发送 `WM_SETTINGCHANGE(SPI_SETNONCLIENTMETRICS, "WindowMetrics")` 让资源管理器重建图层

补丁只存在于资源管理器进程内存中，资源管理器重启即失效，因此通过登录时运行一次来保持效果。

> 为什么不直接 hook `CDesktopWatermark::s_WantWatermark`？在较新的 Insider 版本（如 Build 29708）中，该函数已不在绘制路径上，hook 它不再起作用。

## 编译

需要 Rust（MSVC 工具链）：

```powershell
cargo build --release
# 产物：target\release\NoWatermark.exe
```

## 注意事项

- 资源管理器在会话中途崩溃或被重启时，水印会重新出现；下次登录会自动去除，也可以再次运行 `NoWatermark.exe` 选择“是”（或 `--patch`）立即去除
- 首次遇到新的 shell32 版本时需要联网下载约十几 MB 的符号文件；登录时网络未就绪会每 15 秒重试，最长 10 分钟
- 如果某次系统更新重命名或移除了 `s_DesktopBuildPaint`，日志中会提示找不到符号，此时工具不会做任何修改
- 已在 Windows 11 Insider Preview Build 29708（x64）上验证；仅支持 x64

## 免责声明

本工具仅用于隐藏桌面上的提示文字，不涉及也不改变 Windows 的激活或授权状态。修改进程内存存在一定风险，请自行评估后使用。

## 许可证

[MIT](LICENSE)
