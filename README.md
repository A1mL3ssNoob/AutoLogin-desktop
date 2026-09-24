# 校园网自动登录

一个面向 Windows 10/11 x64 的校园网自动登录客户端。应用常驻系统托盘，定期检查网络状态；检测到配置的校园认证门户后，才会提交已保存的账号信息并验证网络是否恢复。

它适用于使用兼容 ePortal / 校园认证流程的网络环境。认证网关地址、探测地址和门户主机均可配置，默认值针对本项目的目标校园网络。

> 当前版本：`0.1.5` · Windows 专用 · Tauri 2 + Rust

## 功能

- 后台监测网络连接，正常时低频探测，异常时快速确认。
- 仅在识别到配置的校园认证门户后提交凭据，普通断网不会发送账号信息。
- 支持中国电信、中国联通和中国移动三种运营商参数。
- 支持手动填写，或在认证窗口中自动获取公寓 / 楼栋 ID 和房间 ID。
- 系统托盘操作：立即登录、暂停或恢复自动登录、打开主窗口。
- 可选开机启动，主窗口关闭后仍可在托盘继续监测。
- 内置脱敏诊断日志，支持按网络、认证和错误类型查看并导出。

## 下载与运行

项目目前提供源码构建方式。发布安装包后，可在 GitHub Releases 下载对应的 Windows 安装程序。

运行环境：

- Windows 10 或 Windows 11，x64
- [Microsoft WebView2 Runtime](https://developer.microsoft.com/microsoft-edge/webview2/)

安装后首次打开应用，按下面的步骤完成配置即可。

## 首次配置

1. 填写校园网使用的手机号 / 身份证后8位。 （就是你登录每次新设备联网登录时的手机号和识别码）
2. 选择账号对应的运营商。
3. 填写认证系统内部的公寓 / 楼栋 ID 和房间 ID。

   - 不知道时，点击“自动获取认证信息”，在认证窗口中完成一次正常登录，应用会从认证请求中读取这两个字段。
4. 点击“保存配置”，然后等待后台监测，或点击“立即登录”测试配置。

公寓 ID 和房间 ID 是认证系统使用的内部标识，不一定等于页面上显示的楼栋名或房间号。自动获取功能默认访问 `http://baidu.com` 来触发校园认证跳转。

认证窗口使用独立的无痕 WebView2 会话。窗口关闭后不会保留上次会话的 Cookie 和网页存储；应用主动保存的凭据仍可用于后台自动登录。

## 数据安全与隐私

- 手机号 / 账号和 UID / 认证码使用 Windows DPAPI 加密，保存在本机。
- 普通配置文件只保存网络参数、运营商和公寓 / 房间内部 ID，不保存明文凭据。
- 日志采用脱敏 JSONL 格式，不记录密码、UID、Token、Cookie、授权码或完整认证 URL。
- 日志按大小和时间自动轮换：单文件不超过 5 MB，保留 7 天，总量不超过 35 MB。
- “自动获取认证信息”只读取认证请求中的必要字段，不保存认证窗口的 Cookie 或完整请求。

请只在你有权使用的校园网络中运行本项目，并遵守学校网络和账号使用规定。

## 从源码构建

开发环境需要：

- Windows 10/11 x64
- [Node.js 24](https://nodejs.org/)
- [Rust](https://www.rust-lang.org/tools/install)（stable toolchain）
- WebView2 Runtime

在仓库根目录执行：

```powershell
npm install
npm run tauri dev
```

构建 Windows 安装包：

```powershell
npm run tauri build
```

安装包目标为 NSIS，构建产物位于 `src-tauri/target/release/bundle/`。

## 测试与回归检查

运行 Rust 单元测试和桌面端回归脚本：

```powershell
cargo test --manifest-path src-tauri/Cargo.toml --lib
cargo build --manifest-path src-tauri/Cargo.toml --bin campus_auto_login
node tests/capture-lifecycle.mjs --scenario all
node tests/capture-private-session.mjs
```

回归脚本会使用隔离的临时配置和本地 HTTP 服务，检查认证窗口的创建、关闭、取消、重新打开，以及无痕会话的 Cookie / 网页存储行为。测试只清理脚本自己启动的进程，不读取已保存的账号信息。

## 项目结构

```text
.
├── ui/                 # 主窗口前端界面
├── src-tauri/src/      # Tauri 命令、网络探测、认证和运行时
├── src-tauri/icons/    # 应用和托盘图标
├── tests/              # Windows 窗口与会话回归脚本
├── package.json        # 前端依赖和 Tauri CLI 命令
└── src-tauri/Cargo.toml
```

桌面端不包含 OpenWrt 脚本，也不会修改或调用外部的 `auto_login.py`。

## 常见问题

### 没有自动登录

请先确认：

1. 账号、UID / 认证码和运营商配置正确。
2. 公寓 ID、房间 ID 是认证系统内部 ID，而不是可读名称。
3. 当前网络确实会跳转到配置的校园认证门户。
4. 没有在应用中暂停监测。

可以在“运行日志”中查看探测、门户识别、认证和验证结果。

### 无法打开认证窗口

确认 WebView2 Runtime 已安装，然后重新启动应用。窗口生命周期相关问题也可以通过上面的回归脚本复现和定位。

### 如何暂停后台登录

在主窗口点击“暂停监测”，或从系统托盘菜单选择“暂停自动登录”。恢复后应用会继续探测网络。

## 许可证

本仓库当前未附带许可证文件。公开发布前，请在仓库根目录添加与你的发布意图匹配的 `LICENSE` 文件，并在此处补充许可证名称和版权信息。

## 贡献

欢迎提交 Issue 和 Pull Request。提交问题时，请附上 Windows 版本、应用版本、复现步骤和脱敏后的诊断日志；不要上传账号、UID、Cookie 或完整认证 URL。

