# 常驻运行与运维

本指南说明现有实现的操作方式，不依赖本机配置或额外设计资料。先读 [安装与快速开始](getting-started.md)；MCP 接入见 [MCP 使用指南](mcp-usage.md)，服务注册见 [部署模板](../deploy/README.md)。

## 1. 部署前准备

- `item-ingest --daemon --config <文件>` 才是常驻模式；普通 webhook、单路 CLI 采集和 `--demo` 不是同一个生命周期。
- daemon 是写入端；`item-mcp`、`item-query`、`item-web` 读取同一数据库。MCP host 的连接/断开不启动或停止 daemon。
- 按需构建：USB 要 `camera`，RTSP 要 `rtsp`，真实 YOLO 要 `yolo`，VLM 要 `vlm`，可选相对深度要 `relative-depth`。Cargo features 是**编译选项，不是 TOML 字段或运行时开关**。
- 模型不随仓库提供。YOLO 的路径来自 `--model`，默认 `models/yolov8n.onnx`；不要在 `[daemon]` 或 `[[camera]]` 中写 `model` 期待生效。`depth_model` 则是相对深度服务的模型名称。
- 使用专用数据目录，不与私人图片混用。服务账户需读取配置、模型和摄像头，写入数据库目录（含 WAL/SHM/lock）、快照/raw 资产、health 和事件目录。
- 下面路径、设备索引、域名仅为示例；替换后才运行。保存配置及密钥时限制文件权限，不提交带凭据的 URL。

## 2. 最小配置与多摄像头

### USB / 内置摄像头

将下面内容保存为部署目录中的 `config.toml`：

```toml
[daemon]
db = "data/items.db"
snapshots_dir = "data/snapshots"
health_file = "data/health.json"

[webhook]
enabled = false

[[camera]]
id = "desk-usb"
webcam = 0
detector = "yolo"
```

准备好 `camera,yolo` 构建与模型后，在该部署目录前台启动：

```sh
./item-ingest --daemon --config config.toml --model models/yolov8n.onnx
```

PowerShell 使用 `./item-ingest.exe`。摄像头索引依平台/设备枚举变化，`0` 不是所有机器的 USB 摄像头。daemon 的摄像头来自 TOML；不要靠 CLI `--webcam`、`--rtsp` 或 `--frames` 配置/结束 daemon。

### 两路 RTSP

```toml
[daemon]
db = "data/items.db"
snapshots_dir = "data/snapshots"
health_file = "data/health.json"
events_file = "data/events.jsonl"
retention_days = 90

[webhook]
enabled = false

[[camera]]
id = "entrance"
url = "rtsp://camera-a.example.invalid:554/stream"
detector = "yolo"
detect_fps = 1.0

[camera.regions]
door = [0.0, 0.0, 640.0, 720.0]

[[camera]]
id = "shelf"
url = "rtsp://camera-b.example.invalid:554/stream"
detector = "yolo"
detect_fps = 0.5

[camera.regions]
shelf = [100.0, 100.0, 1000.0, 700.0]
```

使用 `rtsp,yolo` 构建和同样的 daemon 启动命令。示例 `.invalid` 域名不会指向真实摄像头；填入自己的源。区域矩形是该摄像头图像的像素 `[x0, y0, x1, y1]`，不是比例或世界坐标。重叠区域按名称字母顺序优先匹配。

- 每台 camera 使用稳定、唯一的 `id`；不能是空字符串、`.`、`..`，不能含控制字符，最长 1024 UTF-8 字节。更换 ID 不会搬迁旧目击记录。
- `enabled` 默认 true；false 不启动该 camera，也不删除已有区域/历史。
- 同时设置非空 `url` 和 `webcam` 时 **url 优先**；空 url 才可能回退到 webcam。
- 两者都没有的 camera 是 **webhook-only**，不会生成采集线程，也不会自动打开设备。

### 无源 / Frigate webhook-only

```toml
[daemon]
db = "data/items.db"

[webhook]
enabled = true
listen = "127.0.0.1:8477"

[[camera]]
id = "frigate-room"

[camera.regions]
desk = [0.0, 0.0, 640.0, 480.0]
```

