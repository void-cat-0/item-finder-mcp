# Item Finders MCP

基于摄像头和历史证据的物品记忆系统。采集端将“某个标签在某台摄像头的某个区域被看到过”保存为 SQLite 记录；MCP、命令行和 Web 界面读取同一数据集，帮助回答“最后在哪里看到它”，并按需提供原图、几何证据和保守的遮挡/容纳候选。

**这是历史目击与证据查询，不是实时定位或物品追踪系统。** 标签可能误检；同类物品可能合并成一条记录；“可能在盒子里”不表示物理容纳已被证明。

## 从哪里开始

| 你的目标 | 入口 |
|---|---|
| 已有采集数据库，接入 ZCode 或其他 MCP host | [MCP 使用指南](docs/mcp-usage.md) |
| 从零安装、体验模拟记录、接 USB/RTSP/Frigate | [安装与快速开始](docs/getting-started.md) |
| 长期运行、多摄像头、保留策略与排障 | [常驻运行与运维](docs/operations.md) |
| 理解标签、区域、hit_count、图片和空间候选 | [采集流程与结果解释](docs/detection-flow.md) |
| 使用开放词汇 VLM 检测或可选相对深度 | [VLM sidecar 使用](docs/vlm-sidecar.md) |
| 配置 systemd、launchd 或 Windows 任务计划程序 | [服务部署模板](deploy/README.md) |
| 了解同类物品、标注检索和实例身份限制 | [已知限制](docs/known-limitations.md) |

使用说明均随仓库分发，不需要本机设计资料。`design/` 是被忽略的本地设计目录，不属于安装或运行前提。

## 工作方式

```text
USB / RTSP / Frigate
         │
         ▼
item-ingest：采集、检测、事实写入、图片/标注生命周期
         │
         ├── SQLite 数据集 ◄── item-query / item-web / item-mcp 只读查询
         │
         └── 认证 loopback API ◄── item-mcp 图片、状态、标注请求
                                            │
                                            ▼
                                    ZCode / 其他 MCP host
```

- **`item-ingest`** 独立运行，负责写入和清理；MCP 断开不停止摄像头采集。
- **`item-mcp`** 是 stdio 服务，由 host 启动。历史和几何查询直接只读 SQLite；图片、当前帧、状态、标注走 daemon 的认证本机 API。
- **`item-query`** 提供 `log`、`ask`、`candidates`；**`item-web`** 提供只读历史列表和证据卡片。
- **`item-core`** 保存共享事实模型与查询能力；**`xtask`** 管理 RTSP 的可选原生工具链。

MCP 不控制 daemon 或摄像头、不接受任意 SQL/路径，也不直接写数据库。`get_camera_frame` 会请求 daemon 冻结临时图片；显式开启的 `submit_image_annotation` 会保存调用方标注，二者不是普通只读操作。

## 最短体验：不需要摄像头

先安装 Rust stable。包含 MCP 的构建需要 Rust **1.89 或更新**；Windows 还需要 Visual Studio C++ Build Tools。默认构建不需要 FFmpeg、摄像头或模型。

在仓库根目录运行；下面命令适用于 Bash 与 PowerShell：

```sh
cargo +stable run -p item-ingest -- --demo --db target/quickstart/items.db --snapshots-dir target/quickstart/snapshots
cargo +stable run -p item-query -- --db target/quickstart/items.db log
cargo +stable run -p item-web -- --db target/quickstart/items.db
```

打开 [http://127.0.0.1:8478](http://127.0.0.1:8478)。`--demo` **会写入模拟数据**，所以示例使用独立的 `target/quickstart/`，不是生产数据集。这只能验证基本采集/查询链路，不证明真实检测、原图或空间候选已可用。

### 接入 MCP

```sh
cargo +stable build -p item-mcp
```

在 MCP host 中配置生成的 `target/debug/item-mcp`（Windows 为 `item-mcp.exe`）及同一数据库的**绝对路径**。ZCode 原生配置示例：

```json
{
  "mcp": {
    "servers": {
      "item-finders-mcp": {
        "type": "stdio",
        "command": "C:/projects/item-finders-mcp/target/debug/item-mcp.exe",
        "args": ["--stdio", "--db", "C:/projects/item-finders-mcp/target/quickstart/items.db"]
      }
    }
  }
}
```

替换示例路径，不覆盖 host 中的其他服务器配置。启动后先调用 `list_recorded_items`，再用实际 label 调用 `locate_item`。详细连接、认证及输入示例见 [MCP 使用指南](docs/mcp-usage.md)。

## 当前能力

- USB/内置摄像头、RTSP 和 Frigate webhook 采集；本地 YOLO 或可选 OpenAI-compatible VLM 检测。
- 常驻 daemon、多摄像头、健康快照、重连和保留策略。
- 历史标签目录、直接目击查询、有限 geometry/events/depth 事实及可审计空间候选。
- 未标注 raw JPEG 读取、当前缓存帧冻结及按 asset ID 复读。
- 调用方单图多物体标注保存、整批修订、幂等重试、独立证据保留；写入默认关闭。
- MCP 默认 **13 个工具**；明确开启标注提交后 **14 个**。工具是否注册不代表相关 daemon、摄像头或证据可用。

## 先了解这些限制

1. `list_recorded_items` 是标签聚合；`observation_count` 不是物品数量，`hit_count` 不是实例数量。
2. 直接 Observation、调用方标注和派生空间候选是不同来源。保存标注不会自动让新标签出现在 `locate_item` 中。
3. 框是图像平面坐标；单目相对深度不是米制距离。规则分数是排序 heuristic，不是概率。
4. 图片可能不存在或已过期。当前帧读取、标注证据复读不会续期原 frame asset。
5. 没有跨帧/跨摄像头实体重识别、持久化推断、MCP resources/prompts 或摄像头管理工具。

完整说明见 [已知限制](docs/known-limitations.md)。部署时让 daemon、MCP 和其他查询端使用同一数据集；密钥和带凭据的摄像头 URL 不应提交到仓库。

## 构建与维护

只查询数据库或使用 MCP，无需启用摄像头 feature。实际采集按需选择 `camera`、`rtsp`、`yolo`、`vlm`、`relative-depth`，见 [feature 与模型准备](docs/getting-started.md)。不要把未启用检测后端的采集启动成功当作已经识别出物品。

开发检查：

```sh
cargo +stable fmt --all -- --check
cargo +stable check --workspace
cargo +stable test --workspace
cargo +stable clippy --workspace --all-targets -- -D warnings
```

`cargo test` 不保证普通可执行文件已刷新；修改后部署前应显式 `cargo build`。Windows 上不要覆盖运行中被锁定的 exe，可用独立构建目录再安排升级。`cargo clean` 会删除 `target/` 内的 RTSP 工具链缓存。离线 maintenance 会删除数据并 VACUUM，务必先阅读 [运维指南](docs/operations.md)，不要在 daemon 活跃时执行。
