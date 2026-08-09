# Remote 重构初始化：使用自动引导的远程 PTY Holder 替代 `tmux`

## 概述

Diri 当前通过 SSH 启动远程 Agent，并使用远程机器上的 `tmux` 保持会话存活和实现重新连接。

当前执行链路大致为：

```text
Diri
  → 本地 PTY
  → ssh -t
  → 远程 SSH PTY
  → 远程 tmux PTY
  → Claude Code / Codex / Shell
```

这个方案比较务实，但它要求每台远程机器都预先安装 `tmux`，并且在 Diri 与 Agent 之间增加了多层终端模拟。

本次重构使用一个由 Diri 自动上传和启动的轻量级远程 PTY Holder，替代远程 `tmux`。

修改后的链路可以变为：

```text
Diri
  → ssh -T 二进制通道
  → 临时远程 Bridge
  → Diri Remote PTY Holder
  → Claude Code / Codex / Shell
```

目标是在保留当前 SSH 开箱即用体验的同时：

* 去掉对 `tmux` 的强制依赖；
* 减少远程运行环境要求；
* 让 Diri 直接拥有远程 Agent 的 PTY 和进程生命周期；
* 建立更清晰、可恢复的远程终端协议。

---

## 本次重构的基线与硬约束

本提案以当前 Rust workspace 为唯一实现基线。对于本次 remote 重构，顶层 `Sources/`、`Package.swift` 和 Swift tests 视为不存在：

* 不从 Swift 实现移植行为；
* 不以 Swift wire format、Holder protocol 或磁盘格式兼容作为验收条件；
* 不为 Swift daemon 增加 remote 兼容层；
* 不运行 Swift 构建或测试来证明本次重构正确；
* Rust 代码中现存的 Swift 兼容注释只描述历史来源，不构成新的设计约束。

本次重构的事实来源按以下顺序确定：

1. 本文档定义的目标、边界和验收场景；
2. `diri/crates/diri-engine` 中已经工作的 Rust Engine；
3. `diri/crates/diri-proto` 中已有的 Rust 数据模型和终端 Frame；
4. `diri/crates/diri-client`、`diri-term` 和 `diri-app` 的现有消费行为。

重构开始前，Rust 远程路径完整实现了 SSH + `tmux`：

* `diri-engine/src/remote.rs` 生成本地 `ssh -t` 与远程 `tmux new-session -A` argv；
* `diri-engine/src/control.rs` 负责 remote spawn 和 resume；
* `diri-engine/src/migrate.rs` 仍按 tmux session name 清理远端会话；
* 本地 Holder、PTY、terminal state 和 attach broadcaster 分别位于 `holder/`、`pty/`、`screen.rs` 和 `attach.rs`；
* `diri-proto` 已经提供 `Grid`、`Scroll`、`Modes`、`Input`、`Resize`、`Ping` 和 `Pong` Frame。

因此这不是从 Swift 再做一次 port，而是替换 Rust Engine 中原有的 remote transport。原 Rust SSH + `tmux` transport 已在第零阶段直接删除，不保留 `legacy_tmux`、运行时开关或隐式回退；重构期间尚未接通新 transport 的 remote 操作返回结构化 `remote_transport_unavailable`，不能继续启动 `tmux`。

### 已确认的设计优先级

本次重构按以下顺序做设计和验收：

1. **稳定性与正确性是发布门槛**：不能丢输入、错连 Session、阻塞 PTY、破坏终端状态或误杀进程；性能优化不能削弱这些保证；
2. **高性能是同级硬约束**：在满足正确性的方案中，选择经过 benchmark 证明具有更低延迟、CPU、内存和复制开销的实现；
3. **最小权限是不可突破的安全边界**：只使用当前 SSH 用户已有的权限和用户目录，不以“提高持久性”为理由申请额外权限；
4. **简单可恢复优先于功能宽度**：第一阶段只实现一个可靠的控制连接，不提前加入多观察者、完整远程服务或非 PTY 能力。

禁止执行或要求用户执行：

* `sudo`、root 登录或 setuid capability；
* 安装 `tmux` 或其他系统软件包；
* 修改 PAM、sshd、systemd system service、launch daemon 或全局 shell 配置；
* `loginctl enable-linger`、写入持久 user unit/LaunchAgent，或以其他方式改变主机登录策略；
* 复制 SSH 私钥、认证响应、本地 Keychain secret 或完整本地 environment 到远端。

---

## 背景与动机

当前 `tmux` 方案解决了三个重要问题：

1. Agent 在远程真实 PTY 中运行；
2. SSH 暂时断开后，Agent 仍然可以继续运行；
3. Diri 可以重新连接已有远程会话。

但对 `tmux` 的依赖也带来了一些限制。

### 远程外部依赖

每一台远程机器都必须安装兼容版本的 `tmux`。

这会影响以下环境：

* 极简容器；
* 权限受限的开发服务器；
* 新创建的云主机；
* 用户无法安装软件包的托管环境；
* 只提供基础 SSH 能力的远程环境。

### 多层终端模拟

当前执行链路可能包含：

```text
本地 PTY
→ 远程 SSH PTY
→ tmux Pane PTY
→ Agent
```

每一层都有可能影响：

* `$TERM`；
* 颜色；
* 鼠标事件；
* 窗口 resize；
* alternate screen；
* 控制序列；
* TUI 排版行为。

### 无法直接管理远程 Agent 进程

当前本地 Holder 真正拥有的是本地 `ssh` 进程，而不是远程 Agent 的进程树。

因此 Diri 很难直接、精确地处理：

* 远程前台进程组；
* Agent 的子进程；
* 远程 signal；
* 远程资源统计；
* 远程端口；
* 真实退出原因。

### 远程 Hooks 和结构化事件受限

本地 Agent Hooks 和 MCP 配置通常引用：

* 本地文件路径；
* 本地可执行程序；
* 本地 Unix Socket。

这些资源在远程机器上不存在。因此远程会话经常只能依赖屏幕内容识别 Agent 状态，而不能完整使用结构化事件。

### Diri 已经具备 Holder 架构

Diri 本地已经通过独立 Holder 将：

* PTY 生命周期；
* Agent 生命周期；
* GUI 生命周期；
* daemon 生命周期；

相互解耦。

同样的 Holder 模型可以应用到远程环境，而不必要求用户安装完整的远程 Diri 服务。

---

## 已确认方案

增加一个单文件、轻量级的远程 Helper，暂定名：

```text
diri-remote
```

当用户第一次使用某台远程主机时，由 Diri 自动上传。

用户不需要手工安装或配置。