默认构建即可运行该 daemon；将 Frigate 的事件发送到 `POST /frigate/webhook`，事件 camera 名称应与配置 ID 一致。配置 camera 本身不会产生事件。该 webhook 没有 internal token 认证；需跨机接收时另行限制网络来源/配置认证代理，不能把读 token 当 webhook 保护。无源 camera 在文件 health 中可能一直是 `starting`、没有 frame 统计，这不等于本地设备启动失败。

### 路径规则

daemon 和 maintenance 将 `db`、`snapshots_dir`、`health_file`、非空 `events_file`、YOLO `--model`（包括默认值）相对路径解析到 **config 文件所在目录**，不是服务的 WorkingDirectory。CLI 覆盖的相对路径也一样。maintenance 没有 `--config` 时才以 cwd 为基准。

例如 `/etc/where-i-put/config.toml` 的 `db = "data/items.db"` 写入 `/etc/where-i-put/data/items.db`。配置目录应只读时，为输出字段配置独立的绝对路径。Windows TOML 用正斜杠路径（如 `C:/where-i-put/data/items.db`）或单引号 literal string，避免反斜杠转义。服务命令中的 exe/config 路径也应绝对化。

## 3. 配置优先级与真实默认值

支持同一字段时，总体顺序由低到高是：**内置默认 → TOML → 环境 → 显式 CLI**。并非每个字段都支持四层，也不能把任意字段名变成环境变量。

| 范围 | 实际入口与注意事项 |
|---|---|
| 数据库、快照目录/模式、webhook listen | `[daemon]`/`[webhook]`，对应 CLI `--db`、`--snapshots-dir`、`--snapshots`、`--listen` 可覆盖 |
| health、事件、保留/清理周期、internal、标注配额 | `[daemon]`；不要编造对应 CLI 或环境变量 |
| 读 token | `ITEM_DAEMON_TOKEN` 高于 TOML `internal_token`，优先用受保护的环境注入 |
| 标注 token | 仅 `ITEM_DAEMON_ANNOTATION_TOKEN`；不能写入 TOML |
| VLM endpoint/model | `--vlm-base-url`/`--vlm-model` 高于 `ITEM_VLM_BASE_URL`/`ITEM_VLM_MODEL`；不是 camera TOML 字段 |
| YOLO 参数 | `--model`、`--input-size`、`--conf`；模型路径不来自 TOML |
| 相对深度 | 显式 `--depth-*` 高于逐 camera 字段；endpoint/model 的 `ITEM_DEPTH_BASE_URL`/`ITEM_DEPTH_MODEL` 高于文件，未设时回退 `ITEM_VLM_*` |
| 日志 | `RUST_LOG`（默认 info），不是 daemon TOML 字段 |

**daemon 逐 camera 的例外**：`detector`、`detect_fps`、`targets` 在每台 camera 中设置时优先于合并后的全局检测设置；全局 CLI 只作这些字段的回退。不要假定 `--detect-fps` 会强制覆盖所有 camera。单路非 daemon CLI 的匹配 camera 设置则可由显式 CLI 覆盖。深度字段按上表的显式 CLI/环境优先级处理。

