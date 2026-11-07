# 跨设备协同套件 · 详细实施方案

> 版本：v1.0 · 2026-11-06
> 依据：`PRODUCT_SPEC.md` v0.2
> 目标：按“可交付产品”标准落地，不是概念稿。

---

## 1. 交付定义

“完成”不是“能跑 demo”，而是：

1. 三平台安装包可安装、可卸载、可自动更新；macOS 已签名公证，Windows 已签名。
2. 用户实际环境（Linux 4K + MacBook Air M4）一次配对成功，跨屏速度/落点符合验收。
3. 断网、睡眠、权限撤销、磁盘不足、同名冲突都有可恢复路径。
4. 崩溃可诊断：输入 helper 独立进程，日志脱敏，一键导出诊断包。
5. 关键路径有自动化测试：布局几何、配对、剪贴板、续传、跨平台文件名。
6. 无“看起来能点、点了没反应”的控件；空状态、错误态、权限态全部覆盖。

**明确不做**：公网中继、跨网传输、云端账号、屏幕镜像、驱动级提权注入。

---

## 2. 已定决策（来自产品规格 0.1 / 12.1）

| 事项 | 决策 |
|---|---|
| 文件剪贴板自动拉取 | 64MB 默认，可配 16–256MB |
| Wayland 基线 | Ubuntu 24.04+；22.04 只承诺 X11 |
| 二维码 | v1 不做，v1.1 评估 |
| 富文本 | 纯文本 + 安全 HTML 子集；不做 RTF |
| 企业审计 | 仅元数据 |
| 应用层 Noise | 默认不叠 |
| 指针映射 | 物理 mm/s 归一化，协议传微米 |
| 布局绘制 | 按物理尺寸 1:1，共享边只高亮重叠段 |

---

## 3. 实测环境与分辨率适配

### 3.1 本机（Linux X11）

实测命令与结果：

```text
$ xrandr --current
Screen 0: current 3840 x 2160
HDMI-0 connected primary 3840x2160+0+0 ... 631mm x 360mm
3840x2160     60.00*+

$ xdpyinfo | grep resolution
resolution:    96x96 dots per inch

$ xrdb -query | grep Xft.dpi
Xft.dpi: 192
```

结论：

| 项 | 值 |
|---|---|
| 原生分辨率 | 3840×2160 @60Hz |
| 物理尺寸 | 631×360mm（约 28.6"，154 PPI） |
| UI 提示 | Xft.dpi=192，按 2.0× 估算 |
| 逻辑分辨率（估算） | 1920×1080，约 77 逻辑 PPI |
| 会话 | X11，HDMI-0，无 Wayland |

注意：`xdpyinfo` 的 96 DPI 与 `Xft.dpi=192` 矛盾。实现必须以 **GDK 实际 scale factor** 为准；原型按 2.0× 展示，并标注“以运行时上报为准”。

### 3.2 MacBook Air M4 32GB

32GB 是内存配置，不影响屏幕。M4 Air 有两种尺寸，程序运行时读取实际型号/模式，不靠用户手选。

| 机型 | 原生 | 默认逻辑 | 有效缩放 | 物理尺寸 | 面板 PPI | 逻辑 PPI |
|---|---|---|---|---|---|---|
| Air 13.6" | 2560×1664 | 1470×956 | 1.741× | 293×183mm | 224 | ≈128 |
| Air 15.3" | 2880×1864 | 1710×1107 | 1.684× | 330×206mm | 224 | ≈132 |

macOS 的 1470×956 是“看起来像”模式：系统先画 2940×1912，再下采样到 2560×1664。因此 **不能假设 scale=2.0**，也不能用 backing 像素做注入坐标；`CGEvent` 用逻辑点。

### 3.3 适配计算

```text
本机逻辑 px/mm      = 1920 / 631 = 3.043
Air13 逻辑 px/mm    = 1470 / 293 = 5.017
Air15 逻辑 px/mm    = 1710 / 330 = 5.182

Air13 速度倍率      = 5.017 / 3.043 = 1.65×
Air15 速度倍率      = 5.182 / 3.043 = 1.70×
Air13 共享边覆盖    = 183 / 360 = 50.8%
Air15 共享边覆盖    = 206 / 360 = 57.2%
宽度比              = 631/293 = 2.15×（13.6"）
                     631/330 = 1.91×（15.3"）
```

