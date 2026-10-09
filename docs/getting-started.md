# 安装与快速开始

本指南先使用独立模拟数据验证查询链路，再按需接 USB、RTSP 或 Frigate。长期采集用 [常驻模式](operations.md)，MCP host 接入用 [MCP 指南](mcp-usage.md)。

## 1. 环境与构建

需要 Rust stable 与 Cargo。包含 MCP 的工作区使用 Rust **1.89 或更新**。Windows 需要 Visual Studio C++ Build Tools/MSVC；其他平台需要 C/C++ 构建工具。摄像头权限和可选推理/视频后端依赖按平台另行准备。

在仓库根目录执行（Bash、PowerShell 均可）：

```sh
rustup toolchain install stable
cargo +stable check --workspace
cargo +stable build -p item-mcp -p item-query -p item-web
```

默认构建不启用原生摄像头、YOLO、VLM 或 RTSP 后端，也不下载模型。程序位于 `target/debug/`，Windows 加 `.exe`。Rust 版本满足最低要求不等于所有 feature 已在该版本、该平台实测。

### 按任务选择 feature

下列 feature 属于 `item-ingest`，按需组合；MCP 本身不需要这些 feature。

| 任务 | feature | 额外准备 |
|---|---|---|
| 模拟数据、Frigate webhook | 无 | 默认编译即可 |
| USB/内置摄像头采集 | `camera` | OS 摄像头权限、正确设备索引 |
| 本地 YOLO 检测 | `yolo` | 兼容 ONNX 模型 |
| RTSP、MJPEG preview | `rtsp` | FFmpeg 7.1 与 libclang |
| 开放词汇 VLM 检测 | `vlm` | 外部 OpenAI-compatible 多模态服务 |
| 可选 ordinal 相对深度 | `relative-depth` | 另行配置相对深度服务 |

例如 USB＋YOLO：

```sh
cargo +stable build -p item-ingest --features camera,yolo
```

RTSP＋YOLO：

```sh
cargo +stable run -p xtask -- setup
cargo +stable build -p item-ingest --features rtsp,yolo
```

FFmpeg 下载、缓存、平台差异见 [运维指南](operations.md)。`cargo clean` 会删除工具链缓存。使用自定义构建目录时，别假设 native vendor 路径也会自动匹配。

## 2. 不接摄像头的最短体验

这里将数据写到独立 `target/quickstart/`，不触碰你已有的数据集。`--demo` 会创建/迁移该 SQLite 库、写入模拟区域和 `keys` Observation，不是只读命令。

```sh
cargo +stable run -p item-ingest -- --demo --db target/quickstart/items.db --snapshots-dir target/quickstart/snapshots
cargo +stable run -p item-query -- --db target/quickstart/items.db log
cargo +stable run -p item-query -- --db target/quickstart/items.db ask "where are my keys?" --label keys
```

预期：demo 打印 `demo observation`；`log` 能看到 `demo-cam`、`desk`、`keys`。重复运行可能合并最近的模拟目击。

模拟帧是空白帧，demo 不生成可用于证明真实识别的 JPEG、depth 或完整 disappearance 场景。MCP 图片和空间候选为空/不可用并不表示基本查询失败。

### Web 查看同一数据库

```sh
cargo +stable run -p item-web -- --db target/quickstart/items.db
```

