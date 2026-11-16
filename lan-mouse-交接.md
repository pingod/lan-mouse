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

旁路读 `/dev/input/event*`，**不拦截**本机按键。

macOS 走被动的 Quartz 事件 tap（`CGEventTapOptions::ListenOnly`），复用 capture
后端已有的辅助功能权限，同样不拦截本机按键；对端注入的事件带
`INJECTED_EVENT_TAG` 标记，会被跳过，避免两台机器来回互发。日志成功时打印
`hotkeys: listening on the macOS event tap`，没有权限时打印
`hotkeys: could not create the macOS event tap (Accessibility permission missing?)`。

Windows 目前仍是空实现（`noop`）。

### GUI

- **Hotkeys** 区块：列表、添加（弹窗输入 `Shift+Alt+Z` / `Z`）、**编辑**（铅笔按钮改组合键，`UpdateHotkey` 就地替换）、删除
- **Release shortcut** 行（Settings → Startup and maintenance）：编辑"把指针拉回来"的组合键，等价于配置里的 `release_bind`
- **Switch** 按钮：指针在对端时向对端要回（`send_leave_event`），不再报 "nothing to switch"
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
| 跨屏后指针位置偏移（左上出、右下进） | 接收端只有相对 motion，没人放光标 → `Enter` 带上"沿边分数"，接收端用 `motion_absolute`/`XWarpPointer`/`CGWarpMouseCursorPosition` 定位 |
| 首次跨屏仍然偏移 | 放置光标时对端还没发过输入，虚拟指针尚未创建 → `enter(handle, ..)` 先按需 `create` |
| macOS 媒体旋钮只影响本机 | 媒体键是 `NSEventTypeSystemDefined`，不在 tap 订阅里 → 订阅并在捕获时转发后 `Drop` |
| 小键盘不工作、NumLock 关不掉 | 捕获路径又发 key 又发显式 modifiers，锁键被 toggle 两次且 locked 被覆盖 → 只发键事件，状态交给合成器 keymap |
| 换显示器后 barrier 不重建（#512） | `zones_changed` 触发时整会话重建，`update_barriers` 随之执行 |
| macOS 上全局热键完全不生效 | `hotkey_task` 只有 Linux evdev 实现，其余平台是 `noop`（`next()` 永远 pending）→ 增加 Quartz 被动事件 tap |
| 热键组合键改不了 | GUI 只能增删 → 新增 `UpdateHotkey` / 行内编辑按钮 |
| `release_bind` 没有 UI | 新增 `UpdateReleaseBind` + Settings 里的 Release shortcut 行 |
| Switch 按钮在对端驱动本机时"点了没反应" | `switch_device()` 只在 `active_capture` 有值时生效，另一方向直接报错 → 记 `driving_peer`，向它发 Leave 要回指针 |
| 日志时间比系统时间慢 8 小时 | `timestamp()` 直接拿 `secs % 86400` 当时分秒，即永远输出 UTC → 改用 `localtime_r`（Windows 用 `SystemTimeToTzSpecificLocalTime`），并缓存每秒的渲染结果 |

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

### 本地编译（不走 CI，约 1 分钟一轮）

Mac 上其实装了 Homebrew（`/opt/homebrew/bin`，只是不在默认 PATH），有 `gtk4`、
`libadwaita`、`pkg-config`，所以可以直接在 Mac 上编译，不用等 CI 的 macOS 包：

```bash
scripts/mac-install.sh          # 同步源码 → Mac 上 release 编译 → 装进 .app → 重签 → 重启
```

它会 rsync 源码和 `.git`（后者让 shadow-rs 打上 `commit_hash`），编译完只替换
`/Applications/Lan Mouse.app/Contents/MacOS/lan-mouse`，然后用 `macos-sign.sh`
重签——证书签名让授权保持不变，不用重新勾辅助功能。

## 重要：macOS 授权

授权（辅助功能 + 输入监控）锚在**签名身份**上：只要每次更新都用
`macos-sign.sh` / `mac-deploy.sh` / `mac-install.sh` 重签，就永远不用重新勾。
之前“每次更新都要重新授权”的原因是 CI 的包是 ad-hoc 签名（按二进制哈希判定）。

万一手滑装了没重签的包导致掉权限，GUI 会自动弹窗，勾一次后按上面的流程更新即可；
也可以 `tccutil reset Accessibility de.feschber.LanMouse` 强制重置。

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

## GUI：Screens 页（可视化编排）

`lan-mouse-gtk/src/arrangement.rs` 是一个自绘的画布（`gtk::DrawingArea` + cairo），
按 Synergy 3 的屏幕编辑器做的：点阵背景、圆角屏幕卡片（显示器图标 + 主机名 + 边标签
药丸 + 右上角三点菜单）、相邻屏幕之间用粗连线表示"可达"，本机用强调色描边。

- 拖动卡片到本机的某条边就发 `FrontendRequest::UpdatePosition`（和 Devices 页下拉框
  同一条路径），配置立刻落盘；CLI `set-position` / 下拉框改动也会即时重画
  （`window.rs` 的 `refresh_arrangement` 在 new/delete/update_config 三处调用）。
- 卡片右上角的三点在**捕获阶段**被 `GestureClick` 认领，弹出 "Move to the left/right/
  top/bottom" 菜单；按在卡片其它位置则落到 `GestureDrag` 上，拖动时实时高亮将要吸附
  的那条边。
- 布局按卡片数量自适应：`State::fit` 计算整体包围盒，用 `scale`/`origin` 把它缩放居中
  画进画布，所以一条边上挂很多台设备也不会跑出面板。
- 颜色不查 style context（`gtk_style_context_lookup_color` 已废弃、`WidgetExt::color`
  需要 gtk4 的 `v4_10` feature，本仓库只开 `v4_2`），改用
  `adw::StyleManager::default().is_dark()` 选两套固定配色。
- 文字用 cairo toy API（`select_font_face` + `show_text`）绘制，不引入 pangocairo。

模型限制：一台设备只能占一条边；多条边共用的场景下按 `position_map` 的既有顺序
由"先在列表里出现的那台"接管，页面上就是把多个方块并排画在同一个方向上。

## GUI：Logs 页

- 级别菜单只有一个入口：`window.ui` 里原来的 `Level` 下拉框和 `Active level` 行已合并，
  下拉框的副标题显示服务端回报的 `Active: <level>`；`set_log_level` 用
  `combo.set_selected(position)` 把选中项同步过去（`refresh_log_level_row` 的注释与
  `set_selected` 的返回值都按"用户选择 ≠ 服务端确认"处理，避免回环）。
- 自动滚屏：`window/imp.rs` 的 `log_follow: Rc<Cell<bool>>` 由 `log_view` 的纵向
  adjustment `value-changed` 派生——停在底部就是跟随，往上拖就停止跟随，拖回底部自动
  恢复。`append_logs` 在插入前读一次 `follow`（插入后 `upper` 已经变大，再判断就会
  永远为假），`set_logs`（刷新/清空）把视图钉到底部并重新开启跟随。
- 载入历史记录时 `set_logs` 末尾补一个换行，否则流式追加的第一条会粘在最后一行后面。
- 滚到精确底部由 `scroll_logs_to_end` 负责：先 `scroll_to_iter`，再在 idle 里把
  adjustment 顶到 `upper - page_size`（`scroll_to_iter` 只保证可见，长行换行后最后一行
  可能被切掉半行）。