**实现规则**：

1. 布局画布用 mm，按 1:1 物理比例绘制；4K 屏必须明显比 Air 大。
2. 相对移动：源机 native 像素 → mm → 微米传输 → 目标逻辑像素。禁止整屏 0..1 相对映射。
3. 绝对进入：只在共享边重叠段内归一化；重叠段外为死区。
4. 两端关闭鼠标加速；否则 mm/s 归一化会被系统加速曲线破坏。
5. 共享边高亮只画重叠段，UI 显示覆盖百分比。

**验收**：

- 在本机右边缘上/中/下三点穿越，Air 落点误差 ≤2 逻辑 px。
- 本机移动 100mm，Air 指针也移动 100mm（允许 ±5%）。
- 布局画布中 4K 屏宽度 ≈ Air 的 2.15 倍（13.6"）/ 1.91 倍（15.3"）。

---

## 4. 总体架构

### 4.1 进程模型

```
┌─────────────────────────────┐
│ cd-ui (Tauri 2 + React)     │  托盘/主窗/引导；不做 OS 钩子
└──────────────┬──────────────┘
               │ JSON-RPC over UDS / Named Pipe
┌──────────────▼──────────────┐
│ cd-core (daemon)            │  会话/布局/剪贴板/传输/发现
│  ├─ transport (quinn)       │
│  ├─ clipboard pipeline      │
│  ├─ file engine             │
│  └─ store (SQLite)          │
└──────────────┬──────────────┘
               │ supervisor + typed IPC
┌──────────────▼──────────────┐
│ cd-input (helper process)   │  OS 钩子/注入；崩溃自动重启
└─────────────────────────────┘
```

为什么输入独立：OS 钩子最容易被权限、杀毒、输入法、Secure Input 打断；独立进程崩溃不影响剪贴板/传输，主进程可 1s 内拉起。

macOS TCC 注意：helper 必须在同一签名 App Bundle 内，M0 验证 TCC 授权是否覆盖 helper；若不覆盖，改为“主进程持权限 + helper 只做纯计算”的降级方案。

### 4.2 Rust workspace

```text
crates/
  cd-proto/       // 协议类型、版本、帧、错误码
  cd-geometry/    // mm 布局、边配对、DPI 变换（纯函数）
  cd-transport/   // quinn、TLS、证书固定、SAS、C0/C1/C2
  cd-discovery/   // mDNS、手动 IP、候选地址探测
  cd-input/       // 平台捕获/注入、事件归一化、helper
  cd-clipboard/   // 快照、物化、防回环、敏感应用排除
  cd-files/       // manifest、FastCDC、续传、冲突、跨平台命名
  cd-store/       // SQLite、迁移、加密、Keychain
  cd-core/        // 会话状态机、编排、IPC server
  cd-daemon/      // headless 入口
  cd-ui/          // Tauri + React
  cd-cli/         // 诊断、配对、日志导出
```

依赖规则：

- `cd-geometry` 不依赖平台、不依赖 tokio；可纯函数单测 + property test。
- `cd-proto` 不依赖 UI/平台；协议 fuzz 只测这一层。
- `cd-ui` 不直接碰网络/文件；全部走 IPC。
- `cd-input` 不依赖 UI；只依赖 `cd-proto` 事件类型与 `cd-geometry` 换算。

### 4.3 线程与运行时

- `cd-core`：tokio multi-thread；每连接一个 task；文件 I/O 用 `spawn_blocking`。
- `cd-input`：独立 OS 线程捕获；事件合并后走 IPC/DATAGRAM；注入线程独立。
- UI：Tauri 主线程 + React；所有状态来自 core 事件流。
- 存储：单 writer 线程 + WAL；避免多线程写锁。

---

## 5. 核心算法

### 5.1 屏幕模型与换算