| 项目 | 默认与 0/空值语义 |
|---|---|
| 数据路径 | `data/items.db`、`data/snapshots`、`data/health.json`、`data/events.jsonl` |
| detector / 检测率 | yolo；本地默认 1.0 fps，vlm 默认 0.2 fps；只节流检测，不降低源解码/采集速率 |
| YOLO | 输入尺寸 640，置信度下限 0.3；需与 ONNX 导出尺寸匹配 |
| VLM / 深度 | VLM timeout 60 秒，坐标 norm1000；深度默认 none，timeout 30 秒 |
| snapshots | first；可选 off / first / events / detections。events目前仅写首图且不回填引用，detections可能增加/覆盖后缀文件；详见[实际快照行为](detection-flow.md) |
| events_file | 默认开启；空字符串关闭事件 JSONL，不是日志轮转 |
| webhook / preview | webhook 默认开启，listen `127.0.0.1:8477`；preview 默认关闭，非空 preview_url 需 rtsp feature |
| checkpoint_secs | 300；0 关闭周期 checkpoint（不取消退出/离线维护的 checkpoint） |
| retention_days | 90，按 Observation 的 last_seen；0 不按年龄删除目击 |
| sweep_secs | 3600；0 不自动 prune；开启时启动后即做一次 sweep |
| reconcile_secs | 604800（7 天）；0 不做周期 orphan 扫描 |
| rescan_config_secs | 0；当前没有运行时重载实现，即使改成非零也不要期待热更新；编辑配置后重启 |
| internal_listen | 未设置，私有 API 默认关闭；internal_max_asset_bytes 默认 8388608（8 MiB） |
| frame cache / 冻结资产 | frame_cache_ttl_ms 5000（范围 1–5000）；frame_asset_ttl_secs 600（范围 1–86400） |
| annotations_enabled | false；标注保留默认 30 天（范围 1–365） |
| 标注配额 | 10000 sets、50000 revisions、metadata 536870912 字节、evidence 2147483648 字节；都必须正数 |

没有 `yolo` feature 的 YOLO 路径会退化为 NullDetector，不产生真实检测；没有 `vlm` feature 的 VLM 路径报错。daemon 若需该构建不支持的 camera/rtsp 源会拒绝启动；已编译 YOLO 但缺模型时 camera 进入 `failed`，不是无限重试。

## 4. internal API 与标注的双重授权

在 `[daemon]` 增加 `internal_listen = "127.0.0.1:8480"`，并在**启动 daemon 的环境**中注入随机、高熵 `ITEM_DAEMON_TOKEN`。仅设置 token 不会打开 listener。token 必须由可见 ASCII 组成，不能包含空格/换行。

- listener 强制 loopback，不能绑定 `0.0.0.0` 或 LAN 地址；MCP endpoint 必须是 HTTP **数字 loopback origin**，例如 `http://127.0.0.1:8480`，不接受 `localhost`、附加路径、查询或 URL 用户密码。
- 每个 internal 请求校验 Bearer token 与实际监听地址的 Host/Origin；不开放跨域访问。webhook/preview 在另一 listener 上，不共享这套认证。
- loopback 不是用户隔离边界。保护 token、配置和宿主环境，不把 internal 转发为公网接口，不把 token 放到命令行历史或截图。
- MCP 设置 `--daemon-endpoint` 或 `ITEM_DAEMON_ENDPOINT`，读 token 用 `ITEM_DAEMON_TOKEN`；MCP 的显式 CLI 高于对应环境值。DB 路径用 `--db` 或 `ITEM_DB`，默认仍是 `data/items.db`，不自动继承 daemon TOML。
- 原图、帧冻结与健康工具依赖 daemon；SQLite 历史查询可以不连 daemon。冻结是临时写入，不创建 Observation，也不证明当前仍有该物品。复读不会续期资产。

标注提交必须**两端都开**：

1. daemon `[daemon] annotations_enabled = true`，启用 internal listener 和读 token；另注入 `ITEM_DAEMON_ANNOTATION_TOKEN`。
2. MCP 启动时加 `--enable-annotations`，提供 endpoint、读 token 以及同一个独立标注 token。

两个 token 必须不同；写 token 不替代读 token。只在 host 显示工具不代表 daemon 已允许写入。标注读取不要求开启提交；保存标注也不会自动变成直接目击或出现在 `locate_item` 标签目录中。标注 schema 初始化与清理仍可能在写入关闭时执行，关闭标注不是“数据库绝对不变”。完整请求语义见 [MCP 使用指南](mcp-usage.md)。

## 5. health、锁与异常恢复

### 监控哪些事实

文件 health 初次启动写一次，通常每 15 秒刷新，退出时最后写一次；日志每 300 秒输出 camera heartbeat。检查 schema（当前 1）、`updated_at` 与 `seq` 的推进，以及逐 camera 的 `state`、`frames`、`last_frame_age_s`、`last_error`、`reconnects`、`inference_ms_ewma`。

