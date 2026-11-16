# lan-mouse 使用与维护文档

当前版本 **0.12.0**。

本仓库是 [feschber/lan-mouse](https://github.com/feschber/lan-mouse) 的维护分支，
在 0.11.0 基础上修复了一批输入捕获、指针回程、macOS 权限与前端可用性问题，
并补充了服务控制、连接状态、日志查看等功能。

上游 README 讲的是安装与设计目标；本文档讲**这一版实际怎么用、为什么这么改、
以及改动时容易踩的坑**。

---

## 1. 快速开始

### 1.1 编译

```bash
# Linux（需要 gtk4 / libadwaita / libei / wayland）
cargo build --release --all-features

# 只编后端、不要 GUI
cargo build --release --no-default-features --features libei_capture,wlroots_emulation
```

macOS 需要在 macOS 上构建（Apple SDK 无法交叉编译）：

```bash
export PATH=/opt/homebrew/bin:$PATH
export PKG_CONFIG_PATH=/opt/homebrew/lib/pkgconfig
cargo build --release --all-features
```

### 1.2 开机自启

GUI 的 **Settings → Start with session** 开关会写系统服务定义：

| 平台 | 位置 |
|---|---|
| Linux | `~/.config/systemd/user/lan-mouse.service` |
| macOS | `~/Library/LaunchAgents/de.feschber.LanMouse.plist` |

手动管理：

```bash
systemctl --user start lan-mouse.service
systemctl --user status lan-mouse.service
launchctl load  ~/Library/LaunchAgents/de.feschber.LanMouse.plist
launchctl unload ~/Library/LaunchAgents/de.feschber.LanMouse.plist
```

---

## 2. 配置

默认路径 `~/.config/lan-mouse/config.toml`，改动**自动热重载**（文件监听）。

```toml
# 释放组合键，必须写在任何 [table] 之前
release_bind = ["KeyLeftCtrl", "KeyLeftShift", "KeyLeftAlt", "KeyP"]

# 定时重启（秒）。留空或删除表示不启用。
restart_interval = 86400

[[clients]]
hostname = "192.168.1.5"
position = "top"          # left / right / top / bottom
activate_on_startup = true

# 全局热键：本地按键不被拦截，同时在对端触发
[[hotkey]]
from = ["KeyLeftShift", "KeyLeftAlt", "KeyS"]
to   = ["KeyS"]

# 本地动作热键（不发按键，执行动作）
[[hotkey]]
from   = ["KeyRightAlt"]
action = "switch_device"  # 或 "force_release"
taps   = 2                # 双击右 Alt

# 输入重映射
[[remap]]
from = ["KeyLeftCtrl", "KeyC"]
to   = ["KeyLeftCtrl", "KeyInsert"]

[authorized_fingerprints]
"ab:cd:..." = "描述"
```

### 配置项说明

| 键 | 说明 |
|---|---|
| `release_bind` | 强制把指针收回本机的组合键 |
| `restart_interval` | 服务定期重启（秒），用于长时间运行后重建后端 |
| `capture_backend` / `emulation_backend` | 覆盖后端选择 |
| `port` | 监听端口，默认 4242 |
| `cert_path` | 证书路径 |
| `switch_double_tap` | 边缘需要在这个毫秒数内被连续穿越两次才切换，`0` 关闭 |
| `switch_corner_size` | 被屏蔽角落的像素半径，`0` 关闭 |
| `switch_corners` | 被屏蔽的角落，取值 `top-left` / `top-right` / `bottom-left` / `bottom-right` |

`[[clients]]` 里除了 `position`、`hostname`、`ips`、`port`、`activate_on_startup`，还有两个区间：

| 键 | 说明 |
|---|---|
| `edge` | 本机这条边上指针离开的区间，`[start, end]` 分数，默认 `[0.0, 1.0]` |
| `peer_edge` | 对端朝向这条边的边上，指针落下的区间，默认 `[0.0, 1.0]` |

一条边可以挂多台机器，各自占一段：没被任何客户端覆盖的区间就是墙，指针停在那里而不是交给"列表里第一个"；两台声明重叠时，列表里靠前的保留它要的区间，靠后的被收窄到剩余部分。

---

## 3. 命令行

```bash
lan-mouse cli <命令>
```

### 客户端管理

| 命令 | 作用 |
|---|---|
| `list` | 列出客户端及连接状态 |
| `add-client --hostname H --port P --ips IP` | 添加客户端 |
| `remove-client <id>` | 删除客户端 |
| `activate <id>` / `deactivate <id>` | 启用/停用 |
| `set-host` / `set-port` / `set-position` / `set-ips` | 修改配置 |
| `add-client … --position P --edge S,E --peer-edge S,E` | 添加时直接指定边与区间 |
| `set-edge <id> --edge S,E [--peer-edge S,E]` | 改区间而不改边 |
| `screens` | 打印本机可达区域尺寸、每条边由谁覆盖、哪里是墙、谁被收窄了 |

`screens` 打印的尺寸来自捕获后端。它**不是**启动时就有的：portal/libei 后端的
`create` 只是把请求排进队列，真正能回答 `zones` 的会话是在捕获任务里之后才建立的，
所以第一次读必然是空的。捕获任务每秒重读一次，拿到就发布；读空不会覆盖已知值
（"还没准备好"和"没有屏幕"在任务内部无法区分，抹掉一个能用的布局是更糟的错误）。
插拔显示器不需要重启——尺寸变了会被下一次轮询发现并推给对端。

`screens` 是这条链路唯一不依赖显示器的自检手段：

```
$ lan-mouse cli screens
this screen: 1920x1080

right:
  id 0: alpha 20-60%  [ok]
             lands in its 10-90%
  id 1: beta 60-100%  [narrowed to what is left of 50-100%]
  0-20% of the right edge is a wall
```

### 服务控制

| 命令 | 作用 |
|---|---|
| `pause` / `resume` | 暂停/恢复捕获与模拟（进程不退，IPC 仍可用） |
| `restart` | 退出以便服务管理器拉起新进程 |
| `set-restart-interval <秒>` | 设置定时重启，`0` 关闭 |
| `reload-config` | 立即重读配置文件 |
| `force-release` | 立刻把指针收回本机 |
| `switch` | 从当前控制本机的对端手中收回控制权 |
| `refresh-peer-info <id>` | 重新拉取对端机器信息 |

### 观测

| 命令 | 作用 |
|---|---|
| `status` | 服务状态、定时重启、每个客户端的完整信息 |
| `logs [N]` | 最近 N 条日志（默认 100） |
| `set-log-level <级别>` | off / error / warn / info / debug / trace |
| `save-config` | 把当前状态写回配置文件 |

`status` 输出示例：

```
service: running
scheduled restart: off
id 0: 192.168.1.5:4242 (top)
  enabled: true, connected: true, alive: true, resolving: false
  address: 192.168.1.5:4242
  round trip: 10 ms
  os: macos
  os version: 27.3
  kernel: 27.3.0
  arch: aarch64
  hostname: Pavia-MacBookPro-1859.local
  version: 0.12.0
  commit: 38eb1dc1
```

---

## 4. 前端

窗口分为四页，顶部 `AdwViewSwitcher` 切换：

### Status

- **Service**：Pause 开关、Release input、Controlling（当前谁持有指针）、Switch device
- **Activity**：Peers（出站/入站数）、Forwarded（收发事件数）、Latency、Uptime、Listen port
- **Backends**：实际使用的后端名与运行状态，异常时显示 Reenable

Activity 由 daemon 每秒推送一次快照驱动；Backends 只在该后端真的不可用时才显示警告。

### Devices

- **Outgoing connections**：可控制的机器。展开后显示对端的系统、内核、架构、
  主机名、版本、commit、显示器。
- **Inbound**：正在控制本机的设备。展开后显示同样的字段，加上地址、进入方向、
  收到事件数、空闲时间、证书指纹。
- **Incoming connections**：已授权的设备指纹。

### Settings

本机名与端口、证书指纹、开机自启、定时重启、重启/重载、热键列表、版本。

热键对话框有 **Record** 按钮：点一下，按下组合键即可捕获，无需手打键名。

### Logs

实时日志流（250 ms 一批），可切换级别、清空、刷新。
日志同时写入 stderr，`journalctl --user -u lan-mouse.service` 也能看到。

---

## 5. 架构要点

```
┌────────────┐   IPC (unix socket)   ┌────────────┐
│  GTK 前端  │ ◄───────────────────► │   daemon   │
└────────────┘                       └─────┬──────┘
                                           │
                        ┌──────────────────┼──────────────────┐
                        │                  │                  │
                  ┌─────▼─────┐     ┌──────▼──────┐    ┌──────▼──────┐
                  │  capture  │     │  emulation  │    │   connect   │
                  │ (libei /  │     │ (wlroots /  │    │  (出站连接) │
                  │  layer-   │     │  libei /    │    └──────┬──────┘
                  │  shell /  │     │  x11 /      │           │
                  │  x11 /    │     │  macos)     │           │
                  │  macos)   │     └──────┬──────┘           │
                  └─────┬─────┘            │                  │
                        │                  │                  │
                        └──────────┬───────┴──────────────────┘
                                   │
                          ┌────────▼────────┐
                          │  DTLS over UDP  │
                          └─────────────────┘
```

关键模块：

| 模块 | 职责 |
|---|---|
| `src/service.rs` | 顶层状态机：前端请求、捕获事件、模拟事件、DNS、热键、定时器 |
| `src/capture.rs` | 捕获任务：barrier 命中、输入转发、状态机（WaitingForAck / Sending） |
| `src/emulation.rs` | 模拟任务：入站连接、按键释放、对端超时 |
| `src/connect.rs` | 出站连接、ping/pong、对端信息接收 |
| `src/listen.rs` | 监听端口、入站 DTLS 连接 |
| `src/hotkey.rs` / `hotkey_task.rs` | evdev 旁路读取 + 组合键/多击匹配 |
| `src/logging.rs` | 环形缓冲日志 + 运行时级别切换 |
| `src/sysinfo.rs` | 本机信息采集与分片收发 |
| `lan-mouse-proto/` | 线上协议（定长二进制） |
| `lan-mouse-ipc/` | 前端 ↔ daemon 的消息定义 |

---

## 6. 关键设计决策

### 6.1 指针回程

接收端（被控方）在指针到达自己屏幕边缘时，必须主动通知发送端释放，
否则发送端会一直把输入转发给一个已经停止模拟的机器，指针永远回不来。

实现分两步：

1. 接收端捕获层必须**看得见注入的指针移动**，才能判断「指针到了我的边缘」。
2. 判断成立后，通过 `ICaptureEvent::ReturnRequested` 让 service 用真实地址回 `Leave`。

**坑**：为打断反馈环而过滤掉所有合成事件，会同时让接收端失去回程判断能力。
现在只过滤合成**键盘**事件，指针移动放行；捕获中收到合成移动只更新位置、不转发。

### 6.2 对端信息交换

对端报告（OS / 内核 / 架构 / 主机名 / 版本 / commit / 显示器）在**连接建立时**
和**手动刷新**时获取，没有周期任务。

报告走 19 字节分片。原因：实测监听方向连接方发送超过约 9 字节的记录会丢失
（2 字节 `Pong` 和 9 字节 `Hello` 能到，257 字节的记录不到），而连接方向监听方
发送同样的记录没问题。分片绕开了这个不对称。

### 6.3 保活节奏

连接方每 `PING_ROUND`（2 秒）发一轮，每轮最多 4 个 ping（间隔 500 ms）。
接收方超时是 10 秒——**必须显著大于 `PING_ROUND`**，否则健康连接会被周期性拆掉。

### 6.4 macOS 权限

ad-hoc 签名会让 TCC 把授权绑定到二进制哈希，每次重编都失效。
`scripts/macos-sign.sh` 用固定证书签名，让 designated requirement 指向证书而非
二进制，重建后授权保留。

签名按证书 SHA-1 而非名称：按名称签名要求证书受信任（需要 sudo 弹窗），
按哈希则不需要。

一次性设置见脚本头部注释。

---

## 7. 构建与验证

### 7.1 本地检查

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --release --workspace
```

### 7.2 macOS 增量构建

```bash
scripts/mac-build.sh
```

只同步源码，保留 `target/` 与 `.git/`。后者用于 `shadow-rs` 打 commit 戳；
没有它，版本行只显示版本号、括号里为空。

### 7.3 图标资源

`glib_build_tools::compile_resources` 只追踪 manifest 变化，新增图标不会重建产物。
`lan-mouse-gtk/build.rs` 现在每次构建都直接调用 `glib-compile-resources`。

图标必须是 **symbolic**：填充色用 `currentColor`，不能写死十六进制值，
否则在深色主题下几乎不可见。

检查某个图标是否真的进了二进制：

```bash
GRES=$(find target/release -name lan-mouse.gresource | head -1)
gresource list "$GRES" | grep -c symbolic   # 应为 30
```

---

## 8. 故障排查

### 指针移过去回不来

1. 确认接收端服务在跑：`cli status` 看 `connected`
2. 看接收端日志有无 `Crossed barrier` 和 `pointer returned`
3. 发送端应有 `releasing capture: left remote client device region`

### 指针卡在对端，按键无反应

```bash
lan-mouse cli force-release
```

若无效，说明连接已断但发送端仍在捕获状态——检查两端 `not responding` 日志。

### macOS 授权反复失效

说明当前二进制是 ad-hoc 签名。用 `scripts/macos-sign.sh` 重签。

检查当前签名：

```bash
codesign -d -r- "/Applications/Lan Mouse.app"
# 应看到 certificate leaf = H"..."，而非 cdhash H"..."
```

### 日志级别改不了

检查是否在某处硬编码了 `LAN_MOUSE_LOG_LEVEL`：

```bash
systemctl --user show lan-mouse.service -p Environment
/usr/libexec/PlistBuddy -c "Print :EnvironmentVariables" ~/Library/LaunchAgents/de.feschber.LanMouse.plist
```

环境变量会覆盖运行时设置。

### 连接周期性断开

`not responding` 每秒出现 → 接收方超时小于发送方保活间隔。
接收方 `PEER_TIMEOUT` 应远大于连接方 `PING_ROUND`。

---

## 9. 协议

事件类型（`lan-mouse-proto`）：

| 事件 | 用途 |
|---|---|
| `Enter(Position)` / `Leave(u32)` / `Ack(u32)` | 指针进出与确认 |
| `Input(Event)` | 键鼠事件 |
| `Ping` / `Pong(bool)` | 保活与 RTT |
| `Hello { commit }` | 版本握手 |
| `PeerInfo` / `PeerInfoRequest` / `PeerInfoChunk` | 机器信息交换 |
| `Enter` / `Leave` | barrier 状态同步 |

缓冲区 `MAX_EVENT_SIZE` 为 257 字节（1 + 8×32）。
未知事件类型会被接收方跳过，因此新事件对旧版本是安全的。

---

## 10. 已知限制

- **不能交叉编译 macOS**：需要 Apple SDK。
- **macOS 每次换二进制都要重签**（`scripts/mac-sign.sh`），否则授权失效。
- **入站信息只在对端连接时获取**：对端不重连就不会更新，需手动 Refresh。
- **libei 后端不产生 `CaptureEvent::Interrupted`**：gamescope 等抢走指针的场景下，
  发送端不会立即得知，需要靠对端超时兜底。
- **日志环形缓冲 500 条**：高频 debug 下较早的记录会被丢弃。