```rust
struct MonitorInfo {
    id: String,
    native_w: u32, native_h: u32,
    logical_w: u32, logical_h: u32,
    scale: f64,             // 系统上报；macOS 用 native/logical 校正
    physical_w_mm: f64,
    physical_h_mm: f64,
    rotation: u16,
    origin_mm: (f64, f64),
}

fn native_px_per_mm(m: &MonitorInfo) -> f64 { m.native_w as f64 / m.physical_w_mm }
fn logical_px_per_mm(m: &MonitorInfo) -> f64 { m.logical_w as f64 / m.physical_w_mm }
fn effective_scale(m: &MonitorInfo) -> f64 { m.native_w as f64 / m.logical_w as f64 }
fn ui_ppi(m: &MonitorInfo) -> f64 { m.logical_w as f64 / m.physical_w_mm * 25.4 }
```

相对位移：

```rust
// 源机 native dx -> 微米
let dx_um = (dx_native / source.native_px_per_mm() * 1000.0).round() as i32;
// 目标：微米 -> 目标逻辑 px
let dx_dst_logical = dx_um as f64 / 1000.0 * target.logical_px_per_mm();
```

绝对进入：

```text
t = (y_src - overlap_src.top) / overlap_src.height
y_dst = overlap_dst.top + t * overlap_dst.height
```

只允许 `t ∈ [0,1]`；越界为死区。

### 5.2 边配对

```rust
struct EdgeSegment { monitor: MonitorId, side: Side, start_mm: f64, end_mm: f64 }
struct EdgePair { a: EdgeSegment, b: EdgeSegment, overlap_mm: f64 }

fn pair_edges(a: &[EdgeSegment], b: &[EdgeSegment]) -> Vec<EdgePair> {
    // 1. 相对边：左-右 或 上-下
    // 2. 法向间距 <= 20mm
    // 3. 投影重叠 >= 25%
    // 4. 取重叠段；角点冲突时水平边优先
}
```

覆盖率：

```text
coverage = overlap_mm / source_edge_length_mm
```

UI 必须显示 `183/360mm（50.8%）` 这类具体值，而不是只说“已连接”。

### 5.3 输入所有权状态机

```rust
enum OwnerState {
    LocalOwned,
    HandoffOut { target: DeviceId, started: Instant },
    RemoteOwned { owner: DeviceId },
    HandoffBack { from: DeviceId, started: Instant },
    Suspended { reason: SuspendReason },
}
```

转移规则：

| 事件 | 转移 |
|---|---|
| 共享边穿越 | LocalOwned → HandoffOut → RemoteOwned |
| 回切快捷键 | RemoteOwned → HandoffBack → LocalOwned |
| 目标本地输入 | RemoteOwned → LocalOwned（本机优先） |
| 目标断线 750ms | RemoteOwned → HandoffBack → LocalOwned |
| 锁屏/休眠/权限丢失 | 任意 → Suspended → LocalOwned |
| 全屏应用 | LocalOwned 不穿越；RemoteOwned 保持 |
| 拖拽中 | 按住鼠标不穿越；文件拖拽悬停 300ms 触发传输 |

交接必须发送：modifier 位图、已按下按钮、当前键盘布局、目标屏逻辑坐标。

### 5.4 剪贴板

```text
snapshot(type, content, hash, origin_id, seq)
→ 防回环：同 origin_id + hash + seq 丢弃
→ 加密推送 C2
→ 对端落盘/物化
→ 设置系统剪贴板
→ 抑制回声（10s 或直到 ack）
```

- `origin_id` 是设备 id，不是时间窗口；2s 窗口废弃。
- 敏感应用命中密码管理器时不入历史、不推送。
- 文件：≤64MB 自动落盘再上剪贴板；>64MB 引用 + 粘贴时拉取；目录必须完整传输。
- 历史密钥放 OS Keychain；SQLite 只存密文/指针；预览截断 200 字符。

### 5.5 文件传输

```text
manifest(v1) → 预检磁盘/文件系统 → 建临时目录（同盘）
→ FastCDC 4MB → blake3 块 → 块级去重
→ 写稀疏文件/part 文件 → 每块校验
→ 单文件原子 rename → 目录最后移入
→ SQLite 记录 (task, rel, offset, len, hash, state)
```

