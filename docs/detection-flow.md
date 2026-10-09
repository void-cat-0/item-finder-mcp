# 检测、观测与图像证据的数据流

本文按源码中的实际路径解释数据含义。检测框、历史观测、原始图像、烧框图片和调用方标注是不同的数据，不能互相替代，也不构成实时在场保证。

相关入口：[开始使用](getting-started.md) · [运行与维护](operations.md) · [MCP 使用](mcp-usage.md) · [已知限制](known-limitations.md)。VLM grounding 配置见 [VLM sidecar](vlm-sidecar.md)。

## 1. 从原始 frame 到 Observation

RTSP 与本地摄像头共用 `CameraRunner` / supervisor，区别主要在 `FrameSource` 和构建 feature：RTSP 使用 `rtsp`，本地摄像头使用 `camera`。

```text
FrameSource 返回 RGB frame + camera_id / captured_at / 宽高
  ├─ daemon 启用原始资产服务时：更新该 camera 的有界最新帧缓存
  └─ detect_fps 节流
       → Detector 返回 label / confidence / 像素 bbox
       → 可选 relative-depth provider，估计该图中检测框的相对深度
       → 裁剪框到图像范围，丢弃非有限或零面积框
       → 同 label NMS（IoU > 0.45 的低优先级框被抑制）
       → 框中心点映射到 regions，未命中则 zone="frame"
       → 合并或新建 Observation，同时维护几何样本及可用深度
       → 按模式保存烧框图；daemon 可为新 Observation 发布 raw JPEG
       → runner 更新 lifecycle tracker，保存事件及事件几何证据
```

- `detect_fps` 是检测节流，不是录制帧率或实际吞吐保证。未送检测的帧不产生检测观测；daemon 缓存可在检测节流前更新，但不是全帧录像。
- 每个 runner 同步进行取帧、推理和持久化。慢推理或 HTTP 请求在途时，该 runner 不继续消费新帧，帧源缓冲可能导致延迟。
- 默认 YOLO/null 为 1.0 次/秒，VLM 为 0.2 次/秒；真实频率还受推理和 I/O 耗时限制。
- 检测失败时跳过该帧，由 supervisor 退避重试，延迟从 2 秒指数增长到最多 60 秒；不把失败伪装成“图中没有物品”。数据库等不可恢复错误则可能停止该 camera。

## 2. Detector、NMS 与 regions

| 后端 | 输入与输出 | 使用边界 |
|---|---|---|
| YOLO（`yolo` feature） | 本地 ONNX 推理，默认 COCO 80 类、`--conf` 默认 0.3 | 词表受模型限制，分数不是物品身份 |
| VLM（`vlm` feature） | JPEG 发给 OpenAI-compatible grounding endpoint，回复转换为像素框 | 开放词表；`--targets` 是提示词约束，不是结果的硬过滤 |
| NullDetector | 返回空检测集合 | 无真实检测后端时用于管线；空集合不证明真实场景为空 |

NMS 按分数从高到低，只抑制**同 label**且 IoU 超阈值的框。它既不是实体跟踪，也不会将不同 label 的框合并。

regions 是按 camera 配置、写入数据库的像素矩形。zone 使用框中心点单点判定，取第一个包含它的 region；无命中返回 `frame`。重叠区域不提供多 zone 归属，改变图像分辨率后应检查 region 坐标。区域归属也不等于三维容纳关系。

## 3. Observation 的去重与计数

`Observation` 是持久化的历史目击记录，主要字段为 `camera_id`、`zone`、`label`、`first_seen`、`last_seen`、`hit_count` 与可选 `sample_snapshot`。

- 去重键是 `(camera_id, zone, label)`，默认窗口 300 秒。新目击与已有记录的时间跨度在窗口内时合并，否则新建记录。
- 对正常按时间到达的目击，可理解为距上次目击不超过 5 分钟继续合并。乱序输入按时间跨度判断；`first_seen` 取更早时间，`last_seen` 取更晚时间。
- 每个 NMS 存活框形成一次 sighting，合并或插入都会计入返回的 `recorded` 数量；它不是“新增行数”。`hit_count` 也不是严格的帧数。
- 一帧中两个不重叠的同类物品，若 camera/zone/label 相同，可能合并到同一 Observation 并增加两次 hits。不能从 hits 或 observation_count 推算物品实例数量。
- 跨 zone 会形成不同记录；停止检测只会停止更新，不会即时删除历史记录。超过窗口再次命中才会新开记录。