- `updated_at` 超过 90 秒是健康信号过期：可能进程已死、卡住，或写文件失败。**不是确认进程死亡的证明**。
- 新鲜 health 也不证明所有摄像头工作；健康线程可继续更新，而某台 camera 已 failed 或没有新帧。`pid` 仅供调查参考，不能仅凭 PID/文件存在判断活性。
- 干净退出后仍保留最终 health，短时间内会看似“新鲜”；结合服务/进程状态、camera stopped 与退出日志判断。
- 写入使用临时文件与 rename；Windows 共享冲突时重试，必要时删除目标再 rename，可能短暂缺失或过期。读取失败先重试，不把它当作可以删除锁的依据。
- MCP `get_health` 是已认证的 daemon 状态读取；`server_info` 只验证 MCP 连接/模式，不等于 daemon 健康。

### 锁的边界

daemon 在打开 DB 前获取数据库同目录的 `ingest.lock`（同目录不同 DB 也会争用这个名称）。这是原子创建的**协作锁文件**，不是 OS 进程锁或 SQLite 对所有写入者的强制锁；不会约束绕过协议的程序。

运行时每 15 秒刷新锁 mtime。daemon 启动允许恢复 mtime 至少 45 秒没更新的锁，但若可读 health 的 `updated_at` 仍小于 90 秒，则拒绝接管；health 缺失/不可读不会否决接管。正常退出释放锁，硬终止可能遗留锁，接管会记录 `previous run died without shutdown`。

该判断是时间戳启发式，不做实际进程死亡检查。磁盘故障、挂起或时钟变化可能影响判断；异常后先检查服务管理器、相关进程及日志，禁用自动启动/重启，确认旧写入者已退出，再恢复。不要用删锁、并行启动或循环硬杀来“解决”冲突，也不要许诺硬停后等一分钟必然恢复（health 的 90 秒窗口可能仍阻止它）。

## 6. 保留、备份与离线维护

自动 retention 删除超过 last_seen 窗口的 Observation 及其关联事实/图片生命周期信息；缩短窗口会不可逆丢失历史证据。`retention_days = 0` 只关闭目击年龄删除，不关闭 orphan、frame TTL 或独立标注保留。

- prune 删除配置目录里的代表快照 `<id>.jpg`，并清理过期 raw；不会自动删除所有对应的 events/detections 后缀快照。
- reconcile 的候选扫描可识别数字 ID 的 `.jpg` / 后缀快照，并只考虑已无活跃 Observation、ID 不大于当前最大活跃 ID 的文件；没有活跃 Observation 时跳过这部分扫描。但当前删除路径仍按 `<id>.jpg` 组装，**不能保证删除扫描到的后缀文件**。它不是“删除目录内所有无用文件”，也不追扫旧 snapshots_dir；需要监测遗留文件增长。
- raw、冻结帧和标注证据有独立生命周期；冻结帧清理通常每秒，标注清理启动时及每 60 秒尝试一次。关闭标注写入不暂停历史标注清理。
- 不承诺整个磁盘有统一硬限额：事件 JSONL 在64MiB附近尝试轮转并保留两代，轮转失败时会告警并继续追加；服务日志和遗留后缀快照也可能增长。自行安排日志/事件归档与磁盘监测，停止对应写入者后再搬迁文件。

**先停止写入、确认退出、备份，再维护。** 备份 config（密钥另行保护）、模型版本信息、同一时点的 DB 与全部快照/raw/标注证据。不要在线只复制主 `.db` 而漏掉 WAL；可离线备份，或使用 SQLite 一致性备份机制。恢复时不要把旧 DB 单独覆盖到仍运行的新数据集上。

```sh
./item-ingest --maintenance --config /absolute/deployment/config.toml
```