恢复：读 SQLite，比对已落盘块；只补缺失块。临时目录与目标必须同文件系统，否则 `EXDEV`。

冲突：

```text
同名 → 询问 → 覆盖 / 跳过 / 重命名 / 合并目录 → 全部应用
```

跨平台命名：Windows 非法字符、保留名、260 路径；macOS NFC；大小写冲突加 `~2`；xattr/ADS/稀疏文件 v1 不保留。

### 5.6 配对与 SAS

```text
code = blake3(sorted(pubA, pubB) || nonce)[0..6]
nonce 由发起方生成，随握手发送；两端使用同一个
两端同时显示，两端确认；60s 过期
```

首次连接 TLS 1.3 + 自签 Ed25519 证书；配对后固定证书指纹；不匹配直接拒绝。

---

## 6. 协议

### 6.1 通道

| 通道 | 传输 | 内容 |
|---|---|---|
| C0 control | QUIC stream | hello、pair、layout、heartbeat、state、error |
| C1 input | QUIC DATAGRAM | 鼠标/键盘事件，不可靠、不重传 |
| C2 data | QUIC stream | 剪贴板、文件块、manifest |

### 6.2 C1 事件（20 字节）

```text
u8  type     // 1=move 2=button 3=wheel 4=key
u8  flags    // bit0 绝对, bit1 按下, bit2 相对量 µm
u16 code     // HID usage / button
i32 x, y     // 绝对: 0..65535；相对: µm
u64 ts_us
```

### 6.3 C0 关键消息

| 消息 | 方向 | 字段 |
|---|---|---|
| `hello` | 双向 | protocol, device_id, capabilities, monitors[] |
| `pair_begin` | 发起方 | nonce, cert_fingerprint |
| `pair_confirm` | 双向 | code_hash, confirm |
| `layout_set` | 双向 | monitors[], origin_mm[], edge_pairs[] |
| `input_owner` | 双向 | owner_id, state, modifier_bitmap |
| `clipboard_announce` | 双向 | type, hash, size, origin_id, seq |
| `transfer_offer` | 双向 | task_id, manifest, total_bytes |
| `error` | 双向 | code, message, retryable |

### 6.4 版本与错误

- `major.minor`；major 不兼容拒绝；minor 用 feature bits 降级。
- 错误码沿用产品规格 4.10；UI 显示“人话 + 下一步动作”。

---

## 7. 数据模型

```sql
CREATE TABLE devices (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  cert_fingerprint BLOB NOT NULL,
  pubkey BLOB NOT NULL,
  capabilities INTEGER NOT NULL,
  paired_at INTEGER NOT NULL,
  last_seen INTEGER
);

CREATE TABLE screens (
  id TEXT PRIMARY KEY,
  device_id TEXT NOT NULL,
  idx INTEGER NOT NULL,
  native_w INTEGER, native_h INTEGER,
  logical_w INTEGER, logical_h INTEGER,
  scale REAL,
  physical_w_mm REAL, physical_h_mm REAL,
  rotation INTEGER,
  origin_x_mm REAL, origin_y_mm REAL,
  primary_flag INTEGER
);

CREATE TABLE clipboard_items (
  id TEXT PRIMARY KEY,
  origin_device TEXT NOT NULL,
  kind TEXT NOT NULL,
  content_hash BLOB NOT NULL,
  size INTEGER NOT NULL,
  preview TEXT,
  blob_path TEXT,
  pinned INTEGER DEFAULT 0,
  created_at INTEGER NOT NULL,
  expires_at INTEGER
);

CREATE TABLE transfers (
  id TEXT PRIMARY KEY,
  direction TEXT NOT NULL,
  peer_id TEXT NOT NULL,
  root_name TEXT NOT NULL,
  total_bytes INTEGER NOT NULL,
  state TEXT NOT NULL,
  dest_path TEXT NOT NULL,
  created_at INTEGER, updated_at INTEGER
);

CREATE TABLE transfer_files (
  task_id TEXT NOT NULL,
  rel_path TEXT NOT NULL,
  size INTEGER NOT NULL,
  blake3 BLOB NOT NULL,
  state TEXT NOT NULL,
  received_bytes INTEGER DEFAULT 0,
  temp_path TEXT,
  PRIMARY KEY (task_id, rel_path)
);

CREATE TABLE chunks (
  hash BLOB PRIMARY KEY,
  size INTEGER NOT NULL,
  path TEXT NOT NULL,
  refcount INTEGER NOT NULL,
  last_used INTEGER NOT NULL
);

CREATE TABLE settings (
  key TEXT PRIMARY KEY,
  value_json TEXT NOT NULL
);
```

