# lan-mouse 交接说明

## 功能（全部已实现并验证）

### 全局热键（指针留在本机时也能触发）

```toml
# ~/.config/lan-mouse/config.toml
release_bind = ["KeyLeftCtrl", "KeyLeftShift", "KeyLeftAlt", "KeyP"]

[[hotkey]]
from = ["KeyLeftShift", "KeyLeftAlt", "KeyS"]
to = ["KeyS"]

[[hotkey]]
from = ["KeyLeftShift", "KeyLeftAlt", "KeyZ"]
to = ["KeyZ"]
```

旁路读 `/dev/input/event*`，**不拦截**本机按键。GUI 可增删。

### GUI

- **Hotkeys** 区块：列表、添加（弹窗输入 `Shift+Alt+Z` / `Z`）、删除
- **version**：`0.11.0 (commit)` 显示在 General 底部
- **start with session**：开关，装/卸自启项

### 开机自启

| 平台 | 文件 | 说明 |
|---|---|---|
| Linux | `~/.config/systemd/user/lan-mouse.service` | 跑 `daemon` |
| macOS | `~/Library/LaunchAgents/de.feschber.LanMouse.plist` | 跑 **GUI**，因为只有 GUI 会请求辅助功能权限 |

## 修掉的 bug

| 问题 | 根因 |
|---|---|
| 开机热键误触发 | evdev 缓冲区积压打开前的事件 → 打开时排空 |
| 热键完全失效 | `try_io` 的 `WouldBlock` 被当致命错误 |
| Mac 收到两份按键 | 热键与捕获路径同时发送 → 捕获时静默 |
| 热键永久静默 | `ClientEntered` 重复导致计数失衡 → 改用共享标志 |
| 锁屏/换显示器后卡死（#545/#512） | 合成器夺走指针后不再有输入 → `CaptureEvent::Interrupted` |
| Leave 后仍收按键（#542） | 迟到的 Input 重建 handle → Leave 后锁定 |
| Mac 上媒体键（音量旋钮、静音）无反应 | 媒体键必须发 `NSEventTypeSystemDefined` 事件，普通 CGEvent 键盘事件被窗口服务器忽略 |
| 并发 `add-client` 出现 `unknown:4242` | `Create` 后的 `Update*` 应答会广播给所有前端，两个前端互相改错 handle → 改为 `CreateClient` 原子创建 |
| 删掉最后一个客户端又自己复活 | 空列表时提前返回，配置没落盘，watch 回灌旧列表 |
| 换显示器后 barrier 不重建（#512） | `zones_changed` 触发时整会话重建，`update_barriers` 随之执行 |

## 上游 issue 已修

#529 #388 #365 #486 #408 #519 #403 #497 #542 #545 #512

## 当前状态（2026-11-15）

- 分支 `main`，已推送并出 pre-release：`main-55d958e`（macOS arm64 / Linux / Windows）
- 本轮审计：36 个文件、约 +2500/−870，含协议截断 panic、前端 IPC 监听器崩溃、
  capture 句柄失配 panic、emulation 各类 unwrap、remap 和弦状态机等
- 验证：`cargo test --workspace` 全绿，`cargo clippy` 仅剩 3 条无关的 manifest 提示，
  macOS 侧的 capture/emulation 在真机 `cargo check` 通过

## 仓库

https://github.com/pingod/lan-mouse

## 重要：macOS 更新流程（别再手工 ditto）

CI 出的包是 **ad-hoc 签名**，而 ad-hoc 下 macOS 把 TCC 授权绑在二进制的
cdhash 上，换二进制就掉权限（表现为 `accessibility permission is required`
→ `using emulation backend: dummy`，整个 Mac 侧注入全部失效，不只是媒体键）。

正确做法：**每次更新都走 `scripts/mac-deploy.sh`**，它安装后会用本地自签证书
`Lan Mouse Local Signing`（SHA-1 `8A3127DAB5EC34226D2EF12788DEA98DFE6338E4`，
keychain `~/Library/Keychains/signing.keychain-db`）重签：

```bash
~/githome/lan-mouse/scripts/mac-deploy.sh ~/Downloads/lan-mouse-macos-arm64.zip
```

重签后 designated requirement 变成
`identifier "de.feschber.LanMouse" and certificate leaf = H"8a3127…"`，
锚在证书上而不是二进制，所以授权不会再掉（实测：重装 + 重签 + 重启后
`Accessibility state (initial = true)`，无需再勾）。

只重签已有 app：`scripts/macos-sign.sh "/Applications/Lan Mouse.app"`。
签名的 keychain 口令是 `lmsign`，证书丢了就得重做一次身份（脚本头部有步骤）。

## 重要：macOS 授权

替换 Mac 二进制后**必须重新授权**（辅助功能 + 输入监控）：
```bash
tccutil reset Accessibility de.feschber.LanMouse
```
因为 ad-hoc 签名下 macOS 按二进制哈希判定。GUI 会自动弹窗。

## 配置陷阱

`release_bind` 必须写在任何 `[table]` **之前**。

## 未完成

**#512 KDE 换显示器后卡死** —— 已修（`55d958e`）

`zones_changed` 流触发时整会话重建，`update_barriers` 随之执行，barrier 跟着
新的显示器几何走。保留 `f91914c` 的兜底：capture 流结束时先释放并给对端发
Leave，所以即使重建失败也不会永久卡死。

**#545 锁屏** 已修两处：
- `CaptureEvent::Interrupted`（合成器夺走指针）
- capture 流结束时先释放再重启

锁屏时日志会出现：`releasing capture: input stream interrupted by the compositor`
或 `capture stream ended — releasing before restarting it`
