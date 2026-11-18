# Maxa Device

Maxa 的设备端应用：把可信宿主上的文件与命令执行、Windows 桌面自动化、Android 控制统一到一个 Tauri 应用中。

桌面端提供 **Windows x64** 安装包；另支持 Android arm64 设备通过用户安装的 Termux、无界面 Agent 和伴随 APK 直接作为宿主，无需 Termux:API 插件。手机目标仅支持 Android。Linux/macOS 宿主、iOS 和云设备不在当前支持范围。

## 下载与使用

从 [Latest Release](https://github.com/maxzhao/maxa-device/releases/latest) 下载 Windows 安装包，可用同页 `SHA256SUMS` 核验文件。当前安装包未做代码签名；遇到系统警告时，请先核对下载来源与摘要。

1. 安装并启动 Maxa Device。Windows 需要 WebView2 Runtime；首次安装可能需要联网获取。
2. 按需开启 System、Windows 或 Mobile 模块。App 保存你的开关选择，重启后分别恢复；模块启动失败会显示原因，不影响其他模块，也不会重放旧操作。新安装及没有历史设置的旧版本默认全部关闭，需要设置一次。System 和 Mobile 的固定组件随安装包提供，不要求全局安装 Python、Node 或 ADB。
3. 在 Maxa 端发现设备，或输入设备的 IP/HTTPS 地址发起配对。
4. 核对双方显示的验证码，由人在目标 App 中批准。批准后保持配对，直到解除配对或撤销客户端。
5. 使用 Android 时，在手机上完成系统 USB 调试授权或无线调试配对；厂商权限、锁屏和其他自动化客户端占用都可能限制操作。

默认启动到 Windows 右下角系统托盘，通过托盘的“打开”或“设置”进入窗口；再次启动会打开已有实例。关闭窗口保留托盘服务，明确退出才停止受管工作，且不清除模块选择。停止按钮取消当前操作，不会关闭模块；需要持续禁止访问时，请关闭模块、移除手机或撤销客户端。

设置页可切换当前用户的 Windows 登录自启（默认关闭）、启动到托盘（默认开启）、深色／浅色／系统主题，以及简体中文／English／系统语言。主题和语言默认跟随系统，不支持的系统语言回退英文；更改自动保存在本机。设置不影响配对、已撤销客户端或已移除手机。

## 模块

| 模块 | 能力 | 运行组件 |
| --- | --- | --- |
| System | 远程文件、搜索、命令、受管任务 | Rust backend、固定版本 ripgrep |
| Windows | 原生 UI 树、截图、输入、窗口与虚拟桌面；14 个工具 | 同一 App 可执行文件派生的受管 worker |
| Mobile | Android 元素、点击、输入、截图、录屏等；30 个工具 | 私有 ADB、loopback-only 手机辅助 JAR |

Windows 模块面向活动交互桌面。虚拟桌面接口包含 Win10 build 19041+ 分支，但 Win10 和 Win11 22621 分支尚未经过实机验收；不支持的环境会明确报错，不应把构建成功当作全平台验证。

## 文件复制

Maxa 的 `copy` builtin 可在本机和已配对设备之间复制二进制文件，也支持显式递归复制目录。两端分别指定路径和设备；远端要求 System 开启及刷新后的 copy v1 能力。目标路径是精确结果路径，不自动追加源目录名；默认不覆盖、不跟随链接或重解析点。

复制前完整检查源清单、目标权限／冲突及空间，全部通过后才创建目标内容和传输字节。单文件最大 100 GB（100000000000 bytes），目录累计大小不限。传输流式执行并逐文件校验 SHA-256／原子提交；失败或取消保留已提交成员、清理未完成文件，不提供目录整体回滚或断点续传。旧 `read/write` 仍只用于 UTF-8 文本。

## 安全边界

- **配对是高信任授权，不是目录沙箱。** 已配对客户端可使用 App 当前及以后由人开启的模块；实际访问仍受宿主用户、操作系统及设备权限约束。
- 默认 HTTPS 入口为 `0.0.0.0:19427`，mDNS 用于局域网发现。仅在可信网络中开放所需连接，不要直接暴露到公网；防火墙与系统权限需要由人配置。
- 配对凭据保存在本机受保护数据区，不应进入 Git、项目配置、日志或模型上下文。发现地址本身不代表已认证。
- 断线、停止、撤销和模块关闭不会撤销已经发生的副作用。结果未知时先查询状态，不要盲目重放命令或输入。
- Android 辅助程序经 ADB shell 推送并运行，使用设备的 UiAutomation；不要求安装输入法 APK，不得与其他自动化客户端同时争用。

## 从源码构建

正式 Windows 安装包使用 Windows 原生构建。需要 Node.js 22.12+、Rust/MSVC 工具链、Visual Studio C++ Build Tools、Windows SDK，以及打包时可用的网络。Tauri 管理 NSIS 打包工具。

先在 Linux x86_64（可用 WSL）执行 `bash android/agent/build.sh`、`bash android/build-companion.sh`，生成同版本 Agent、伴随 APK、对应源码包和校验清单。将 `android/dist/companion/` 原样放入同一源码版本的 Windows 构建目录。

在 Windows PowerShell 中，进入本仓库根目录：

```powershell
npm ci

$archives = Join-Path $env:TEMP ('maxa-device-archives-' + [guid]::NewGuid())
$stage = Join-Path $env:TEMP ('maxa-device-packs-' + [guid]::NewGuid())
New-Item -ItemType Directory -Path $archives | Out-Null
$c = Get-Content ./components.json -Raw | ConvertFrom-Json
Invoke-WebRequest $c.runtimeSources.ripgrep.url -OutFile (Join-Path $archives 'ripgrep-15.1.0-x86_64-pc-windows-msvc.zip')
Invoke-WebRequest $c.runtimeSources.adb.url -OutFile (Join-Path $archives 'platform-tools_r37.0.1-win.zip')
./build-packs.ps1 -RuntimeArchives $archives -BuildDirectory $stage

npm run prepare:licenses
npm run check:backend
./build-release.ps1
```

`build-packs.ps1` 校验固定归档摘要，拒绝复用已有 staging 目录。Mobile 包仅保留 ADB 必需文件与许可，不打包 fastboot、文件系统工具或解释器。`build-release.ps1` 将 Rust/native 编译路径映射为通用路径，避免在公开二进制中嵌入本机用户目录。安装包位于 `src-tauri/target/release/bundle/nsis/`；发布时必须附带 `release/third-party-sources.tar.gz` 和分发材料，详见 [THIRD-PARTY.md](./THIRD-PARTY.md)。

WSL/Linux 可运行 `npm run build` 和 `npm run check:backend`。Windows 交叉构建还需要单独配置 MSVC 兼容工具链、SDK 和打包工具，不能只安装 Rust Windows target。

## 项目归属与许可

本仓库独立维护 App 源码、依赖锁、组件清单、构建和用户文档。行为规格及 proposal/delta 仍由 Maxa 主项目的 `.maxa/specs/` 维护，不复制到本仓库；Maxa 的连接器、builtin 和 Ability 也仍由主项目维护。

仓库根许可为 [Apache-2.0](./LICENSE)；含 Termux:API 派生代码的组合伴随 APK 按 GPLv3 分发。其[对应源码说明](./android/companion/licenses/SOURCE.md)和[内置 API 清单](./android/companion/EMBEDDED-API.md)随源码提供。第三方组件保留各自许可证，参见 [第三方分发说明](./THIRD-PARTY.md)、`licenses/` 和 release 中的依赖源码包。