例如：09:00 首次见到 `cup@desk` 新建一行；09:01 再次命中更新同一行；09:10 才再次命中则通常新建一行。这个例子不提供同一只杯子的跨帧身份保证。

## 4. Geometry、depth 与 lifecycle

### Geometry：框会持久化，但不是所有逐帧检测的日志

`observation_geometry` 保存有界 `first` / `last` 样本，包括采集时间、bbox、图像宽高、zone、source、confidence 和可选 `snapshot_ref`。更早输入可更新 `first`，更晚输入可更新 `last`；事件另行冻结几何副本，后续更新 `last` 不改变事件证据。

因此不能再说“框不落库”，也不能说“每一帧的框都永久保留”。几何样本、图像文件和 raw asset 是独立对象；有 bbox 或 `snapshot_ref` 不保证图片仍存在，也不保证引用对应 MCP raw asset。

### Depth：独立可选证据，不是米制距离

`relative-depth` 是独立 feature，运行时 `depth_provider` 默认 `none`，可选择 `relative-vlm`。它可与 YOLO 或 VLM grounding 配合，不会因为启用 `vlm` 自动启用。

provider 给出 `nearer` / `middle` / `farther` / `unknown`、可选 0–1 序数分值及质量，持久化时关联对应几何样本，并保存 provider/model/frame/response/prompt 等来源信息。这里的分值不是概率或米数；没有标定、metric depth、三维坐标或深度图。流式估计失败保留检测结果，只缺少深度证据。

### Lifecycle：检测时间线，不是物品消失的确定解释

runner 的 tracker 按 camera/zone/label 维护当前跟踪记录。首次跟踪或 Observation ID 变化生成 `appeared`；成功检测步骤后，未命中时间超过 `max(30 秒, 3 × 检测间隔)` 可生成 `disappeared`，reason 为 `missed_gap`。

- 消失事件的 `occurred_at` 是最后命中时间，`noticed_at` 是后来发现间隔已超过阈值的时间。
- 正常结束会关闭 tracker，reason 为 `shutdown`；记录切换可能为 `row_changed`。不能将这些原因都解释为“物品被移走”。
- 数据库事件包含 session/source/reason，并冻结关联几何；JSONL 是另一个运维时间线输出。写入证据失败会告警，不能假设时间线永远完整。
- 事件间隔与 Observation 的 300 秒去重窗口不同：同一 Observation 可以在不同 tracker 生命周期中再次出现，`appeared` 不一定表示新建数据库行。

## 5. 各运行模式的实际差异

| 路径 | 检测与持久化行为 |
|---|---|
| `--detect <JPEG/PNG>` | 读单图、运行 detector、NMS、打印结果；可用 `--out` 写烧框副本、`--depth-out` 写相对深度 JSON。不打开观测数据库，不创建 Observation 或 lifecycle |
| `--rtsp` / `--webcam` | 共用 runner/supervisor，写 Observation、几何与事件；受 `--frames` 限制。此 CLI 路径不配置 daemon raw asset registry |
| `--daemon --config ...` | 常驻管理配置中的 enabled cameras，提供资产、最新帧缓存及维护能力；不是用 CLI `--frames` 结束的单路任务 |
| Frigate webhook | 上游已完成检测，本项目不再次运行本地 Detector/NMS；按同样时间窗口去重，有 bbox 时保存几何，记录 `observed` 事件。缺 bbox/宽高时证据能力受限，不推断本地 `disappeared` |
| `--preview` | RTSP 预览组件另取流，提供 `/preview`、`/preview.jpg`、`/preview.mjpg`；不执行检测。但宿主 ingest 仍会打开数据库、可初始化 regions 和提供 webhook，不能称整个 preview 进程“完全不碰库” |