远程机器只需要：

* 可以通过 SSH 登录；
* Home 或临时目录可写；
* 已安装需要运行的 Agent；
* 操作系统和 CPU 架构受到支持。

基础模式不应该要求：

* `tmux`；
* `screen`；
* `zellij`；
* Node.js；
* Python；
* `socat`；
* `nc`；
* 常驻安装的 `diri-node`；
* 用户手工配置的 systemd 或 launchd 服务。

---

## 远程组件的职责

远程 Helper 应保持足够小，不应演变成第二套完整的 Diri Engine。

一个二进制可以提供以下子命令：

```text
diri-remote launch
diri-remote attach
diri-remote inspect
diri-remote list
diri-remote kill
diri-remote probe
diri-remote gc
diri-remote directories
```

`directories` 是桌面端 New Agent 目录选择器所需的唯一只读环境辅助
命令：接收结构化路径，只枚举一层目录，并对扫描量、返回条目与响应大小设置
硬上限。它不读取文件内容、不递归、不监控目录，也不改变 Holder 的状态权威
边界；本地 Engine 仍拥有 host/project 选择和 UI 编排。

### 持久化 Session Holder

每个远程 Agent 会话对应一个独立 Holder 进程，负责：

* 持有 PTY master；
* 持有 Agent 子进程；
* 转发用户输入；
* 处理终端 resize；
* 发送 signal；
* 记录进程退出状态；
* 维护当前终端状态；
* 通过 Unix Domain Socket 接受重新连接；
* 保存有界输出日志。

### 临时 SSH Bridge

`attach` 命令只作为短生命周期 Bridge：

```text
SSH stdin/stdout
  ↔ 远程 Unix Domain Socket
  ↔ Remote Holder
```

Bridge 不负责 Agent 编排或会话业务逻辑。

当 SSH 连接断开时：

* Bridge 退出；
* 在远程主机允许 detached user process 存活的情况下，Holder 和 Agent 继续运行。

### 应用退出后的进程生命周期

GUI 退出不是 Session 终止操作。后台进程必须按照是否仍拥有活跃 Session 明确收敛：

* 没有活跃 Session、也没有其他 control client 时，App 请求本地 Rust Engine 持久化并退出；Engine 同时关闭 Diri 私有的 OpenSSH ControlMaster，并要求空闲的本地 Holder Manager 立即退出；
* 仍有本地 Session 时，本地 Engine、单个 Holder Manager、对应 Holder 和 Agent 进程树继续存在；Engine 继续承担状态归约和编排，Holder 则保证 Agent 不依赖 GUI/Engine 存活；最后一个本地 Session 退出后，Manager 仍保留一个有界的短 grace period，随后自行退出；
* 仍有远程 Session 时，本地 Engine 及当前 `ssh -T` Bridge 继续承担状态归约、终端同步和编排；远端每 Session 一个 `diri-remote` Holder、process guard 和 Agent 进程树独立保活。Bridge 因网络中断或 Engine 重启而退出不影响远端 Session，有限时长 ControlMaster 也不承担保活；
* 没有 Session 时，不允许为了“加速下一次打开”保留 Engine、Holder、Remote Helper、SSH 或 node 进程；Helper 的版本化二进制缓存只是文件，不是后台服务；
* `diri-node` 只允许由用户显式启动的增强模式使用，默认 SSH bootstrap 不得自动启动或保留它。

自然退出的 Agent 必须先完成 Session 清理再被视为空闲。退出与 GUI 关闭发生竞争时，应宁可暂时拒绝 Engine 退出，也不能误杀仍可能活跃的 Agent；下次 authoritative cleanup 可以继续收敛残留记录。

---

## SSH 传输方式

新实现应使用：

```bash
ssh -T
```

替代：

```bash
ssh -t
```

SSH 在新架构中只负责：

* 身份验证；
* 加密；
* 远程命令执行；
* 二进制字节流传输。

它不再作为终端生命周期的所有者。

这样可以避免 SSH PTY 的行规程和控制字符处理，并允许直接传输 Diri 已有的二进制终端 Frame。

同一远程主机上存在多个 Agent 会话时，可以通过 OpenSSH 连接复用降低重复认证和握手成本：

```text
ControlMaster=auto
ControlPersist=<有限时长>
```

但 ControlMaster 只能是性能优化，不能承担 Agent 会话持久化职责。

---

## 参考 Zed 的远程引导模型