迁移用 `rusqlite_migration`；每次升级只前进，不回滚；失败保留备份。

---

## 8. 平台实现

### 8.1 Linux X11

- 捕获：XInput2 raw events，过滤本进程注入事件（`XTest` 标记）。
- 注入：XTestFakeMotionEvent / XTestFakeButtonEvent / XTestFakeKeyEvent。
- 显示器：XRandR + EDID（`xrandr --verbose` 的 mm 尺寸）；`Xft.dpi` 仅兜底。
- 快捷键：XGrabKey，处理 NumLock/CapsLock 组合。
- 自启：systemd user service。
- 打包：AppImage + deb；依赖 WebKitGTK 4.1、libx11、libxtst。

### 8.2 Linux Wayland（24.04+）

- 会话：xdg-desktop-portal RemoteDesktop + libei/EIS。
- 捕获/注入在同一 portal 会话内；每次会话可能弹系统授权，UI 必须解释。
- 全局快捷键：GlobalShortcuts portal；不可用时降级为应用内快捷键。
- 显示器：portal 可能不提供 EDID mm；缺失时用逻辑像素估算，布局标记“估算”。
- 不承诺 22.04；M0 必须实测 24.04 的 portal/libei 版本。

### 8.3 macOS 13+

- 捕获：CGEventTap（Accessibility + Input Monitoring）；Secure Input 时降级并提示。
- 注入：CGEventPost；坐标用逻辑点。
- 显示器：NSScreen + CGDisplayMode；逻辑分辨率/缩放/物理 mm；Retina 缩放模式不能假设 2.0。
- 权限：Accessibility、Input Monitoring；macOS 15+ 本地网络；首次引导 + 状态检测。
- 签名：Developer ID + notarization + hardened runtime；helper 同 bundle。
- 自启：LaunchAgent。

### 8.4 Windows 10/11

- 捕获：WH_MOUSE_LL / WH_KEYBOARD_LL；消息线程独立。
- 注入：SendInput；进程 Per-Monitor DPI Aware V2；坐标按目标屏物理像素换算。
- 显示器：EnumDisplayMonitors + EDID；混合 DPI 每屏独立。
- 权限：UIPI 不能注入提权窗口；文档说明，不做驱动绕过。
- 安装：MSI/NSIS + 代码签名 + WebView2 bootstrapper + 防火墙入站规则。
- 自启：注册表 Run / 计划任务。

### 8.5 Tauri

- 单实例插件；托盘；全局快捷键；自动更新（签名清单）；窗口位置恢复。
- 最小窗口 1024×680；低于 1000 宽时设备页堆叠；布局画布自适应缩放。
- Linux WebView 用 WebKitGTK 4.1；Windows WebView2；macOS WKWebView。

---

## 9. UI 实现

### 9.1 技术栈

- React 18 + TypeScript + Vite；Zustand 或 reducer；CSS 变量设计令牌。
- 组件：
  - `DeviceList`、`LayoutCanvas`、`AdaptationCard`、`PairingWizard`
  - `ClipboardList`、`ClipboardDetail`、`TransferList`
  - `Settings`、`PermissionWizard`、`TrayMenu`
  - `Toast`、`EmptyState`、`ErrorState`、`ConfirmDialog`
- 所有状态来自 core 事件流；UI 不轮询。

### 9.2 布局画布

