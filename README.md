# Linux.do Accelerator

<div align="center">
  <img src="./assets/icons/128x128.png" alt="Linux.do Accelerator" width="88" />
  <h1>Linux.do Accelerator</h1>
  <p>一个原生 Rust 的 <code>linux.do</code> 专属加速器，提供 <b>CLI + 桌面 GUI</b> 双形态。</p>

  <p>
    <img alt="Rust" src="https://img.shields.io/badge/Rust-Native-orange?style=flat-square" />
    <img alt="Platforms" src="https://img.shields.io/badge/Platform-Windows%20%7C%20Linux%20%7C%20macOS%20%7C%20Android-1f6feb?style=flat-square" />
    <img alt="Package" src="https://img.shields.io/badge/Package-EXE%20%7C%20DEB%20%7C%20DMG%20%7C%20APK-2da44e?style=flat-square" />
    <img alt="Mode" src="https://img.shields.io/badge/Mode-CLI%20%2B%20Desktop%20%2B%20Android-6f42c1?style=flat-square" />
  </p>

  <p>
    <a href="./CHANGELOG.md">更新日志 / Changelog</a>
  </p>
</div>

<p align="center">
  <img src="./docs/images/gui-preview.png" alt="Linux.do Accelerator GUI Preview" width="480" />
</p>

## Star History