Diri 当前锁定的 GPUI 上游是 Zed commit [`dc2a339`](https://github.com/zed-industries/zed/commit/dc2a339d5d043da448a3f7ddc7c0a85c63864aad)，因此本次设计固定参考同一提交的远程实现，而不是不稳定的 `main`：

* [Zed Remote Development 文档](https://zed.dev/docs/remote-development)；
* [SSH transport 与 server binary 安装](https://github.com/zed-industries/zed/blob/dc2a339d5d043da448a3f7ddc7c0a85c63864aad/crates/remote/src/transport/ssh.rs)；
* [远程 server 的 proxy/daemon 重连](https://github.com/zed-industries/zed/blob/dc2a339d5d043da448a3f7ddc7c0a85c63864aad/crates/remote_server/src/server.rs)；
* [登录 shell 环境捕获](https://github.com/zed-industries/zed/blob/dc2a339d5d043da448a3f7ddc7c0a85c63864aad/crates/util/src/shell_env.rs)。

Zed 的流程可以概括为：

```text
建立 SSH ControlMaster
→ 探测远端 shell / OS / arch
→ 检查精确版本的 remote server binary
→ 缺失时下载或经 SSH 上传
→ 临时文件解包、chmod、原子移动到最终路径
→ 通过新的复用 SSH Channel 启动短生命周期 proxy
→ proxy 启动或重新连接远程 daemon
```

Diri 应采用其中已经被验证的引导模式：

* 继续调用用户系统中的 OpenSSH，并尊重 `~/.ssh/config`、ProxyJump、IdentityFile 和 host key 策略；
* 将认证、host key 和 key passphrase 提示反馈到 Diri UI；
* 建立有限生命周期的 ControlMaster，后续 probe、upload、launch 和 attach 复用连接；
* 先探测平台，再选择精确构建；
* 使用带 Build ID 的并存二进制，安装过程写临时文件并原子提交；
* 将每次 SSH stdio 连接限制为 Bridge，真正可重连的状态保留在远端独立进程中；
* 在远端进程启动时加载真实登录 shell 环境。

但 Diri 不复制 Zed 的完整远程 server 模型：

* Zed server 管理 project、language server、task、extension 等大量远程业务；`diri-remote` 只管理 PTY、Agent 进程和终端状态；
* Zed 的新建 proxy 可以替换其 project daemon；Diri 不得因为新的 attach 或 Helper 升级杀死已有 Holder；
* Zed 默认可以让远端使用 `curl`/`wget` 下载二进制；Diri 第一阶段默认从本地应用包上传，确保“SSH + 可写目录”仍是完整依赖集合；
* Zed 的 ControlMaster 生命周期与 project 相连；Diri 的复用粒度固定为远程 host，并且不能成为 Session 存活条件。

---

## 自动引导流程

Diri 第一次连接远程主机时，应执行一个显式、可重试的 bootstrap state machine：

1. 使用 host 配置建立 SSH ControlMaster，并完成所有交互式认证；
2. 通过 `ssh -T` 运行固定的 probe 命令，探测 remote home、login shell、OS 和 CPU 架构；
3. 将平台规范化为 Diri 支持的 target tuple；
4. 从本地应用包选择对应的 `diri-remote` artifact，并读取其 protocol、Build ID、长度和 SHA-256；
5. 调用目标路径上的 `diri-remote probe --format=json`，命中完全相同的 protocol、Build ID 和 artifact hash 时跳过安装；
6. 缺失或不匹配时，在目标版本目录写入带随机 nonce 的临时文件；
7. 优先直接通过 SSH stdin 上传原始二进制；SFTP/SCP 可以作为优化，但不能成为远端依赖；
8. 对临时文件设置 owner-only executable mode，再执行其 `probe` 自校验；
9. 校验成功后使用同目录原子 rename 提交到最终路径；
10. 运行 persistence probe，并在本地缓存该 host 的能力结果；
11. 通过固定 Helper 路径启动或连接 Session Holder；
12. 新建 `ssh -T` Channel 运行 `diri-remote attach`，完成 `Hello`/`HelloAck` 后进入二进制 Frame 模式。

Bootstrap 每一步必须幂等。两个 Diri 进程并发安装同一 Build ID 时可以各自写不同临时文件，但最终只能提交完全相同的 artifact；失败和取消只清理本次创建的临时文件，不得删除已经验证的版本或任何 live session。

第一次 bootstrap 后，Engine 在每个新的、无状态的远程动作前都必须 probe 当前应用包对应的精确 Build ID、protocol 和 artifact hash。进程内只缓存已经规范化的平台 target，避免每次重复执行 `uname`；Helper 身份仍需通过有限生命周期 ControlMaster 上的一次短 `ssh -T` round trip 验证。probe 缺失、损坏或版本不匹配时，动作先进入上述安装流程，验证成功后才继续。因此 Diri 更新后的第一次远程动作会自动同步远程环境，不依赖用户手工升级。

`probe` 还必须声明完整的第一阶段 Helper 能力集合：Holder terminal、Session management、environment capture、directory list、persistence probe 和 atomic activation。Engine 在安装复用、临时文件自校验、激活后校验及每次无状态动作前统一验证该集合；缺少任何必需能力时必须先安装当前 Helper，不能等到调用 `directories` 等命令后才暴露 `unknown command`。协议 1.2 起，能力集合由 `diri-proto::remote_pty` 单一声明并由 Engine 与 Helper 共用；未知的可选能力可以忽略，缺失的必需能力 fail closed。

正式应用包只从经过完整性验证的跨平台 `remote-helpers/manifest.json` 构建 catalog。直接运行 `cargo build` 产生的 loose Engine 则优先使用同目录刚构建的原生 `diri-remote`；显式的 `DIRI_REMOTE_HELPER_PATH` 仍具有最高优先级。这样历史构建留下的 `target/release/remote-helpers` 不会遮蔽当前 sibling Helper，而打包布局仍保留完整的受支持平台 artifact 集合。

本地 Rust Engine 是长生命周期进程，不能仅凭应用重新打开就假设它已经使用新 catalog。Engine 启动时计算并缓存自身可执行文件 SHA-256，`Hello.executableHash` 向 App 报告该身份；App 启动时与当前 bundle 内 `dirijord-rs` 比较。哈希缺失或不一致时，仅对已确认的 Rust Engine 请求持久化退出并启动 bundle 内新 Engine，Holder/Agent 不退出，新 Engine 通过既有 binding 完成 adoption。无法解析 bundle Engine 或遇到非 Rust daemon 时 fail closed，不猜测所有权也不强制替换。

这个版本门禁适用于新建/恢复 Session、目录浏览、远程 Git 检查、偏好同步与仓库定位。已经运行的 Holder、attach、inspect、signal 和 kill 继续使用 Session 创建时记录的 Build ID；它们验证该历史版本与 Session incarnation，而不能被当前应用版本原地替换或中断。

SSH Host 管理界面提供显式的“Reinstall Environment”。它强制将当前包内 artifact 重新经过 nonce 临时路径上传、自校验和 no-replace 激活，并重新执行环境捕获与 persistence probe。若相同内容寻址版本已经存在，激活幂等复用它；若最终路径被不同内容占据则 fail closed，绝不覆盖可能被 live Session 引用的二进制。

远程目录结构固定为：

```text
~/.cache/diri/bin/
  protocol-1/
    <build-id>/
      diri-remote

~/.local/state/diri/sessions/
  <session-id>/
    session.json
    output.log

$XDG_RUNTIME_DIR/diri/ 或 owner-only 临时运行目录：
  <session-id-hash>.sock
```

Unix Socket 使用短运行时目录和 Session ID 哈希，避免 macOS `sockaddr_un`
路径长度上限；认证与 incarnation 校验仍由协议完成，Session 的持久状态仍只在
`~/.local/state/diri/sessions/` 下保存。

目录和文件权限至少应满足：

```text
~/.cache/diri/                  0700
~/.cache/diri/bin/.../          0700
diri-remote                     0700
~/.local/state/diri/            0700
session.json / output.log       0600
holder.sock                     0600
```

安装器不得跟随不受信任的 symlink 覆盖任意路径。最终目录必须属于当前远程用户，session ID、protocol 和 Build ID 必须先按严格字符集校验，再用于构造路径。

Linux 版本采用 musl 静态链接，尽量避免远端运行库依赖。

macOS 版本可以是仅依赖系统库的单文件 Mach-O。

不同版本的 Helper 不应原地覆盖：

```text
错误：
~/.cache/diri/diri-remote

正确：
~/.cache/diri/bin/<protocol>/<build-id>/diri-remote
```

旧会话继续使用创建时对应的 Helper 版本。没有任何会话引用的旧版本可以由 GC 自动清理。

GC 必须保留：

* 当前本地 Engine 对应的 Build ID；
* 任意 `session.json` 引用的 Build ID；
* 最近一次成功 attach 的有限个可回滚 Helper 版本。

陈旧的 `.tmp-*` 可以按年龄清理，但 GC 不能只依赖进程列表推断版本是否仍在使用。

---

## 终端恢复

仅保存有界原始输出日志，不足以保证重新连接后恢复任意全屏 TUI。

终端程序可能执行：

* 移动光标；
* 擦除已有内容；
* 使用 alternate screen；
* 覆盖之前的行；
* 只更新局部区域。

因此 Remote Holder 应维护权威的当前终端状态：

```text
PTY 字节
  → 终端解析器
  → Grid + Cursor + Terminal Modes
```

重新连接时发送：

```text
完整终端快照
→ 后续增量 Grid 更新
```

可以复用 Diri 现有协议中的：

* `Grid`；
* `Scroll`；
* `Modes`；
* `Input`；
* `Resize`；
* `Ping`；
* `Pong`。

正常运行时只发送发生变化的行，并对短时间内的更新进行合并。

慢速客户端或断开的客户端不能阻塞 PTY 读取。增量更新无法可靠继续时，应丢弃过期 diff，并重新发送完整快照。

第一阶段的 `FullSnapshot` 固定只包含当前可见 Grid、Cursor、Terminal Modes、尺寸和单调递增的 sequence，不包含 scrollback。Holder 仍维护最多 4 MiB 的有界 scrollback；历史内容通过 `Scroll` 按需读取，不能延迟重连后的首屏。

Terminal state 的单一 owner 必须持续消费 PTY 并更新 parser；attach client 只能读取不可变 snapshot/diff，不能持锁进入 PTY 读取路径。每条连接使用有界发送队列；队列溢出时丢弃该连接的旧 diff，并在最新 sequence 上重新播种 `FullSnapshot`。

---

## 权威边界

远程 Helper 不应成为另一套完整的会话引擎。

### Remote Holder 对以下状态负责

* PTY 生命周期；
* Agent 进程生命周期；
* 当前终端 Grid；
* 光标和终端模式；
* 输出 offset；
* 进程退出状态；
* 当前输入控制权。

### 本地 Diri Engine 对以下状态负责

* `SessionRecord`；
* 项目和 worktree 信息；
* Agent Manifest；
* 状态 Reducer；
* GUI 广播；
* 跨会话编排；
* 面向用户的生命周期操作；
* 远程主机管理。

远程 Helper 只报告可观测事实。Working、Permission、Question、Done 等高层 Agent 状态仍然由本地 Engine 统一判断。

---

## 持久化能力探测

在所有 Linux 主机上，仅使用 `setsid()` 或 double fork 并不能绝对保证 SSH 退出后进程继续存活。

部分服务器可能通过 PAM 或 systemd-logind，在登录会话结束时终止属于该会话的所有进程。

因此 Diri 不应默认假设 Remote Holder 一定具备与 `tmux` 相同的持久化能力。

每台远程主机首次使用时必须执行一次持久化探测：

1. 启动临时测试 Holder；
2. 主动关闭 SSH Channel；
3. 重新连接；
4. 检查 Holder 是否仍然存在；
5. 删除测试 Holder。

根据结果将远程主机标记为：

```text
native-detach
user-supervisor
non-persistent
```

### 已确认行为

* `native-detach`：使用普通轻量 Holder；
* `user-supervisor`：只使用主机上已经可用、无需安装和配置的 transient user service，并且必须用同样的 detach probe 验证；
* `non-persistent`：允许创建会话，但明确提示 SSH 断开后会话可能退出。

自动引导不得安装或启用 supervisor，不得写持久 user unit/LaunchAgent，不得调用 `sudo` 或要求开启 linger。找不到可用 user supervisor 时直接标记 `non-persistent`；不存在 `tmux` 回退。常驻安装的 `diri-node` 只能作为用户明确配置的增强模式存在，不属于默认 SSH 模式。

---

## 输入控制权

多个客户端同时发送：

* 输入；
* resize；
* Ctrl+C；
* 权限确认；

可能破坏交互式 TUI。

第一阶段固定为：

```text
一个 live attach
+
一个 Active Controller Lease
```

新 attach 在 Holder 内原子递增 Control Epoch、撤销并断开旧 attach，然后取得控制权。只有当前 Controller 可以发送：

* Input；
* Resize；
* Signal；
* Kill。

协议保留以下消息：

```text
AcquireControl
ControlGranted
ControlRevoked
ReleaseControl
```

通过单调递增的 Control Epoch，避免已经失效的旧 SSH 连接在重新连接后继续写入。

多个只读 observer 属于后续可加能力，不进入第一阶段，不得为它在 Holder 热路径中预先引入 fan-out、共享锁或无界队列。

---

## Agent 启动环境

Remote Helper 不能假设 `claude`、`codex` 等 Agent 一定存在于非交互 SSH 默认 `$PATH` 中。

远程 Agent 可能依赖：

* `nvm`；
* `mise`；
* Homebrew；
* `~/.local/bin`；
* Login Shell 初始化脚本。

这里应采用 Zed remote server 的核心思路：Helper 使用远程用户数据库中的 login shell，而不是相信启动它的 SSH Channel 恰好带有正确的 `$SHELL`；随后在远程用户的 Home 中启动一次有超时的 login + interactive shell，捕获完整 environment。这样可以覆盖在 shell 初始化中配置的 Homebrew、`nvm`、`mise`、`asdf` 和 proxy 变量。

环境捕获不能把 shell stdout 当作干净协议。用户的 rc 文件可能输出 greeting、控制序列或报错。`diri-remote probe` 必须启动捕获子进程，并通过专用继承 fd 输出 JSON 或 NUL-delimited environment；普通 stdout/stderr 只作为有界诊断信息。需要同时限制：

* 捕获超时；
* 单个变量和总 environment 大小；
* 变量名格式；
* stdout/stderr 诊断长度；
* shell 初始化失败时的明确 warning。

环境解析固定分为两层：

1. **Account environment**：在 `$HOME` 中捕获登录环境，作为该次 Helper/Holder 启动的基础；
2. **Working-directory environment**：以 account environment 为输入，在目标 `cwd` 中解析 `mise`、`asdf`、`direnv` 等目录相关调整。

第二层会执行用户和项目配置，应采用与“在该目录打开一个交互式 shell”相同的信任模型，并带独立超时。第一阶段必须实现两层捕获；account environment 可以按 host/user/login-shell 指纹缓存，working-directory environment 可以按 canonical cwd 缓存，但 attach 不得重新执行 shell 初始化。捕获失败必须返回明确错误或 warning，不能静默退化为 SSH 非交互环境。

不得从本地 Diri 进程批量复制 environment 到远端。特别是本地 API key、Keychain 派生值和本地 socket path 不得跨主机传播。以下 SSH 临时变量默认也不应进入可 detached 的 Agent：

```text
SSH_CONNECTION
SSH_CLIENT
SSH_TTY
SSH_AUTH_SOCK
```

`SHLVL`、`PWD`、`OLDPWD` 和 `_` 应由最终进程环境重新建立，而不是从捕获结果继承。

最终环境优先级固定为：

```text
remote account/cwd environment
< Agent Manifest defaults
< host/session 显式配置
< Diri 保留的 per-session protocol variables
```

`DIRI_*`/`DIRIJOR_*` 保留变量不能被 host 配置覆盖。`TERM`、`COLORTERM`、locale 和颜色能力应由 Diri 按实际 terminal protocol 明确设置，而不是继承某个短生命周期 SSH PTY 的值。

Diri 最终必须使用结构化的：

* `argv`；
* `cwd`；
* environment；

启动 Agent。Helper 应先在远端验证并 canonicalize `cwd`，再由 PTY child 直接调用 `execve`/等价 API。

不应把用户参数拼接成一条 Shell 字符串。

SSH 命令行只应调用固定的 Helper 路径。Session ID、工作目录、参数和环境变量均通过 stdin 上的结构化协议传输。

Bootstrap shell 和 Agent shell 必须严格区分：前者只允许少量固定、内部生成的安装命令；后者不参与 argv 拼接。即使用户 prompt、路径或 Agent 参数包含引号、换行或 shell metacharacter，也不能改变远程命令结构。

---

## Rust 代码边界

### Tailscale、iPhone Companion 与 Remote Holder 边界

第一阶段远程执行链路固定为本地 Engine 主动发起的 OpenSSH 连接。Tailscale
可以为 OpenSSH 提供私网 IP 或 MagicDNS 名称，但只是用户网络环境的一部分；Diri
不检测、不配置也不强制依赖 Tailscale。

`diri-node` 仍是可选增强模式，其 TCP endpoint 可以绑定到已经存在的私网或
Tailscale 地址，但不得参与默认 Remote Holder 的安装、启动、重连或持久化。

iPhone Companion 属于手机主动连接本地 Engine 的入站控制面，不属于 Remote
Holder。Rust 第一阶段不提供该 listener，因此不得保留仅写入 `remote.json`、生成
配对链接或把配置文件存在误报为 Ready 的 UI。遗留 `remote_active` 与
`session.set_owner` 也不构成可用实现，应删除。未来若重新引入 Companion，必须使用
独立、版本化且经过鉴权的 Rust 协议，并与 Holder controller lease 明确仲裁；不能
复用或模糊 `remote_pty` 的 SSH 执行语义。

初始化阶段固定建立以下 Rust-owned seam：

```text
diri-proto
  └─ remote PTY wire types + existing terminal Frames

diri-engine
  └─ host catalog + bootstrap + SSH transport + local session orchestration

diri-remote
  └─ probe/install self-check + environment capture + Holder + attach Bridge

shared terminal-state crate
  └─ PTY bytes → Grid/Cursor/Modes/Snapshot/Diff
```

具体代码落点固定为：

* 将当前 `diri-engine/src/remote.rs` 拆为 `remote/mod.rs`，并删除所有 `ssh -t`、`tmux new-session -A`、tmux session name、tmux kill/cleanup 与相关测试；
* `remote/bootstrap.rs` 只负责平台探测、artifact 选择、安装和 capability cache；
* `remote/ssh.rs` 只负责 OpenSSH argv、ControlMaster、认证提示和 Channel 生命周期；
* `remote/transport.rs` 把本地 Engine 的 session 操作映射为 Helper protocol，不直接操作 UI；
* 新增 workspace member `crates/diri-remote`，产出同名单文件二进制；
* 本地控制协议的 `Hello` 必须带有 Rust Engine identity；桌面端和 client 对缺失或错误 identity fail closed，不能把旧 daemon 当作 remote transport；
* macOS 应用包同时携带通用架构的 Rust `diri-ssh-askpass`，由 OpenSSH 通过 `SSH_ASKPASS_REQUIRE=force` 调起原生认证/host-key UI，协议 stdin 不承担认证交互；
* 在 `diri-proto` 中使用独立的 `remote_pty` module；第一阶段不存在 companion-access `remote.rs`；
* 从 `diri-engine::screen` 抽取最小 `diri-terminal-state` library，作为本地 Engine 与 `diri-remote` 唯一的 parser/Grid/Snapshot/Diff 实现；`diri-remote` 不依赖整个 `diri-engine`；
* `diri-app` 和 `diri-client` 不直接执行 SSH，它们继续只与本地 Engine 通信。

`diri-remote` 不得依赖 GPUI、`diri-app`、`diri-client` 或 `diri-node`。第一阶段也不应把 manifest、status reducer、worktree、browser、usage、migration 和 node management 链接进远程二进制。

每个 Session 固定对应一个独立 Holder 进程、一个 Unix Socket 和一个 session state 目录。第一阶段不实现管理多个 Holder 的 Diri Supervisor；进程级隔离可限制单个 parser、PTY 或 Agent 故障的影响范围。Session 目录内的原子 lock/incarnation 负责阻止重复 Holder，外部已存在的 user supervisor 也只能启动单个 Session Holder。Holder 为自己的 Agent 进程组创建一个只观察 Holder-liveness 的最小 process guard；Holder 正常退出时先解除 guard，Holder 崩溃或被强杀时 guard 只清理该 Session 的 Agent 进程组。它不持有 PTY、UDS、session state 或任何多 Session 编排，因此不构成 Supervisor。

### 热路径与性能门槛

PTY read、terminal parse、diff build 与 attach write 必须采用单 owner/event loop、批量读取、复用 buffer 和有界 channel；禁止在 PTY 热路径上使用跨任务 `Arc<Mutex<Terminal>>`，禁止让 socket backpressure 传播到 PTY reader。活动输出的 diff 合并窗口上限为 16 ms；无 attach 时仍持续解析 terminal state，但不构造或序列化无人消费的 diff。

第一阶段至少建立以下可重复的本机 Helper/UDS benchmark，并作为回归门槛：

* `HelloAck` 到 `FullSnapshot` 的 p90 不高于 100 ms；
* Input frame 到 PTY write 的 p95 不高于 10 ms；
* PTY output 到可发送 diff 的 p90 不高于 50 ms；
* 本机 loopback 端到端交互延迟 median 不高于 75 ms、p90 不高于 150 ms；
* Holder 在空闲且无 attach 时不得轮询 PTY/Socket，不运行 heartbeat 或自动 GC，只由 PTY、Socket 和 child-process 事件唤醒。

SSH 握手和真实网络 RTT 分别记录，不混入 Helper 本机热路径指标。任何放宽门槛的变更必须提供基准证据并同步修改本文档。

---

## 最小协议扩展

应尽量复用已有终端 Frame，只增加必要的控制消息：

```text
Hello
HelloAck
FullSnapshot
ProcessExit
Signal
AcquireControl
ControlGranted
ControlRevoked
ReleaseControl
Error
```

`Hello` 至少应携带：

```text
protocol version
local build id
session id
requested role
client nonce
last acknowledged output/grid sequence
```

`HelloAck` 至少应返回：

```text
protocol version
holder build id
session incarnation
capabilities
current controller epoch
process state
snapshot/grid sequence
```

所有消息必须有长度上限。未知可选字段可以忽略；未知必需 capability、protocol major 不匹配或 session incarnation 不匹配必须 fail closed，并返回结构化 `Error`，不能退回到把 stdin 当作终端字节流。

第一阶段不应同时实现：

* MCP 转发；
* Artifact 检测；
* Port 检测；
* Usage 收集；
* 跨节点 Handoff；
* Checkpoint 迁移；
* Remote Resource Governor。

这些能力可以在 PTY 替换稳定后独立增加。

---

## 实施阶段

### 第零阶段：Rust-only seam 与自动引导

先建立 Rust seam 并删除旧 transport：

* 建立 `diri-remote` crate 和三个受支持平台的 release artifact；
* 定义 `remote_pty` protocol、Build ID 和 capability negotiation；
* 实现 OpenSSH ControlMaster、平台 probe、原子安装和 Helper self-check；
* 实现 login/cwd environment capture；
* 删除 Rust Engine 中当前 `ssh -t` + `tmux` transport、迁移清理逻辑、配置入口和测试，不保留 fallback；
* 为 bootstrap 使用 fake SSH executable/fixture host，测试认证输出、shell 噪声、并发安装、损坏上传和版本并存；
* 打包脚本将受支持平台的 Helper 放入 Diri 应用资源，不依赖 Swift packaging products。

在新 Helper 尚未完成验收前，remote spawn/resume 明确返回 `remote_transport_unavailable`；不得静默回到 tmux。新 Helper 必须独立完成：

```text
probe → install/verify → launch test holder → attach → detach → inspect → kill → gc
```

### 第一阶段：严格替代 `tmux`

只实现：

* 自动上传 Helper；
* 远程 PTY；
* Agent 进程生命周期；
* Input；
* Resize；
* Signal；
* Process Exit；
* Terminal Grid；
* Full Snapshot；
* Incremental Update；
* Reconnect；
* Persistence Probe；
* 单一 Active Controller。

验收场景：

```text
1. 在远端启动一个交互式 Agent。
2. 断开网络数分钟。
3. 重新连接。
4. 看到与断线前一致的终端画面。
5. 继续操作同一个 Agent 进程。
```

第一阶段还必须覆盖以下失败场景：

* SSH 在 upload、launch 和 attach 各阶段断开；
* 两个客户端并发 bootstrap 同一 host；
* 本地 Engine 与远端 Helper protocol/build 不匹配；
* attach 发送速度不足时触发 diff 丢弃和 Full Snapshot 重置；
* 新 attach 使旧 controller 失效后，旧连接继续发送 input/resize；
* Agent 正常退出、被 signal 终止以及 Holder 异常退出；
* shell rc 输出噪声、超时或返回非零；
* remote home、cache 或 state 目录不可写；
* persistence probe 返回三种能力等级，且整个流程没有权限提升或主机配置变更。

### 第二阶段：结构化 Agent 事件

增加：

* Claude Hooks；
* Codex Notify；
* 小型离线事件缓冲区；
* 远程 Agent Conversation/Thread ID；
* 与本地 Status Reducer 集成。

### 第三阶段：用户显式配置的增强远程模式

增加可选支持：

* 已由用户配置的 systemd user service；
* 已由用户配置的 launchd agent；
* `diri-node`；
* 远程机器重启后的恢复；
* 资源信息；
* MCP Bridge；
* Artifact 和 Port 检测。

---

## 预期收益

### 用户体验

* 不再需要手工安装 `tmux`；
* 远程配置仍然只需要 SSH；
* 更适合极简远程环境；
* Helper 自动上传和版本管理。

### 架构

* 本地和远程采用统一 Holder 模型；
* Diri 直接拥有远程 PTY 生命周期；
* 远程运行事实与本地业务状态之间边界明确；
* 不再依赖 tmux Session Name 和 Shell 命令拼接。

### 终端正确性

* 减少 PTY 和终端模拟层数；
* 不再受 tmux `$TERM` 行为影响；
* 直接同步终端 Grid；
* 明确定义重新连接语义。

### 后续扩展能力

该架构将更容易支持：

* 远程 Hooks；
* 结构化 Agent 状态；
* 远程进程树控制；
* 资源监控；
* 多客户端观察；
* 远程会话迁移；
* 与 `diri-node` 更深度集成。

---

## 风险与权衡

### Diri 需要承担更多底层职责

去掉 `tmux` 后，Diri 必须自行维护：

* PTY 持久化；
* 终端状态；
* 重连；
* 输出缓冲；
* 输入控制权。

这比调用成熟的通用终端复用器承担更大的维护责任。

### 平台支持

Diri 需要为不同平台和架构提供远程 Helper。

第一阶段固定支持：

```text
Linux x86_64
Linux aarch64
macOS arm64
```

macOS x86_64 不属于 Remote Helper 支持矩阵，不构建、不打包、也不通过 Rosetta 或 CI 模拟测试；探测到 Intel macOS 时必须明确返回 unsupported-platform。Release pipeline 必须分别构建、strip、记录 SHA-256，并验证每个受支持 artifact 的 `probe` 输出。Linux artifact 应在低版本/极简发行版容器中执行兼容性测试；macOS artifact 应验证最低支持版本。任何一个受支持 target 缺失都应在 host probe 阶段返回明确的 unsupported-platform，而不是回退到上传本机架构二进制。

### 自动安装的安全边界

自动上传可执行文件扩大了 supply-chain 和路径覆盖风险。Diri 必须只安装本地发行包 manifest 中列出的 artifact，校验长度、SHA-256、Build ID 和 protocol，使用 owner-only 路径与权限，并记录可诊断但不包含 secret 的安装事件。第一阶段不接受远端自行选择 URL 或执行服务端返回的任意安装命令。

### 不同远程环境的持久性差异

部分服务器不允许登录会话中的 detached process 在退出后继续运行。

Diri 必须探测并明确展示这一限制，不能静默假设已经获得可靠持久性。

### 远程机器重启

默认自动引导模式无法让正在运行的进程跨远程机器重启继续存活。

重启恢复应依赖：

* Agent 自身的 resume；
* Diri checkpoint；
* 可选持久服务。

---

## 非目标

本提案第一阶段不试图：

* 替代 SSH 传输层；
* 建立完整的远程 Diri daemon；
* 与 Swift daemon、Swift Holder 或 Swift wire/on-disk format 保持兼容；
* 从 Swift 代码补齐或推断 remote 行为；
* 保证跨远程机器重启的进程存活；
* 实现通用终端复用器；
* 强制依赖 `diri-node`；
* 将完整状态引擎移动到远程机器。

---

## 已确认的重构边界

以下决策不再处于待讨论状态：

* 抽取最小共享 `diri-terminal-state` crate，不让 `diri-remote` 依赖整个 Engine；
* 每个 Session 一个独立 Holder，不实现 Diri 多 Session Supervisor；
* native detach 失败后只探测无需配置的现有 user supervisor，否则标记 non-persistent；
* `FullSnapshot` 只含可见终端状态，4 MiB scrollback 通过 `Scroll` 按需访问；
* 第一阶段只有一个 live attach/controller，多 observer 延后；
* Swift 不在实现、兼容或验收范围；
* 原 Rust SSH + `tmux` transport 直接删除，永不作为回退。

Remote Holder 只有在以下能力全部通过自动测试、三个受支持平台的 artifact 验证和真实 SSH soak 后才能合入可发布分支并成为唯一 remote transport：

* 创建远程会话；
* 网络断开；
* 终端完整恢复；
* 继续输入；
* Helper 升级兼容；
* 持久性能力探测；
* 权限边界审计；
* 本文档定义的性能门槛。

整个方案应坚持一个核心原则：

> 远端只保留无法放在本地的状态：PTY、Agent 进程和当前终端画面。会话编排和业务逻辑继续保留在本地 Diri Engine。

---

## 实施状态

截至 2026-08-08，Phase 0 与第一阶段的 Rust 实现已完成，新的 Remote Holder 是唯一 remote session transport：

* Rust Engine 中原有的 SSH PTY + `tmux` transport、tmux session name、清理路径与回退均已删除；未打包有效 Helper catalog 的构建仍以 `remote_transport_unavailable` fail closed；
* `diri-proto::remote_pty` 已实现版本、capability 协商、结构化 launch/environment、认证 token、snapshot/delta、scrollback、进程退出、signal 与 controller epoch；所有 frame 有硬上限，token 的 Debug 输出被遮蔽且 drop 时清零；
* snapshot/delta 在编码和解码前都会校验终端维度、总 cell 数、cursor、row index 与每行精确宽度，恶意或损坏的超大 Grid 无法先触发无界分配；
* `diri-pty` 与 `diri-terminal-state` 是 Engine/Helper 的唯一共享 PTY 和终端状态实现；Remote Holder 不链接 Engine、GPUI、app、client 或 node；
* `diri-remote` 已实现 `probe/launch/attach/inspect/list/kill/environment/persistence/gc`，每 Session 一个 Holder、一个 owner-only UDS、一个 PTY，并保存 32 MiB 有界 raw output 与 4 MiB 按需 scrollback；独立 process guard 保证 Holder 异常死亡不会遗留 Agent 进程树；
* Holder 热路径由单 owner event loop 完成 PTY drain、parse、16 ms diff coalesce 与 attach write；无 attach 时不构造 diff，慢 attach 无法阻塞 PTY，过期增量会通过断开/reconnect 和 `FullSnapshot` 重新播种；
* 每次新 attach 会原子递增 controller epoch、显式撤销旧 controller；Input、Resize、Signal 与 Scroll 均校验当前 epoch；
* 本地 Engine 已接通 remote spawn、断线重连、daemon 重启 adoption、远程 Agent resume、终端 Grid、raw output offset、scrollback、exit/Holder failure 归因及 owner-only bearer binding；
* New Agent 目录选择通过 Engine 的统一 `host.list_directories` RPC 完成；远程侧复用精确版本 Helper 的只读 `directories` 命令，每次只列一层，最多返回 512 个目录并限制总扫描量，不把 SSH 或远程路径处理下放到桌面 App；Helper 返回的 canonical path 是后续导航的权威路径，host `defaultCwd` 只作为首次打开的 fallback，不能覆盖用户已经选择的绝对子目录；
* Project 身份由执行位置与目录共同决定，Engine 保证每个 Session 都归属一个一级 Project；相同路径位于不同 SSH host 时不会合并，项目级新增操作继承该 Project 的 host；
* Inspector 的 working-tree 请求按 Session 的执行位置路由：本地目录由 Engine 本地读取，远程目录通过固定的无 PTY SSH 脚本读取，cwd/comparison 只走 stdin；login-shell 噪声由响应 marker 隔离，非 Git 目录和远端未安装 Git 被桌面端显示为普通兼容状态而不是持续故障；
* 第一方 Claude/Codex manifest 直接 exec Agent，不再在 Agent 退出后回落到登录 Shell；桌面端观察到正常退出、signal 或外部退出后会立即 detach Terminal 并删除对应 Agent 行，`daemon-restart` 仍保留自动恢复语义；
* Claude 启动时只对精确识别出的 workspace trust 选择器自动确认用户刚选择的目录；不会使用 `--dangerously-skip-permissions` 或任何同时绕过工具权限、审批和 sandbox 的参数；
* 本地控制面提供 `host.initialize`，桌面端新增 SSH 主机后会立即显示初始化状态，并由 Engine 完成 Helper 安装/验证、登录环境捕获与 persistence probe；结果只返回 Build ID、协议、cwd、shell 和持久性等级，不向 UI 暴露完整环境或认证数据；远程准备中的 UI 使用共享、可降低动态效果的活动指示器，重装成功仅短暂确认，失败状态则持续保留以支持重试；
* SSH Host 编辑界面提供显式远程环境重装；`host.initialize` 的 additive `forceReinstall` 路径强制执行已验证的 staging/upload/activation，且不覆盖或终止旧 Session Helper；所有新的无状态远程动作都会先 probe 当前应用 Build ID，应用更新后的首次动作自动安装新版本；
* 桌面端 bundled/default daemon 解析已切换到 `dirijord-rs`；Engine `Hello` 使用显式 Rust identity，app/client 会拒绝旧或未知 daemon，Remote Holder 不存在旧 transport fallback；Rust-owned Agent manifests 随 Engine 打包，remote 启动不读取 Swift resource 或 Swift Holder；
* `Hello.executableHash` 使用 Engine 启动时缓存的 SHA-256；App 启动时只对确认身份但与 bundle 哈希不一致的 Rust Engine 执行平滑升级，持久化 Engine 状态并保留 Holder/Agent，使更新后的首次远程动作必然读取新 Helper catalog；
* bootstrap 已实现带噪声平台 probe、三个 target 精确选择、并发幂等上传、nonce 临时文件、长度/SHA-256/Build ID/protocol 三次验证、no-replace 激活和版本共存；Intel macOS 明确返回 unsupported-platform；GC 不删除任何 live Session 引用的 Build ID，已有 Session attach/inspect 还会再次拒绝与当前 Helper Build ID 不一致的 live Holder；
* protocol 1.2 将 terminal、Session management、environment capture、directory list、persistence probe 与 atomic activation 作为 Helper 必需能力统一声明；Engine 在任何管理 RPC 前完成能力门禁并自动同步缺失能力的旧版本，loose Cargo Engine 优先选择同目录当前 Helper，避免旧生成 catalog 造成命令面倒退；
* 所有内部远程命令固定通过 `exec /bin/sh -c <quoted-script>` 执行，避免 fish 等 account shell 误解析 POSIX bootstrap；过长的 OpenSSH `ControlPath` 会映射到经过 owner/type/symlink 校验的短 `/tmp` owner namespace，避免 macOS Unix socket 长度上限；
* OpenSSH 保持 `-T` 二进制通道，使用有限连接/存活超时；Rust `diri-ssh-askpass` 提供原生安全输入与 host-key 确认，认证响应不会进入协议 stdin 或日志；
* 远端环境通过用户数据库解析 account login shell，再分 account login 与目标 cwd 两层捕获，走独立 fd，不信任 SSH 继承的 `$SHELL` 且不解析 stdout；启动参数始终以结构化 `argv/cwd/environment` 进入 PTY child，local socket、凭据与 `DIRI_`/`SSH_` 会被剔除；
* persistence probe 使用两个独立 SSH channel 验证 `native-detach`；失败后只尝试现成的 transient `systemd --user`/`launchctl submit`，成功报告 `user-supervisor`，否则报告 `non-persistent`。整个流程不安装软件、不写持久 service、不调用 sudo、不修改 PAM/sshd/linger；
* `non-persistent` 会在桌面通知和 Session 侧栏以持续 `No detach` 状态明确展示，不会把可能随 SSH 退出的会话伪装成可靠持久化；
* `scripts/build-remote-helpers.sh` 生成 Linux x86_64/aarch64 musl 与 macOS arm64 三份版本化 artifact 和 manifest；packaging/CI 强制三个 target 完整、逐 artifact hash/length 校验并签名 macOS Helper，同时将 Rust Engine、local Holder、AskPass 和 manifests 作为 remote 所需的 Rust-owned 资源打包；
* CI 在 Linux x86_64、Linux arm64 与 macOS arm64 原生 runner 上分别构建并执行精确 artifact `probe`，并在 disposable ordinary-user OpenSSH endpoint 上运行 detach/reconnect soak；这些是发布分支的强制门禁，不依赖开发者个人 SSH 主机。
* `scripts/remote-acceptance.sh` 执行 release UDS 性能门槛、23 MiB slow-attach 恢复和 transient user supervisor Holder 验收。2026-08-08 本机 release 样本为 snapshot p90 1 µs、input-to-PTY p95 429 µs、output-to-diff p90 16.276 ms、loopback median 145 µs / p90 427 µs，均保留较大门槛余量；测试在每次 CI 中打印实测值。普通测试另覆盖环境噪声/超时、认证诊断、protocol/incarnation mismatch、并发及中断 bootstrap、upload/launch/attach 三阶段 SSH 断链、symlink cache 拒绝、controller revocation、正常/signal/Holder 异常退出、同 PID detach/adopt、owner-only 权限、完整 `list/kill/gc` 生命周期与三种 persistence 结果。

当前不进入第一阶段的内容保持不变：remote hooks/Codex notify、MCP forwarding、artifact/port/usage、handoff/checkpoint migration、跨主机重启恢复和多 observer 属于后续独立阶段，不得借本次 PTY transport 重构隐式加入 Helper。

本地交付验证已经构建并执行全部三个精确 target；macOS arm64 在宿主原生执行 `probe`，两个 Linux musl 静态产物分别在 arm64/x86_64 Alpine 容器执行 `probe`。不需要 Rosetta，也不存在 macOS x86_64 Remote Helper artifact。真实 OpenSSH soak 已在一次性普通用户、临时 Home、非 root sshd 上通过，验证了 bootstrap、fish login shell、断线、同 PID/同 incarnation 重连、snapshot、继续输入与清理；CI 另有相同的 disposable endpoint 门禁。

发布候选仍必须让三个受支持 target 的原生 CI 与真实 SSH soak 全部为绿色；本机/fake-SSH 测试不冒充 PAM 或 logind 策略的证据。需要手工复现其他主机策略时使用一次性普通用户账号，不需要 sudo，也不修改主机配置：

```bash
DIRI_REMOTE_SSH_TARGET=user@disposable-host \
DIRI_REMOTE_SOAK_SECONDS=180 \
scripts/remote-ssh-soak.sh
```

可选设置 `DIRI_REMOTE_HELPER_PATH`（必须是远端 target 可执行的精确 Helper）、`DIRI_REMOTE_SSH_EXECUTABLE` 与 `DIRI_REMOTE_CWD`。测试打印唯一 Session ID，并在成功、断言失败和 panic unwind 时调用 authenticated `kill` 清理 Holder；若测试进程被不可恢复地强杀，可使用该 ID 在远端执行对应 Helper 的 `inspect`/`kill`，之后运行 `gc`。测试覆盖真实 OpenSSH bootstrap、环境捕获、persistence probe、Bridge 断开、同 PID/同 incarnation 重连、终端快照恢复、继续输入及退出清理。