- 输入：`MonitorInfo[]` + `origin_mm`；输出：拖拽/对齐后的 mm 坐标。
- 缩放：`fit` 计算 `px_per_mm = min(canvas_w / layout_w_mm, canvas_h / layout_h_mm)`。
- 拖拽：mm 坐标，10mm 网格吸附；重叠拒绝；越界限制。
- 共享边：只画 `EdgePair` 的重叠段；显示覆盖率。
- 对齐：居中/顶部/底部一键；拖拽后变自定义。
- 键盘：方向键 10mm 微调，Shift 1mm；Esc 取消。
- 辅助信息：物理尺寸、逻辑分辨率、有效缩放、PPI、速度倍率。

### 9.3 适配卡片

设备页左侧固定显示：

```text
分辨率适配                    [通过]
本机     3840×2160 · 2.0× · 631×360mm
对端     Air 13.6" 2560×1664 · 1.74× · 293×183mm
共享边   183/360mm = 50.8%
速度     逻辑像素 1.65×；物理速度 1:1
建议     Air 垂直居中；关闭鼠标加速
[查看计算过程] [应用推荐布局]
```

- 对端尺寸下拉：13.6" / 15.3"，切换后画布、指标、建议全部联动。
- “查看计算过程”弹窗列出公式、实测值、macOS backing 说明。
- 未配对/离线时状态变“不可用”，但仍显示理论值。

### 9.4 状态与可达性

- 空：无设备、无剪贴板、无传输、无配对结果。
- 加载：发现中、配对中、接收中、重连中。
- 错误：AP 隔离、短码不一致、权限拒绝、磁盘不足、文件系统不支持、同名冲突。
- 权限：输入灰置 + 向导；剪贴板/文件仍可用。
- 键盘：全部交互可 Tab 到达；模态焦点陷阱；Esc 关闭。
- 对比度：正文 ≥4.5:1；小字 ≥11px；支持 reduced motion。

---

## 10. 里程碑与工作分解

> 只列顺序与依赖，不给时间估算。

### M0 · 风险验证

| ID | 任务 | 验收 |
|---|---|---|
| M0-1 | Linux 4K ↔ Air 13.6 双 DPI 布局 spike | 速度误差 ≤5%，覆盖 50.8%，落点 ≤2px |
| M0-2 | macOS 1470×956 / 1710×1107 模式读取 | 逻辑分辨率、mm、有效缩放正确 |
| M0-3 | Wayland 24.04 portal/libei | 能注入/捕获；授权流程可解释 |
| M0-4 | Windows 钩子 + SendInput + 防火墙 | Win10/11 可用；UIPI 提示明确 |
| M0-5 | QUIC DATAGRAM 延迟 | 1000 次移动 P95 ≤12ms |
| M0-6 | 文件剪贴板物化 | CF_HDROP / NSPasteboard 小文件可粘贴 |
| M0-7 | macOS TCC + helper | 权限覆盖 helper；不覆盖则有降级方案 |

任一项失败 → 缩小平台范围，不硬扛。

### M1 · 骨架（Linux X11 ↔ macOS）

| ID | 任务 | 验收 |
|---|---|---|
| M1-1 | workspace、proto、geometry | 单测/属性测试通过 |
| M1-2 | 发现 + SAS 配对 + 证书固定 | 双端同码；指纹不匹配拒绝 |
| M1-3 | 布局编辑器（mm、1:1、共享边） | Air 13.6/15.3 两种尺寸正确 |
| M1-4 | 输入 helper + 所有权状态机 | 穿越/回切/锁屏/断线 |
| M1-5 | 文本/图片剪贴板 | 防回环、敏感应用排除 |
| M1-6 | 文件剪贴板 ≤64MB | 落盘后粘贴 |

验收：连续 2 小时，输入 P95 ≤12ms，无粘键。

### M2 · 文件体系

| ID | 任务 | 验收 |
|---|---|---|
| M2-1 | manifest 流式生成 | 10 万文件不卡 UI |
| M2-2 | FastCDC + blake3 + 块库 | 秒传命中；缓存配额 |
| M2-3 | 落盘/续传/原子 rename | 2GB 断网 30s 恢复；blake3 一致 |
| M2-4 | 冲突策略 | 覆盖/跳过/重命名/合并 |
| M2-5 | 跨平台命名 | Windows 非法字符/保留名/NFC/大小写 |
| M2-6 | 传输面板 | 进行中/暂停/失败/完成 |

