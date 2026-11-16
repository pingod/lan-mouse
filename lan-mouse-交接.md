# lan-mouse 交接说明

## 功能（全部已实现并验证）

### 屏幕边缘区间（一条边挂多台机器）

```toml
[[clients]]
position = "right"
hostname = "cobalt"
ips = ["192.168.178.190"]
edge = [0.0, 0.6]        # 本机右边上，指针离开的区间
peer_edge = [0.0, 1.0]   # cobalt 左边上，指针落下的区间
```

`edge` / `peer_edge` 都可省略（默认整条边）。没被任何客户端覆盖的区间是**墙**，指针停在
那里，而不是交给列表里第一个；两台声明重叠时靠前的保留，靠后的被收窄。

`lan-mouse cli screens` 不依赖显示器就能看覆盖情况与墙的位置，是这条链路唯一的自检手段；
`cli set-edge <id> --edge 0.0,0.6` 改区间，`cli list` 在非整条边时把区间打出来。

配套的两个阈值也生效了：`switch_double_tap`（毫秒内连续两次穿越才切换）、
`switch_corner_size` + `switch_corners`（角落屏蔽，画布会把被吃掉的边画成墙）。

细节见下面"Screens 页"一节；守护进程侧按落点归属的说明在 `DOC.md` 的
"Screen Edges and Intervals"。

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
- **Switch** 按钮（三种情况都覆盖）
  - 本机正在把指针送给对端 → 收回（`capture.release()` + 预热下一个客户端）
  - 对端正在驱动本机 → 让对端放手（`send_leave_event`）
  - 两边都没在用指针（界面显示 `Controlling: this machine`）→ **本机主动接管**：
    `CaptureRequest::Begin(handle)` → `InputCapture::begin()` → 平台后端的
    `Capture::begin(pos)`。macOS 通过 `ProducerEvent::Take(pos)` 隐藏并钉住物理光标，
    然后照常发 `CaptureEvent::Begin(0.5)`；同时置 `center_cursor` 标记，让发送的第一个
    `ProtoEvent::Enter(opposite, 0.5)` 后面追加 `ProtoEvent::CenterCursor`，对端收到后把
    自己的光标**移到屏幕正中**（`Emulation::enter_center`：wlroots 取输出并集中心、
    X11 取 root 窗口中心、macOS 取主屏中心）。
  - 局限：合成器驱动的后端（layer-shell / libei / windows / x11）无法被要求接管指针，
    `begin` 返回 `CaptureError::BeginUnsupported`，此时 GUI 会弹出错误提示（"cannot hand
    the pointer to client N: this capture backend cannot take the pointer on request"）。
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

这一页在第二轮被重写，起因是上一版只画得像 Synergy 3，而守护进程根本撑不住它画的东西。
先把结论写在前面：**问题不在画布，在数据模型**。当时 `client_at()` 只返回同边的第一个
客户端，`activate_client()` 还会主动把同边的另一台停用，所以"一条边挂多台机器"不仅没实现，
还被主动阻止了；画布把多台都画成可达，等于画了个谎。

### 数据模型（先把地基铺好）

`input-event/src/edge.rs` 的 `EdgeRange` 是半开区间，**以 permille（0..=1000）存**而不是
`f64`——`ClientConfig` 派生了 `Eq`，塞 `f64` 会破坏它；配置文件里仍然是 `edge = [0.0, 0.6]`
这样的分数，两边换算精确。

- `ClientConfig::edge`：本机这条边上指针**离开**的区间
- `ClientConfig::peer_edge`：对端朝向这条边的边上，指针**落下**的区间

两者独立：`0.0-0.6` 映射到 `0.0-1.0` 是把指针摊到对端整条边，映射到 `0.3-0.9` 则收在中间。
协议没有改动：`ProtoEvent::Enter(Position, f64)` 本来就带归一化分数，发送端用
`CaptureEntry::peer_fraction` 把自己的分数重映射到对端区间即可（与 Synergy 的
`mapToFraction → transform → mapToPixel` 三步同构）。

归属按**落点**判定而不是按边（`src/capture.rs` 的 `route`）：落在谁的区间归谁，落在缝里
谁都不给并记日志。后端实现差异：libei / layer-shell 造的是**物理屏障**（指针真的撞墙），
macOS / Windows 在落点处判断（越界表现为"切过去又被弹回"）。

### 画布

`lan-mouse-gtk/src/arrangement/` 拆成四个文件：

- `layout.rs`：纯几何。本地屏幕按上报尺寸、对端按其分辨率（没上报则取"使两段区间对齐"
  的尺寸）。`place_all` 把沿边重叠的卡片分道——对端按自己分辨率画时**装不进**它配置的
  区间是常态（`our 0-55% -> its 0-100%` 两台同尺寸屏，卡片就是它挂的那段的两倍长），
  不分道就会互相埋掉。沿边的位置是路由的一部分，绝不动；离边的距离不是，所以分道只改
  后者。
- `draw.rs`：桥与墙同出 `reachable_stretches`——把每个客户端的声明减去被角落阈值吃掉的
  两端，剩下的画桥，补集画墙，两者不可能自相矛盾。角落把声明吃光时干脆不画桥，这才是
  "没有任何东西能到这台机器"的诚实画法。
- `paint.rs`：配色与文字。文字走 pango（`pangocairo`），因为 cairo toy API 没有 fallback，
  中文主机名会变成方框；装不下时按字符数二分加省略号，不用 `PANGO_ELLIPSIZE_END`
  （那需要 pango 1.56 才暴露的 `WRAP_NONE`，会把整个程序的运行时要求抬高）。