此命令**不是 dry-run**：按 retention prune、reconcile orphan、checkpoint WAL（TRUNCATE）、VACUUM，然后退出。使用与 daemon 相同的配置和显式 DB/目录覆盖；停止查询端可减少 reader 对 checkpoint/VACUUM 的阻塞。WAL checkpoint 失败会报告，但此前删除可能已经完成；VACUUM 失败也不是自动回滚整个维护流程。

maintenance 使用严格锁：**任何已有锁都拒绝，包括过期锁**，不会像 daemon 自动接管。正常停止后锁应移除；若是异常残留，先确认无写入者且重启已禁用，再审慎处理该残留锁，并重新运行普通 maintenance。

`--maintenance --force` 存在，但**绝不用于日常维护或自动重试**：它跳过获取锁；即使 health 新鲜也只是警告，仍会继续删除/VACUUM。health 缺失/过期不能授权 force。它仅是已经独立确认进程退出后的特殊恢复逃生口，不是安全检查、清理锁或停止服务的命令。

## 7. 构建缓存与下载验证

常规默认构建没有原生依赖，也不自带摄像头/检测能力。RTSP 工具链准备与状态检查（在仓库根目录执行）：

```sh
cargo +stable run -p xtask -- status
cargo +stable run -p xtask -- setup
```

- setup 下载到 `target/vendor/`，安装记录包含 URL、sha256、platform、flavor（预构建/source）；manifest 不匹配/缺失或关键文件缺失视为 stale。不要手动把 FFmpeg 换成 8.x；目前兼容线是 7.1。
- 默认下载 4 workers、8 MiB chunk；`--download-workers` 范围1–32，`--download-chunk-mib` 范围1–128。符合身份/校验条件的分块可恢复；Range/响应不符合预期时可退回全量下载，不能保证并行或公网速度。

```sh
cargo +stable run -p xtask -- setup --download-workers 8 --download-chunk-mib 8
cargo +stable run -p xtask -- setup --from-source
```

中断后先重跑相同setup命令以复用有效缓存，不需要先clean或force。当前没有 `--offline`、`--archive` 或手动归档导入参数。
- “verified” 要区分：Windows FFmpeg、Windows libclang wheel、FFmpeg 源码包有代码内预置 SHA-256；Linux 预构建 FFmpeg 目前未预置 hash，首次下载计算并记录，只能确认后续一致性，不能替代独立可信来源认证。
- 下载 receipt 验证缓存长度/hash；安装 status 则检查 manifest/marker，**不是每次重算全部已解压文件 hash**，也不是软件签名或漏洞审计。不要为绕过 hash mismatch 修改 pin；调查来源/缓存后再重试。
- `setup --from-source` 使用固定源码包并要求 sh/perl/make/nasm，Windows 还需 MSVC 开发环境；缺工具的 preflight 失败不是下载错误。macOS 没有该预构建 FFmpeg：请准备与本项目兼容的 FFmpeg **7.1** 和 libclang；不要直接采用包管理器当前默认的 FFmpeg 8（rust-ffmpeg 9.x 不接受 FFmpeg 8）。
- `.cargo/config.toml` 提供默认工具链路径；既有环境值可覆盖，排障时检查冲突。Windows RTSP 构建尝试将 DLL 放在 exe 旁，部署时带上匹配 DLL；不能只复制 exe。Unix 还需保留可用的动态库加载路径。
- **默认 `cargo clean` 删除构建产物及 target 内工具链/下载缓存**，不是数据维护命令。不要用它处理运行故障；生产数据不能放在 target 下。自定义 target-dir/CARGO_TARGET_DIR 时也要核对构建输出、vendor 与配置引用的实际位置。
- `setup --force` 是重新获取/安装工具链，与 maintenance force 完全不同；不建议常规使用，也不能在 DLL 正被加载时覆盖它。

## 8. 进程控制与安全升级

`item-ingest`、`item-web` 是独立进程；`item-mcp` 是 host 管理的 stdio 子进程。当前没有通用 `--stop` 或 MCP 摄像头/daemon 停止工具。