### M3 · 平台补齐

| ID | 任务 | 验收 |
|---|---|---|
| M3-1 | Wayland 24.04+ | 注入/捕获/授权 |
| M3-2 | Windows 钩子/注入/防火墙 | 三平台两两组合 |
| M3-3 | 权限向导 | macOS 15 本地网络；X11/Wayland 状态 |
| M3-4 | 设备能力位/撤销/隐身 | 收回权限立即生效 |
| M3-5 | 安装包/签名/公证 | 干净机器可安装 |

### M4 · 打磨发布

| ID | 任务 | 验收 |
|---|---|---|
| M4-1 | 隐私模式/密码管理器排除 | 不记录、不推送 |
| M4-2 | 拖拽文件跨屏/限速 | 不打断普通拖拽 |
| M4-3 | 诊断日志/自动更新 | 脱敏；可回滚 |
| M4-4 | 企业审计（元数据） | 不碰内容 |
| M4-5 | 10 人 2 周试用 | 配对 ≥90%，传输 ≥98% |

---

## 11. 测试计划

### 11.1 单元与属性测试

- 几何：随机屏幕尺寸/位置，边配对不重叠、覆盖率计算正确、往返映射误差 ≤2px。
- DPI：随机 native/logical/mm，速度倍率误差 ≤5%。
- 协议：帧解析 fuzz；版本降级；错误码。
- 文件：随机目录树，manifest 往返；Windows 命名映射；冲突策略。
- 剪贴板：同源回声、不同源同内容、敏感应用、阈值边界。

### 11.2 集成与 E2E

| 组合 | 重点 |
|---|---|
| Linux X11 ↔ macOS Air 13.6 | 4K 2× ↔ 1.74×，共享边 50.8% |
| Linux X11 ↔ macOS Air 15.3 | 共享边 57.2% |
| macOS ↔ Windows | 键盘布局、UIPI、防火墙 |
| Wayland 24.04 ↔ macOS | portal 授权、每次会话 |
| 三台 A→B→C | 所有权栈、回切 |

### 11.3 性能

- 延迟：硬件计时/示波器或高精度日志；capture→inject P95。
- 吞吐：1GB/10GB 文件；千兆 ≥80MB/s。
- 内存：8 小时 soak；常驻 <120MB。
- 启动：托盘就绪 <1.5s。
- CPU：空闲 <1%，输入时 <5%。

### 11.4 安全

- MITM 模拟：短码不一致必须中止。
- 证书指纹变化：拒绝，不弹“继续”。
- manifest 路径穿越 `../`、绝对路径、符号链接：拒绝。
- 剪贴板敏感内容：密码管理器排除；日志无内容。
- 端口扫描/未配对设备：零权限。

### 11.5 验收清单（用户实际环境）

- [ ] Linux 4K 作为本机，Air 13.6 或 15.3 作为对端，配对成功。
- [ ] 布局画布按物理比例显示，4K 屏明显更大。
- [ ] 共享边只覆盖 Air 高度（50.8% / 57.2%），UI 显示数值。
- [ ] 本机右边缘上/中/下穿越，Air 落点误差 ≤2px。
- [ ] 本机移动 100mm，Air 移动 100mm（±5%）。
- [ ] Air 默认 1470×956；切到 1710×1107 后指标自动更新。
- [ ] 两端关闭鼠标加速后速度一致。
- [ ] 剪贴板文本/图片/≤64MB 文件/大文件引用/目录传输。
- [ ] 断网 30s 后续传只补缺失块。
- [ ] 权限撤销 750ms 内回切。
- [ ] 8 小时不断连。

---

## 12. 性能预算

| 阶段 | 预算 |
|---|---|
| 捕获 + 合并 | 1–2ms |
| 序列化 + 加密 | <0.5ms |
| 局域网传输 | 0.5–1ms |
| 注入 | 1–2ms |
| 调度/唤醒 | 2–4ms |
| 合计 P95 | ≤12ms |