[![Star History Chart](https://api.star-history.com/svg?repos=fjh1997%2FLinux.do-Accelerator&type=Date)](https://www.star-history.com/#fjh1997/Linux.do-Accelerator&Date)

## Overview

`linuxdo-accelerator` 的目标很直接：

- 一键生成并安装本地根证书
- 一键写入和清理 `hosts`
- 本地监听 `127.211.73.84:80/443`
- 为 `linux.do` 及其子域提供本地接管和转发
- 同时支持脚本场景下的 `CLI`，以及普通用户可双击使用的桌面 GUI

## Why This Exists

相信大多佬友都是用梯子访问论坛的，但看之前的[我的帖子](https://linux.do/t/topic/1763604/21)和这个[帖子](https://linux.do/t/topic/1761457/63)，`linux.do` 实际上是被 `SNI` 阻断的。运营商检测到 `linux.do` 的 `SNI` 后，会直接发出 `RST` 包，导致连接被重置。

类似 [`steamcommunity302`](https://www.dogfight360.com/blog/18682/)、`Watt Toolkit`、`dev-sidecar` 这类项目，很多是通过 `SNI` 伪造来解决问题。但 `linux.do` 运行在 Cloudflare 上，而 Cloudflare 不支持这套 `SNI` 伪造方案，所以这条路走不通。

至少以目前这个项目的验证结果来看，直接依赖传统 `SNI` 伪造并不现实。至于 `SNI` 窃取 / 冒用同 CDN 站点，或者把 `SNI` 设空这类思路，我这里还没有做完验证，所以 README 里不把它们当作当前可用方案，只先作为可能方向保留判断。

对 `linux.do` 来说，比较可行的办法是通过 `ECH` 对 `SNI` 进行加密，从而绕过运营商的阻断。但问题又来了：运营商会拦截 `linux.do` 正常 DNS 返回里的 `ECH key`，导致浏览器拿不到密钥，自然也就无法完成 `ECH`。

这也是这个项目存在的原因。

- 通过私人 `DoH` 获取可用的 `ECH key`
- 支持在配置文件中填入多个 `DoH`
- 支持缓存，避免每次都重新解析
- 避免把私人 `DoH` 配成系统全局，减少额外负载
- 目前测试可在 `IPv6` 网络环境下加速 `linux.do`

几个关键点：

- 项目可以无需梯子一键加速 `linux.do`
- 默认内置的是秦始皇的 `DoH`，同时也支持你自己配置多个私人 `DoH`
- 支持 `Linux`、`Windows`、`macOS` 三端
- 额外提供 Android 非 Root 版
- 项目所有流量都在本地处理，没有任何第三方服务器转发
- 需要安装系统证书，并会监听 `127.211.73.84` 上的 `80/443` 端口
- 理论上也支持其他被阻断、但支持 `ECH` 的网站，不过这类站点很少

如有 bug，欢迎反馈和 PR。

## Highlights

- 原生 Rust 实现，不依赖 Node 运行时
- GUI 与后台代理逻辑分离，窗口关闭或最小化后后台仍可继续工作
- 支持系统提权，适合证书安装、`hosts` 写入和 `127.211.73.84:80/443` 低端口监听
- 配置项集中在单个 `linuxdo-accelerator.toml`
- Android 非 Root 版通过 DNS 代理接管 `linux.do` 相关解析，无需 Root、无需安装证书
- 三端统一思路：
  - Windows：双击打开 `.exe`
  - Linux：安装 `.deb` 后桌面启动
  - macOS：拖入 `Applications` 后直接打开

## Android

Android 版目前走的是 DNS 代理接管方案：

- 无需 Root
- 无需安装证书
- 只接管 `linux.do` 及其子域的 DNS
- 普通域名仍走系统默认 DNS

当前限制也比较明确：

- 推荐使用 Chrome、Edge 这类支持 `ECH / HTTPS RR` 的浏览器
- 系统自带浏览器和 WebView 不一定支持 `ECH`，在部分设备上可能仍会超时
- 后续可能继续提供 Root 版，配合系统证书安装改善系统浏览器和 WebView 兼容性

## GUI

桌面端默认提供：

- `开始加速 / 停止加速`
- 一键最小化
- 错误详情展示
- 当前上游、DoH、证书和域名接管范围预览
- 配置和关于面板

平台行为：

- Windows：支持托盘最小化与恢复
- Linux：Wayland / GNOME 下使用托盘代理恢复窗口
- macOS：支持最小化到 Dock，已接入菜单栏图标恢复链路

## 合规说明

本项目定位为本地网络接管与调试工具，主要在用户自己的设备上完成以下操作：

- 本地监听指定回环地址和端口
- 本地生成或安装证书，用于本机 HTTPS 接管测试
- 按配置文件对指定域名进行 DNS / hosts / 本地代理处理

本项目不提供境外代理服务器、中转节点或专用传输通道，不自建国际通信出入口，也不承诺可用于绕过任何地区、网络或平台的访问控制要求。用户访问目标站点时，相关网络连接仍依赖用户现有网络服务提供者提供的通信链路。

用户应自行确认其使用场景符合所在地法律法规、平台规则和单位内部制度。请勿将本项目用于任何违法违规用途；如用于企业、组织或公开分发场景，建议事先咨询专业律师或合规顾问。

## Quick Start

初始化默认配置：

```bash
cargo run --bin linuxdo-accelerator -- init-config
```

准备证书和 `hosts`：

```bash
sudo cargo run --bin linuxdo-accelerator -- setup
```

前台直接启动：

```bash
sudo cargo run --bin linuxdo-accelerator -- start
```

停止后台加速：

```bash
sudo cargo run --bin linuxdo-accelerator -- stop
```

查看当前状态：

```bash
cargo run --bin linuxdo-accelerator -- status
```

直接打开 GUI：

```bash
cargo run --bin linuxdo-accelerator
```

## Configuration Paths

默认情况下，程序只使用一个主配置文件 `linuxdo-accelerator.toml`。

| 平台 | 主配置文件 | 运行状态目录 | 证书目录 |
| --- | --- | --- | --- |
| Linux | `~/.config/linuxdo-accelerator/linuxdo-accelerator.toml` | `~/.local/share/linuxdo-accelerator/runtime` | `~/.local/share/linuxdo-accelerator/certs` |
| Windows | `%APPDATA%\linuxdo\linuxdo-accelerator\config\linuxdo-accelerator.toml` | `%LOCALAPPDATA%\linuxdo\linuxdo-accelerator\data\runtime` | `%LOCALAPPDATA%\linuxdo\linuxdo-accelerator\data\certs` |
| macOS | `~/Library/Application Support/io.linuxdo.linuxdo-accelerator/linuxdo-accelerator.toml` | `~/Library/Application Support/io.linuxdo.linuxdo-accelerator/runtime` | `~/Library/Application Support/io.linuxdo.linuxdo-accelerator/certs` |
| Android | `/storage/emulated/0/Android/media/io.linuxdo.accelerator.android/linuxdo-accelerator.toml` | Android VPN 运行时内存态 | 非 Root 版无需证书 |

如果显式指定：

```bash
linuxdo-accelerator --config /path/to/linuxdo-accelerator.toml
```

程序会改用该配置文件；对应的 `runtime` 和 `certs` 目录也会优先跟着这个配置目录走。

## Config Example

```toml
listen_host = "127.211.73.84"
hosts_ip = "127.211.73.84"
http_port = 80
https_port = 443
upstream = "https://linux.do"
proxy_domains = ["linux.do", "www.linux.do"]
certificate_domains = ["linux.do", "www.linux.do", "*.linux.do"]
ca_common_name = "Linux.do Accelerator Root CA"
server_common_name = "linux.do"
```

当前项目把以下内容统一放在同一个配置文件中：

- DoH 上游
- 接管域名列表
- 证书 SAN 域名列表
- 监听地址和端口

默认监听地址和 hosts 回环地址使用 `127.211.73.84`，而不是 `127.0.0.1`，这样可以尽量减少与其他只占用 `127.0.0.1` 的本地代理/抓包/加速软件冲突。

## Binaries

项目当前只包含一个统一可执行文件：

- `linuxdo-accelerator`
  - 默认直接打开桌面 GUI
  - 传入命令参数后作为 CLI 使用
  - 负责 `setup / start / stop / status / gui` 等命令
  - Windows 下打包为可双击启动的 `.exe`
  - Linux 下由 `.desktop` 启动
  - macOS 下打包为 `.app / .dmg`

Android 版当前单独打包为 APK：

- `linuxdo-accelerator-android-arm64-v8a.apk`
- `linuxdo-accelerator-android-x86_64.apk`

## Packaging

项目使用 [`cargo-packager`](https://github.com/crabnebula-dev/cargo-packager)，打包配置直接写在 [`Cargo.toml`](./Cargo.toml) 的 `[package.metadata.packager]` 下：

- Windows：`NSIS .exe`，同时输出 `x64` 和 `arm64` 两个变体
- Linux：`.deb`，同时输出 `amd64` 和 `arm64` 两个变体
- macOS：`.dmg`，同时输出 `Apple Silicon (arm64)` 和 `Intel (x64)` 两个变体
- Android：`.apk`，同时输出 `arm64-v8a` 和 `x86_64` 两个变体

本地打包：

```bash
cargo install cargo-packager --locked
cargo packager --release
```

只打 Linux `deb`：

```bash
cargo packager -f deb --release
```

macOS 安装提示：

- 如果首次打开 `.app` 或安装 `.dmg` 后遇到系统拦截，需要去“设置 -> 隐私与安全性”里点击“允许”
- 允许后再重新打开应用即可
- 如果仍然打不开，可以在“隐私与安全性”页面里找到对应提示后再次确认放行

## GitHub Actions

macOS 不再走本地交叉编译脚本，而是通过 GitHub Actions 原生构建：

- Linux runner：生成 `.deb`
- Linux ARM runner：生成 `arm64 .deb`
- Windows runner：生成 `x64 NSIS .exe`
- Windows ARM runner：生成 `arm64 NSIS .exe`
- macOS ARM runner：生成 `arm64 .dmg`
- macOS Intel runner：生成 `x64 .dmg`
- Android runner：生成 `arm64-v8a .apk`
- Android runner：生成 `x86_64 .apk`

相关工作流见：

- [`.github/workflows/build-release.yml`](./.github/workflows/build-release.yml)

## Current Scope

当前定位仍然比较明确：

- 站点专属本地接管，不是系统全局代理
- 以 `HTTP / HTTPS` 为主
- 侧重 `linux.do` 及其关联域名

## Development Notes

本项目已经完成并验证过的关键点：

- Linux Wayland / GNOME 下的最小化和恢复
- Windows 托盘恢复、图标打包和无黑框提权
- macOS 本机编译与窗口最小化恢复链路
- 证书、`hosts` 和运行状态文件统一管理

## Inspirations

- [docmirror/dev-sidecar](https://github.com/docmirror/dev-sidecar)
- [`steamcommunity302`](https://www.dogfight360.com/blog/18682/)
