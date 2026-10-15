<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/brand/kelpie-lockup-horizontal-dark.svg" />
    <img src="assets/brand/kelpie-lockup-horizontal-light.svg" alt="Kelpie" width="400" />
  </picture>
</p>

<p align="center"><b>Many agents. One calm shepherd's dog.</b></p>

<p align="center">
  <a href="#安装">安装</a> · <a href="#功能">功能</a> · <a href="#开发">开发</a> · <a href="#致谢">致谢</a> · <a href="README.md">English</a> · 简体中文
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-d9a066?labelColor=1a1a24" alt="Apache 2.0 license" /></a>
  <img src="https://img.shields.io/badge/Windows%2011-first--class-d9a066?labelColor=1a1a24" alt="First-class on Windows 11" />
  <img src="https://img.shields.io/badge/built%20on-herdr-d9a066?labelColor=1a1a24" alt="Built on herdr" />
</p>

```
▗▖ ▗▖
▐▙▄▟▌     Kelpie
███▘███▄  agent workspace
███████▘  built on herdr
▐██▌
```

---

Kelpie 是面向 AI 编程智能体(如 Claude Code、Codex、Cursor)的终端复用器和运行时。你可以并排运行十个或更多会话,每个会话保留各自原生的终端界面,背后是由服务器托管的运行时和本地 JSON API。监督智能体可以观察每个会话并推动它继续前进。最终决定权始终在人手里。在 Windows 11 和 PowerShell 7 上是一等公民,在 Linux 和 macOS 上同样好用。

## 为什么有 Kelpie

herdr 是少数真正打动过我的工具之一。第一次用它时,我喜欢到落泪。但用得越久,那些细小的毛糙之处就越让我在意,而 herdr 只接受获批贡献者的 PR,我没法把修复提交回去。所以我 fork 了它。Kelpie 就是打磨掉这些毛边、再加上一个监督者的 herdr。

## 功能

- **断开连接,工作不停。** 关闭客户端或 SSH 断线后,终端仍在后台服务器中运行。重启后,Kelpie 会恢复保存的布局,并可恢复受支持的智能体会话。
- **所有智能体一目了然。** 每个窗格都标记为工作中、等待中、空闲或完成。侧边栏显示空闲时长,标出过期的工作区,并用 `!!` 标出正在等你的那一个。
- **先确认、再输入的监督者。** `kelpie agent watch` 输出所有智能体的有界视图。`kelpie agent submit`、`answer`、`interrupt`、`reset` 和 `resume` 只会写入干净的输入框,会避开你自己的按键,并留下可用 `kelpie agent submit-status` 查询的回执。
- **智能体配置档。** 内置 Claude Code、Codex 和 Cursor:输入框形状、占位符样式、提交/中断/清除按键。Claude Code 还会通过其提示钩子确认送达。
- **运行你已经在用的工具。** Claude Code、Codex、Cursor、OpenCode、Grok 等。Kelpie 不包装也不替换它们,只管理它们的终端。
- **键盘和鼠标都是一等公民。** tmux 风格的前缀键(`ctrl+b`),也支持点击、拖拽、分屏。
- **适合长时间使用的配色。** `midnight`、`dusk`、`ember`、`slate`、`forest` 和 `kelpie`,按 `ctrl+b t` 实时预览选择。
- **主机信息。** `ctrl+b i` 显示机器、shell、GPU、Docker、WSL 以及智能体可用的命令行工具。
- **单个 Rust 二进制文件,不用 Electron。** 在你现有的任何终端里运行。

## 安装

Kelpie 还没有发布渠道,请从源码构建。在有发布渠道之前,自动更新是关闭的。

需要:Rust(版本见 `rust-toolchain.toml`)和 Zig 0.16.0。

```powershell
git clone <kelpie repository>
cd kelpie
cargo build --release
.\target\release\kelpie.exe
```

在 Linux 和 macOS 上,二进制文件是 `./target/release/kelpie`。

Kelpie 使用自己的配置、状态和套接字,可以与已安装的 herdr 并存,互不影响。

运行智能体、分屏,然后离开。`ctrl+b q` 断开,`kelpie` 重新连接。

## 开发

```bash
just test        # 单元测试
just check       # 格式、测试和维护检查
```

如果你是在本仓库中工作的 AI 智能体,修改前请阅读 [`AGENTS.md`](./AGENTS.md),提交 issue 或 PR 前请阅读 [`CONTRIBUTING.md`](./CONTRIBUTING.md)。

## 致谢

Kelpie 是基于 [herdr](https://github.com/herdrdev/herdr) 的社区分支,herdr 由 Ogulcan Celik 及其贡献者开发。运行时、终端引擎、智能体检测以及 Kelpie 的大部分基础都来自 herdr。谢谢。

herdr 过去的赞助者和支持者列在 [SPONSORS.md](./SPONSORS.md) 中。

## 许可证

Kelpie 采用 [Apache License 2.0](LICENSE) 许可,与 herdr 相同。