优化顺序：输入线程优先级 > 事件合并 > DATAGRAM > 注入路径 > 协议压缩（不压缩输入）。

---

## 13. 交付与发布

- CI：Linux/macOS/Windows 构建矩阵；`cargo test`、`clippy -D warnings`、`rustfmt`、前端 lint/test。
- 签名：macOS Developer ID + notarization；Windows EV/OV 证书；Linux 校验和。
- 安装包：macOS dmg/pkg；Windows MSI/NSIS；Linux AppImage/deb。
- 更新：Tauri updater，签名清单，stable/beta；可关闭；失败回滚。
- 诊断：滚动日志 7 天、脱敏、`cd-cli diagnostics export`。
- 文档：快速开始、权限向导、AP 隔离、文件冲突、企业部署、隐私说明。
- Definition of Done：安装、配对、跨屏、剪贴板、文件、续传、权限、错误态、更新、诊断全部通过验收清单。

---

## 14. 风险与缓解

| 风险 | 触发 | 缓解 | 退出条件 |
|---|---|---|---|
| Xft.dpi 与 GDK scale 不一致 | 本机 4K | 运行时读 GDK；原型标注估算；M0 实测 | 布局/注入正确 |
| Air 缩放模式变化 | 用户改“更多空间” | 读取实际模式，自动重算 | 指标正确 |
| macOS TCC 不覆盖 helper | 权限授予 helper 失败 | helper 同 bundle；降级主进程注入 | M0 通过 |
| Wayland 每次弹 portal | 24.04 | 文案解释；会话保持；降级 X11 | 可接受 |
| Windows 防火墙 | 首次连接 | 安装写规则；手动兜底 | 连接成功 |
| 输入加速曲线 | 两端系统设置 | 引导关闭加速；读取原始事件 | 速度误差 ≤5% |
| 文件系统 EXDEV | 临时目录跨盘 | 同盘临时区；复制回退 | 续传正确 |
| 同名/非法字符 | 跨平台 | 冲突策略 + 命名映射 | 无覆盖事故 |
| 块缓存膨胀 | 长期使用 | 8GB/10% 上限 + LRU | 磁盘可控 |
| 杀毒误报 | Windows 钩子 | 签名、白名单、企业文档 | 安装成功 |

---

## 15. 开放项

1. 定价/授权模式（商业）。
2. 企业审计范围书面确认。
3. 产品名商标检索。
4. Air 实际尺寸由程序读取，原型两种都可切换。

---

## 附录 A · 实测数值

```text
本机：3840×2160, 631×360mm, 154 PPI, Xft.dpi 192 → 2.0× → 1920×1080, 77 PPI
Air13：2560×1664, 293×183mm, 224 PPI, 1470×956 → 1.741×, 128 PPI
Air15：2880×1864, 330×206mm, 224 PPI, 1710×1107 → 1.684×, 132 PPI

速度倍率 Air13 = (1470/293)/(1920/631) = 1.649
速度倍率 Air15 = (1710/330)/(1920/631) = 1.703
共享边 Air13 = 183/360 = 50.8%
共享边 Air15 = 206/360 = 57.2%
```

## 附录 B · 布局伪代码

```rust
fn map_relative(dx_native: f64, src: &MonitorInfo, dst: &MonitorInfo) -> f64 {
    let dx_mm = dx_native / src.native_px_per_mm();
    dx_mm * dst.logical_px_per_mm()
}

fn map_entry(y_src: f64, overlap_src: RectMm, overlap_dst: RectMm) -> Option<f64> {
    if y_src < overlap_src.top || y_src > overlap_src.bottom { return None; }
    let t = (y_src - overlap_src.top) / overlap_src.height;
    Some(overlap_dst.top + t * overlap_dst.height)
}
```

## 附录 C · 验收记录模板

| 项 | 实测 | 目标 | 结果 |
|---|---|---|---|
| 输入 P95 | | ≤12ms | |
| 速度误差 | | ≤5% | |
| 落点误差 | | ≤2px | |
| 共享边覆盖 | | 50.8/57.2% | |
| 2GB 续传 | | 只补缺失块 | |
| 8h soak | | 不断连 | |