- 前台 daemon：Unix Ctrl-C/SIGINT、SIGTERM、SIGHUP 都是**停止**，SIGHUP 不是重载；Windows 支持实际送达的 Ctrl-C/Ctrl-Break/console-close/logoff/shutdown 事件。
- 停止时先结束接收并置停止标志，camera 线程有 10 秒 drain 预算，随后写最终 health、尝试 checkpoint、释放锁。预算不是所有 HTTP 请求/DB IO 的全局退出保证，OS 强制终止时也不能承诺 flush 完成。
- 服务管理器的启动/停止和权限见 [部署模板](../deploy/README.md)。Windows `Stop-ScheduledTask` 是硬停；`-Unregister` 删除注册，不是优雅停机协议。
- Web 默认 `127.0.0.1:8478`；用独立 `--db`/`--listen` 指定，只读不等于可公开暴露私人图像。前台进程从所属终端结束；无专用 stop 参数。
- MCP 由 host 禁用/断开服务器以结束，更新后重连；不影响 daemon。host 若注入新 token，必须重启相应子进程以刷新环境。

升级顺序：

1. 记录当前 exe、features、模型、配置、服务 action、环境和数据路径，保留旧版本及恢复用备份。
2. 在**独立构建/发布目录**生成新版本；显式 build，不能靠 cargo test 更新普通 exe。选择与设备匹配的 features，核对模型及运行库。
3. 在隔离数据目录验证新版本，不使用生产 DB/设备做无授权测试；安排维护窗口。
4. 停止并禁用自动重启/触发，确认目标进程及其 DLL 已释放。Windows **绝不覆盖运行中的 exe/DLL**；改服务/任务指向版本化的新目录，或停机后替换整套产物。
5. 核对模型 CLI 参数、config-relative 路径、读/标注 token 和权限，启动唯一写入者；再重启读取端/MCP host 连接。
6. 检查服务日志、health/seq、逐 camera 新帧与真实检测、同库查询/图片，再恢复自启动。二进制回滚不保证兼容已迁移数据；必要时离线恢复成套备份，不能悄悄覆盖新数据。

## 9. 常见排障

| 症状 | 先检查 |
|---|---|
| `unexpected argument` / source unsupported | 执行的 exe 是否为所需 feature 的显式 build；daemon 源来自 TOML，不是单路 CLI 参数 |
| running 但没目击 | 是否 NullDetector；模型与阈值、目标词汇、真实帧、检测率；running 不是正确识别的证明 |
| camera failed / model load | `--model` 按 config 目录解析，实际文件可读；修正后重启，fatal 不会自动重新加载模型 |
| reconnects 持续增长 | 源地址、网络、设备占用、摄像头权限；指数退避 2 秒起、上限 60 秒，收到真实帧后重置 |
| 改 TOML 无效果 | 当前没有热重载；重启；逐 camera 检测字段可能覆盖全局回退 |
| 找不到数据 / UI 空白 | 三个读取端是否同一绝对 DB 路径；Web 在 DB 尚不存在时可用空内存 UI，不代表已读取生产数据 |
| internal 启动失败/401/403 | loopback 数字地址、端口占用、daemon/MCP 环境 token 是否一致、Host/Origin；不是拿标注 token 替代读 token |
| 标注工具缺失/提交拒绝 | MCP `--enable-annotations` 与 daemon annotations_enabled 双开；独立写 token 和读 token、资产到期、修订冲突/配额 |
| 原图不可用 | webhook 可能无本地 raw；snapshot 模式、历史资产/引用、TTL、同库同 camera；不能拿标注快照冒充 raw |
| 锁冲突/health 过期 | 调查服务与进程及磁盘写入，禁用重复触发；不要据此直接删锁或 force |
| 维护后磁盘没完全下降 | WAL reader、VACUUM、reconcile 安全边界、独立 raw/标注、事件/日志；保留删除不可逆 |
| Windows exe/DLL 被锁 | 停止确切进程/自动重启，使用新版本目录；不通过强删文件或 cargo clean 绕过 |

部署验收应分开记录：源码/文档核对、隔离测试、实际服务安装、登录/重启行为和真实设备采集。仅注册成功、进程存活或单元测试通过，不能代替后几项验证。