打开 [http://127.0.0.1:8478](http://127.0.0.1:8478)。列表显示历史 Observation；ask 栏先给直接最后目击，再显示可用证据卡片。不配置语言模型也可读取记录。

- `--listen` 默认 `127.0.0.1:8478`；保持 loopback，避免无认证开放。
- `--db`、`--priors` 是显式 CLI 参数；不要把 MCP 的 `ITEM_DB`/`ITEM_PRIORS` 优先级套用到 query/web。
- 历史库中只有 `frigate://` 引用或已清理图片时，卡片可能无图片。
- 对旧的相对 snapshot 引用，从与采集端一致的工作目录启动 Web；新 daemon 配置路径会固定到配置文件目录。

## 3. 准备 YOLO 模型

仓库不分发模型。将兼容的 YOLO ONNX 检测模型放在 `models/`，或用 `--model` 指定绝对路径。

默认路径是 `models/yolov8n.onnx`，默认 `--input-size 640`、`--conf 0.3`。推荐先用常规 Ultralytics YOLOv8 检测模型验证；模型需采用此后端支持的输入/输出及预处理格式，不能假设任意 YOLO 导出或自定义预处理节点可用。导出时让图像输入尺寸与 `--input-size` 一致（通常 640），opset 不高于17。

可按 [Ultralytics 官方导出说明](https://docs.ultralytics.com/modes/export/) 在独立模型准备环境安装工具、从可信来源取得权重并导出。例如已准备好 `ultralytics` 与 `yolov8n.pt` 后，在 Python 中执行：

```python
from ultralytics import YOLO
YOLO("yolov8n.pt").export(format="onnx", imgsz=640, opset=17, dynamic=False)
```

将生成的ONNX复制到上述目录。外部工具可能联网获取权重/依赖，按其许可与安全说明操作；本项目不会自动准备YOLO模型，本文也不承诺所有外部工具版本导出的图都兼容。

单图试验（不写 Observation）：

```sh
cargo +stable run -p item-ingest --features yolo -- --detect path/to/photo.jpg --detector yolo --model models/yolov8n.onnx --out target/detected.png
```

预期：stdout 打印检测框和耗时；`--out` 保存带框副本。这不等于采集数据库已经建立或 `locate_item` 已能查到图片中的物体。

YOLO 类别受模型词表限制，常见 COCO 模型并不包含钥匙等所有家居物品。开放词汇需求见 [VLM sidecar](vlm-sidecar.md)。

## 4. USB/内置摄像头前台采集

先确认设备索引和权限。索引在 Windows、Linux、macOS 上不通用，不能保证 `0` 就是你想用的摄像头。

```sh
cargo +stable run -p item-ingest --features camera,yolo -- --webcam 0 --camera-id desk --detector yolo --model models/yolov8n.onnx --db target/quickstart/items.db --snapshots-dir target/quickstart/snapshots --detect-fps 1 --frames 120
```

这里 `--frames 120` 限制**读取的帧数**，不是检测数或120秒；`0` 表示持续运行。默认只对部分帧检测（YOLO默认1次/秒），因此有采集帧不保证有 Observation。

仅启用 `camera` 而没有有效 detector 可能只是空检测链路。显式使用匹配 feature 的 `--detector yolo` 或 `vlm`，并查看启动警告和最终记录。

前台单源模式不使用 daemon 单实例锁。**不要让它与 daemon 或另一个采集程序共写同一个生产数据集。** 前台模式一般按启动 cwd 解析数据/模型路径；长期运行请改用 [daemon TOML](operations.md)。

## 5. RTSP 前台采集

安装本项目支持的原生依赖后，用你有权限访问的摄像头地址。不要把带密码的地址写进共享终端记录或仓库。

```sh
cargo +stable run -p xtask -- setup
cargo +stable run -p item-ingest --features rtsp,yolo -- --rtsp "rtsp://CAMERA_HOST:554/STREAM_PATH" --camera-id desk --detector yolo --model models/yolov8n.onnx --db target/quickstart/items.db --snapshots-dir target/quickstart/snapshots --detect-fps 1 --frames 120
```

替换 `CAMERA_HOST` 和 `STREAM_PATH`。地址只是占位符，不会自动发现相机。RTSP 需启用 `rtsp`；真实 YOLO 检测还需 `yolo`。启动失败先区分网络/解码错误与模型错误，不要因为没记录就删除数据库。

### 可选 preview

```sh
cargo +stable run -p item-ingest --features rtsp -- --preview "rtsp://CAMERA_HOST:554/STREAM_PATH" --listen 127.0.0.1:8477 --db target/preview/items.db
```

打开 [http://127.0.0.1:8477/preview](http://127.0.0.1:8477/preview)。这是合并在 Frigate webhook HTTP 服务中的 MJPEG bridge，不等于 detector 采集闭环；该模式会打开/创建数据库并可接受 webhook 写入。preview 路由没有 MCP 内部 API 的 bearer 认证，保持 loopback 或置于受控网络/代理后。

## 6. Frigate webhook

默认模式启动 HTTP receiver，不运行本地模型：

```sh
cargo +stable run -p item-ingest -- --listen 127.0.0.1:8477 --db target/frigate/items.db
```

项目接收 `POST /frigate/webhook` 的简化事件 JSON；Frigate 的 MQTT 消息需由你配置的转发器映射，不是自动订阅原始 MQTT。事件示例：

```json
{
  "camera": "desk",
  "label": "cup",
  "bbox": [120, 80, 260, 180],
  "top_score": 0.8,
  "frame_time": "2026-10-11T08:00:00Z"
}
```

`bbox` 是源帧 pixel XYXY。有效 label 写入历史记录；未给 bbox 默认归到 `frame` 区域。`frame_time` 缺失/解析失败使用接收时刻。可选 snapshot ID 是 Frigate 引用，不会自动拉取远端原图。

同一 camera ID 要选择一种事实来源，不要让 Frigate 和本地采集同时报告同一台相机。离散 webhook 事件不能替代连续帧 disappearance，不能保证生成与本地采集相同的空间候选。配置区域和常驻 webhook 见 [运维指南](operations.md)。

## 7. 查询和空间候选

所有查询端指向同一实际 SQLite 文件：

```sh
cargo +stable run -p item-query -- --db target/quickstart/items.db log cup --limit 20
cargo +stable run -p item-query -- --db target/quickstart/items.db ask "where is the cup?" --label cup
cargo +stable run -p item-query -- --db target/quickstart/items.db candidates keys --camera desk --window-secs 120 --limit 200 --json
```

- `log [label]`：最近记录，可选字面标签子串；不是实例台账。
- `ask <question> --label <label>`：显式 label 优先。未提供时使用英文 keyword heuristic，不是中文翻译/语义搜索。
- `candidates <label>`：已有生命周期证据上的候选，不制造新目击，不持久化推断。CLI 的 `--limit` 是事件读取预算，不是 MCP `limit` 的候选条数；CLI/Web 与新 MCP facade 的有效性门槛不完全相同。

可选配置尺寸/容器先验：

```sh
cargo +stable run -p item-query -- --db target/quickstart/items.db --priors crates/item-query/priors.example.toml candidates keys --camera desk --json
cargo +stable run -p item-web -- --db target/quickstart/items.db --priors crates/item-query/priors.example.toml
```

[样例文件](../crates/item-query/priors.example.toml) 中的 camera/zone/label 和尺寸只是示例，必须按实际数据和可解释来源调整；先验不等于测量事实。无先验文件不会将候选提升为 `possibly_contained_in`。缺事件、框或足够 evidence 时允许空结果；重叠和高分也不能证明物理 inside。

## 8. 常见入门问题

| 现象 | 先检查 |
|---|---|
| MCP 提示 database_unavailable | DB是否存在、绝对路径、文件权限、是否指向错误 cwd |
| 摄像头启动但无记录 | 是否有匹配 detector feature、模型、词表和足够置信度 |
| Web 列表空 | 是否与采集端读取同一实际文件；不是只比较文件名 |
| raw 图片不可用 | 是否配置 daemon认证API；历史数据是否真的发布过raw |
| unknown depth/空候选 | 查看evidence/schema/truncation状态，不用空结果断言物品不在 |
| Windows exe 被锁 | 使用独立target目录构建，再安排host/service升级 |

下一步：[MCP host 接入](mcp-usage.md) 或 [长期采集与多摄像头](operations.md)。
