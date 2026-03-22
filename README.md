# Linux.do Accelerator

<div align="center">
  <img src="./assets/icons/128x128.png" alt="Linux.do Accelerator" width="88" />
  <h1>Linux.do Accelerator</h1>
  <p>一个原生 Rust 的 <code>linux.do</code> 专属加速器，提供 <b>CLI + 桌面 GUI</b> 双形态。</p>

  <p>
    <img alt="Rust" src="https://img.shields.io/badge/Rust-Native-orange?style=flat-square" />
    <img alt="Platforms" src="https://img.shields.io/badge/Platform-Windows%20%7C%20Linux%20%7C%20macOS-1f6feb?style=flat-square" />
    <img alt="Package" src="https://img.shields.io/badge/Package-EXE%20%7C%20DEB%20%7C%20DMG-2da44e?style=flat-square" />
    <img alt="Mode" src="https://img.shields.io/badge/Mode-CLI%20%2B%20Desktop-6f42c1?style=flat-square" />
  </p>
</div>

<p align="center">
  <img src="./docs/images/gui-preview.png" alt="Linux.do Accelerator GUI Preview" width="480" />
</p>

## Overview

`linuxdo-accelerator` 的目标很直接：

- 一键生成并安装本地根证书
- 一键写入和清理 `hosts`
- 本地监听 `80/443`
- 为 `linux.do` 及其子域提供本地接管和转发
- 同时支持脚本场景下的 `CLI`，以及普通用户可双击使用的桌面 GUI

## Why This Exists

相信大多佬友都是用梯子访问论坛的，但看之前的[我的帖子](https://linux.do/t/topic/1763604/21)和这个[帖子](https://linux.do/t/topic/1761457/63)，`linux.do` 实际上是被 `SNI` 阻断的。运营商检测到 `linux.do` 的 `SNI` 后，会直接发出 `RST` 包，导致连接被重置。

类似 `steamcommunity302`、`Watt Toolkit`、`dev-sidecar` 这类项目，很多是通过 `SNI` 伪造来解决问题。但 `linux.do` 运行在 Cloudflare 上，而 Cloudflare 不支持这套 `SNI` 伪造方案，所以这条路走不通。

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
- 项目所有流量都在本地处理，没有任何第三方服务器转发
- 需要安装系统证书，并会占用本地 `80` 或 `443` 端口
- 理论上也支持其他被阻断、但支持 `ECH` 的网站，不过这类站点很少

如有 bug，欢迎反馈和 PR。

## Highlights

- 原生 Rust 实现，不依赖 Node 运行时
- GUI 与后台代理逻辑分离，窗口关闭或最小化后后台仍可继续工作
- 支持系统提权，适合证书安装、`hosts` 写入和低端口监听
- 配置项集中在单个 `linuxdo-accelerator.toml`
- 三端统一思路：
  - Windows：双击打开 `.exe`
  - Linux：安装 `.deb` 后桌面启动
  - macOS：拖入 `Applications` 后直接打开

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

如果显式指定：

```bash
linuxdo-accelerator --config /path/to/linuxdo-accelerator.toml
```

程序会改用该配置文件；对应的 `runtime` 和 `certs` 目录也会优先跟着这个配置目录走。

## Config Example

```toml
listen_host = "127.0.0.1"
hosts_ip = "127.0.0.1"
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

## Binaries

项目包含两个可执行文件：

- `linuxdo-accelerator`
  - CLI 主入口
  - 负责 `setup / start / stop / status` 等命令
- `linuxdo-accelerator-ui`
  - 桌面 GUI 入口
  - Windows 下双击打开弹窗
  - Linux 下可由 `.desktop` 启动
  - macOS 下打包为 `.app / .dmg`

## Packaging

项目使用 [`cargo-packager`](https://github.com/crabnebula-dev/cargo-packager) 和 [`Packager.toml`](./Packager.toml)：

- Windows：`NSIS .exe`
- Linux：`.deb`
- macOS：`.dmg`

本地打包：

```bash
cargo install cargo-packager --locked
cargo packager --release -c Packager.toml
```

只打 Linux `deb`：

```bash
cargo packager -f deb --release -c Packager.toml
```

macOS 安装提示：

- 如果首次打开 `.app` 或安装 `.dmg` 后遇到系统拦截，需要去“设置 -> 隐私与安全性”里点击“允许”
- 允许后再重新打开应用即可
- 如果仍然打不开，可以在“隐私与安全性”页面里找到对应提示后再次确认放行

## GitHub Actions

macOS 不再走本地交叉编译脚本，而是通过 GitHub Actions 原生构建：

- Linux runner：生成 `.deb`
- Windows runner：生成 `NSIS .exe`
- macOS runner：生成 `.dmg`

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
- `steamcommunity302`