### 烧框快照模式：以实际写入为准

| `--snapshots` | 当前本地检测路径的行为 |
|---|---|
| `off` | 不写烧框快照；不关闭 Observation/geometry，daemon raw 发布也不受此项关闭 |
| `first`（默认） | 新 Observation 写 `{id}.jpg`，并回填 `sample_snapshot`；以后命中不刷新这张代表图 |
| `events` | 当前实现同样在新 Observation 写 `{id}.jpg`，但不回填 `sample_snapshot`；runner 保存事件元数据/冻结几何，不另外写事件 JPEG |
| `detections` | 每个存活 sighting 写 `{id}-g{geometry_id}.jpg`；不回填 `sample_snapshot`，也不自动建立几何到该文件的引用 |

虽然配置注释将 `events` / `detections` 描述为逐级增加事件图，当前实现不能据此承诺事件图片或可查询的全帧档案。`last` 的 geometry ID 可复用，`detections` 文件名也可能重复覆盖；这些文件不等于完整录像。

烧框图包含本帧存活检测框、区域线和逐 sighting 高亮。同帧新建多个 Observation 时，可以生成底图相同而高亮不同的 JPEG。

## 6. Raw、annotated 与 caller annotation

| 数据 | 内容与寿命 | 不代表什么 |
|---|---|---|
| Observation raw asset | daemon 在新建 Observation 时，从未画框 RGB 编码 JPEG；独立资产身份/哈希，受 Observation 和 retention 生命周期约束 | 不一定是最新命中图片；不是 `sample_snapshot` |
| frozen camera frame | 从有界最新帧缓存冻结的不可变未画框 JPEG，携带 camera/capture/expiry；读取同一 asset 不刷新其采集时间或 TTL | 不创建 Observation，历史冻结图不保证当前在场 |
| annotated snapshot | 检测框和区域线烧入像素的展示图片；是否有图、如何命名由快照模式决定 | 不可作为 MCP raw 图片的回退来源 |
| caller annotation | host 中调用方模型对特定 raw 图片提交的结构化标注，有 producer、版本和独立保留证据 | 不是 daemon detector 的目击，不自动加入已有寻物 label 查询 |

caller annotation 默认禁止写入，启用流程见 [MCP 使用](mcp-usage.md)。一次标注可含 0–128 个物体，使用单图内 `object_id`，修订不会提供跨帧/跨 camera 身份。保留的 annotation evidence 与原 frame asset TTL 独立，不会续期原资产；raw 图片也不会因此被烧框修改。

### 坐标不能混用

- 检测与持久化 geometry：像素 `xyxy = [xmin, ymin, xmax, ymax]`，原点为图像左上。
- VLM sidecar 的 `bbox_2d`：按选定 `norm1000` 或 `pixel` 解释后转换为像素 xyxy，详见 sidecar 文档。
- caller annotation：`coordinate_system="normalized_xywh"`，`bbox=[x, y, width, height]`，在 0–1 范围内；宽高必须为正，且整个框在图内。
- 若从像素 xyxy 转换：`[xmin/W, ymin/H, (xmax-xmin)/W, (ymax-ymin)/H]`。不能直接复制 detector bbox，也不能将 0–1000 数值当作 0–1 标注坐标。

## 7. Web 和查询应如何解读

`item-web` 是只读查询/图片展示端。Observation 卡片展示 label、camera、zone、hits 和 first/last 时间；图片取自可解析且存在的 `sample_snapshot`。默认 `first` 图通常比 `last_seen` 旧，这不是实时画面。

空间/容纳候选的证据视图可展示几何、事件与深度来源；几何图片接口读取样本自己的 `snapshot_ref`，可能没有图片，不能据此回退到其他 frame。候选是解释性假设，不是直接目击或确定当前位置。

检索不到记录、没有图片、raw 已过期或缺少深度，分别表示不同的证据缺口。Web 刷新、卡片数量和资产列表都不证明“所有 frame 已保留”；同类物品合并、标注与现有寻物查询的边界见 [已知限制](known-limitations.md)。