- `arrangement.rs`：widget、手势、工具栏。

交互：拖动卡片 → `layout::snap`（摆放的逆运算）由两面边的重叠重新算出两段区间；本机边上
的把手单独调整一段声明的一端，这是精确设区间的方式。Ctrl+滚轮缩放、中键平移、方向键换边、
`+`/`-`/`0` 缩放复位。

### 这一轮修掉的两个真 bug

- **画布每次状态广播都重建**：守护进程在 peer 连接、激活、回 ping 时都会广播，等于把用户
  正按着的控件删掉——拖动中途死掉，缩放复位。现在只建一次，之后只喂数据；拖动/调整期间
  到达的更新直接丢弃。
- **卡片圆角弧线多扫了四分之一圈**：每块屏幕都被切出一个缺口。`paint.rs` 里有一条按像素
  检查四个角的测试，用旧的弧线角度会失败。

### 验证

`cargo test --workspace` 全绿（`edge.rs` 20 条、`capture.rs` 12 条、GTK 57 条），clippy 无
新告警。GTK 侧有**离屏渲染测试**：把画布画到 cairo surface 上按像素断言——本地屏比笔记本
高、同边两台上下叠放不重叠、无人可达处确实画了墙、停用的机器不画。设
`LAN_MOUSE_RENDER_DEMO=<path>` 会导出 PNG，供人眼复核（配色、圆角、分道这类东西没法靠
断言看）。

dummy 后端双客户端实测（`lan-mouse cli screens`）：

```
right:
  id 0: alpha 20-60%  [ok]
             lands in its 10-90%
  id 1: beta 60-100%  [narrowed to what is left of 50-100%]
  0-20% of the right edge is a wall
```

### 明确的非目标

按显示器拆几何（一台机器仍是一个矩形，与 Synergy 1 / Input Leap 相同）、一台机器挂两条边
（指针不能绕角）、非对称链路、`switchNeedsShift/Ctrl/Alt`（Wayland 读不到不归自己的指针
修饰键）、`switchDelay`（屏障报的是穿越不是停留，没有可等的东西；实现了
`switchDoubleTap`，因为两次穿越可观测）、`switch_corners` 的 GUI 编辑（配置键与守护进程
逻辑先行）。

行为变化：同边多台客户端各自维持 DTLS 会话与心跳，连接数与开销都比以前多。

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

## 协议与后端：绝对定位 / 滚动停止 / 平台热键（2026-11-16）

- **libei 模拟后端的绝对定位**：绑定 `ei_pointer_absolute`，用 `ei_device.dimensions`
  报出的逻辑尺寸把 `ScreenEdge` + 比例换算成绝对坐标后调 `motion_absolute`，因此
  Hyprland 上的跨屏落点/切换落点是精确的（wlroots 的虚拟指针会被指针加速曲线影响，
  只能近似；portal 后端拿不到 stream 几何，保持空实现）。
- **惯性滚动停止**：``ProtoEvent::PointerAxisStop`` / `PointerAxisCancel` 新增，
  EIS 的 `scroll_stop` / `scroll_cancel` 现在能传到接收端，wlroots 后端转发为
  `wl_pointer.axis_stop`，libei 后端转发为 `ei_scroll.scroll_stop`。
- **Windows 全局热键**：`WH_KEYBOARD_LL` 被动钩子（始终 `CallNextHookEx`），
  扫描码经 `scancode::Windows → Linux` 后走与 Linux/macOS 相同的匹配逻辑；
  `LLKHF_INJECTED` 的事件会被跳过，避免两台机器互发热键。
- **日志时间戳**改为本地时区（Unix `localtime_r`，Windows
  `FileTimeToSystemTime` + `SystemTimeToTzSpecificLocalTime`），并缓存每秒的渲染结果。
- **前端广播**改为并发写（`FuturesUnordered`），单个卡住的 GUI 不再拖慢其它连接。
- 触摸屏仍不支持（`#17`），X11 原生捕获仍是桩（`#18`）——两者都在 issue 里写了
  实现方案；wlroots 的虚拟指针协议本身没有触摸注入，所以触摸只在 libei/portal
  后端上有意义。

## GUI：Key remapping 与 enter/leave hook 编辑（2026-11-16）

- 协议：`FrontendRequest::{UpdateHooks, EnumerateRemap, AddRemap, UpdateRemap, RemoveRemap}`、
  `FrontendEvent::Remap`。`UpdateHooks` 一次性替换两条命令——分两次发 `UpdateEnterHook` /
  `UpdateLeaveHook` 会让守护进程广播中间态，GUI 可能短暂显示"只改了一半"的状态。
- 服务端：`update_hooks` / `add_remap` / `update_remap` / `remove_remap` / `apply_remap`
  （写 config → 推 capture task → 刷新前端）；非法键名走 `remap_error`：报错并重播未变的
  快照。`sync_frontend` 现在也会补发 remap 快照，否则晚连上的前端只能看到空列表。
- 配置：`Config::set_remap_rules` 在列表清空时把 `remap` 置 `None`（写 `remap = []` 会让
  默认规则在下次重载时复活）。
- GUI：客户端行新增 hooks 行与 "Edit hooks" 对话框（`sh -c` 提示、留空即清除）；
  Settings 页新增 "Key remapping" 组（加/改/删）；热键与 remap 共用同一个录音对话框
  （`open_chord_dialog` + `ChordTarget::{Hotkey, Remap}`），避免两份重复的录音逻辑。
