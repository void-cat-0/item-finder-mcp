# MCP 使用指南

本文面向在 ZCode 等 MCP host 中查询物品历史、读取原图及保存调用方标注的用户。
内容按当前源码核对；不代表你正在运行的 executable、daemon 或 host 已部署这些能力。
进一步阅读：[入门](getting-started.md)、[运维](operations.md)、[已知限制](known-limitations.md)、[检测流程](detection-flow.md)。

## 1. 先明确组件与权限

- `item-ingest` 写入 SQLite Observation、采集帧，并管理 raw 资产及标注生命周期。
- `item-mcp` 是独立 **stdio** server；历史查询逐次只读打开数据库，不创建或迁移数据库。
- MCP 不启动/停止 daemon 或 camera，也不加载检测模型；调用方 VLM 留在 host 内。
- 默认注册 **13 个工具**；显式启用标注提交后为 **14 个**。注册不等于依赖可用。
- `get_camera_frame` 会在 daemon 中冻结临时 JPEG，非只读、非幂等，但不生成 Observation。
- `submit_image_annotation` 经 daemon 持久化视觉推断；其他 12 个工具为只读工具。
- 历史记录、当前帧、空间候选、调用方标注是不同来源，不能混写成“物品现在就在这里”。

| 工具分组 | 工具 | 必要依赖 |
| --- | --- | --- |
| 连接检查 | `server_info` | MCP 连接本身 |
| 直接历史 | `list_recorded_items`、`locate_item`、`list_observations`、`get_observation` | 可读的现有 DB |
| 几何/空间 | `get_observation_context`、`get_spatial_hypotheses` | DB；候选需 geometry/events 事实，depth 可缺失并保持 unknown |
| daemon 状态 | `list_cameras`、`get_health` | DB identity、daemon endpoint、读 token |
| 原图/当前帧 | `get_item_images`、`get_camera_frame` | 同一 dataset 的 daemon、读 token；取帧另需可用 camera |
| 标注读取 | `get_image_annotation`、`list_image_annotations` | 同一 dataset 的 daemon、读 token、标注 schema |
| 标注提交（可选） | `submit_image_annotation` | 上述依赖，加双方写 opt-in 和独立写 token |

## 2. 安装并接入 stdio

需要 Rust **1.89 或更新版本**。MCP 本身不需要 camera/YOLO/FFmpeg native 依赖。
下面给出构建与接入步骤；它们不会自动替换你正在运行的 MCP/daemon 或替你配置摄像头。

**Windows PowerShell：**先进入你实际的仓库目录，下面路径仅为占位示例。

```powershell
Set-Location 'C:/projects/item-finder-mcp'
cargo +stable build -p item-mcp
```

产物为 `target/debug/item-mcp.exe`。

**Linux/macOS Bash：**在仓库目录运行，产物为 `target/debug/item-mcp`。

```sh
cargo +stable build -p item-mcp
```

构建后由 MCP host 直接启动 executable。stdio 启动参数示例（Windows PowerShell）：

```powershell
& 'C:/projects/item-finder-mcp/target/debug/item-mcp.exe' --stdio --db 'C:/projects/item-finder-mcp/data/items.db'
```
这是给 host 使用的入口；直接启动会等待 stdin 上的 MCP JSON-RPC，不是交互式寻物 CLI。
`--stdio` 可省略；stdout 只承载 MCP 消息，日志在 stderr；连接初始化后关闭 stdin 可结束 server。

DB 选择顺序为 `--db` > `ITEM_DB` > `data/items.db`。
相对 DB 路径按 MCP 启动 cwd 解析；host 配置应使用绝对路径。

| 启动选项 | 默认 / 环境变量 | 使用范围 |
|---|---|---|
| `--db` | `ITEM_DB`，否则data/items.db | 历史数据集，reader不建库 |
| `--priors` | `ITEM_PRIORS`，否则builtin | 启动时空间先验，文件≤1MiB |
| `--daemon-endpoint` | `ITEM_DAEMON_ENDPOINT`，默认不连接 | 数字loopback HTTP origin |
| `--daemon-token` | `ITEM_DAEMON_TOKEN` | 支持CLI但推荐环境，勿放命令历史 |
| `--daemon-timeout-secs` | 15，无对应env，必须>0 | daemon工具请求；不重设几何固定预算 |
| `--enable-annotations` | 默认关闭，无env开关 | 另需独立 `ITEM_DAEMON_ANNOTATION_TOKEN` |
| `--stdio` | 可省略 | 当前唯一MCP transport |

显式CLI高于对应环境变量；不是每个参数都有env。`RUST_LOG`控制stderr日志。
DB 缺失/不可读返回 `database_unavailable`，不是成功的空列表。
之后 ingest 创建好 DB，后续查询即可重试，不必仅为 DB 出现而重启 MCP。
旧的 observation-only DB 仍可查基础历史，但不保证几何、资产 identity 或标注可用。

### ZCode 配置

打开 **Settings → MCP**，把以下内容合并到 `mcp.servers`，不要覆盖其他 server。
这里使用 ZCode 的 `type`/`command`/`args` 结构，**不是 generic `mcpServers`**。
路径是本项目位置示例；DB 必须指向你选择的真实 ingest 数据集。

```json
{
  "mcp": {
    "servers": {
      "item-finder-mcp": {
        "type": "stdio",
        "command": "C:/projects/item-finder-mcp/target/debug/item-mcp.exe",
        "args": ["--stdio", "--db", "C:/projects/item-finder-mcp/data/items.db"]
      }
    }
  }
}
```

需要文件配置时，用户级为 `~/.zcode/cli/config.json`，workspace 为 `.zcode/config.json`；
同名 server 的用户级设置覆盖 workspace。本文不要求读取或复制你的已有 secret 配置。
修改 executable/参数后重新连接 MCP 或重启 host 会话，让 executable 和工具清单重新加载。
Windows 若运行中的 exe 被锁定，可另行构建 `cargo +stable build -p item-mcp --target-dir target/mcp-query`，
再将 command 改成该目录下的 `debug/item-mcp.exe`；不要为了构建强行结束用户进程。

## 3. 按需接入本地 daemon

仅历史/几何查询不要求 daemon。要读状态、图片或标注，需 daemon 的独立内部 HTTP listener。
在 daemon TOML 的 `[daemon]` 中合并这些字段；已有表头时不要重复。它们不是 MCP 工具参数，listener 与 Frigate webhook/preview listener 不同。

```toml
[daemon]
internal_listen = "127.0.0.1:8480"
internal_max_asset_bytes = 8388608
```

需要启动 daemon 时，使用已配置好采集后端的 ingest executable，参数为 `--daemon --config` 加配置文件路径；
camera/native 后端准备和服务启动应单独授权，不能由一次 MCP 查询自动完成。

daemon 和 MCP 启动环境均需同一个 `ITEM_DAEMON_TOKEN`；listener 缺 token 时 daemon 拒绝启用。
MCP 还需 `ITEM_DAEMON_ENDPOINT=http://127.0.0.1:8480`，或在 args 中加
`--daemon-endpoint` 与 `http://127.0.0.1:8480`。endpoint 参数优先于环境变量。
endpoint 只接受 **HTTP numeric loopback origin**；不要用 `localhost`、LAN 地址、URL 凭据、路径、query 或 fragment。

**凭据只经环境提供。** MCP 会把读 token 放入 Bearer header；不要传 `--daemon-token` 明文、
不要放入 JSON tool 输入、URL、tracked 文件或对话。读 token 应为非空可见 ASCII，无空格。
先由安全的环境/服务管理方式注入 token，再从该环境启动 host；重用已启动的 GUI 不保证继承新环境。
本文不假定 `.env` 自动加载，也不假定 ZCode JSON 会替换 `${ENV_VAR}`。

**Windows PowerShell，token 已通过安全渠道注入当前环境：**

```powershell
if ([string]::IsNullOrWhiteSpace($env:ITEM_DAEMON_TOKEN)) { throw '请先安全注入读 token' }
$env:ITEM_DAEMON_ENDPOINT = 'http://127.0.0.1:8480'
```

**Bash/Git Bash，token 已安全注入：**

```sh
: "${ITEM_DAEMON_TOKEN:?请先安全注入读 token}"
export ITEM_DAEMON_ENDPOINT='http://127.0.0.1:8480'
```

从同一环境启动 host；上面不设置、打印或复制 token，daemon 服务环境需独立配置。

daemon 配置内相对路径按 **配置文件目录** 解析，与 MCP 的 startup cwd 规则不同。
MCP 从 `--db` 获取 dataset identity，核对 daemon 响应，不能仅凭“文件名都叫 items.db”认定同源。
不一致返回 `dataset_mismatch`；缺少 identity 返回 `schema_unavailable`，不是让 MCP 自动建表。
daemon 停止时历史查询仍可用，但图片、状态和标注读取会失败。

## 4. 最小连接与寻物流程

以下 JSON 均为**工具输入**，在 host 中选择对应工具调用，不要原样写到 stdio stdin。
`desk`、`tray`、Observation `42` 都是示例，必须替换成你有权限访问且实际返回的值。

1. 调用 `server_info`，输入 `{}`。success 仅证明 MCP 连接，**不证明 DB/camera 在线**。
   当前源码 mode 为 `queries_only`、`queries_camera_frames_and_annotation_reads` 或
   `queries_camera_frames_and_annotations`；这是配置能力，不是 daemon 的实时健康。
   若已部署版本仍显示旧 mode/旧工具数，以实际 `tools/list` 为准，不能只凭 version 判定更新成功。
2. 中文物品名或 label 不确定时，先调用 `list_recorded_items`。

```json
{"camera":"desk","limit":200}
```

success 的 `items` 含 `label`、`observation_count`、`last_seen`、`cameras`、`zones`；
`truncated` 表示还有更多 label。count 是 Observation 行数，**不是实例数或帧命中数**。
label 可能包含误检；库存不是当前画面清单，也不含调用方 annotation label。

3. 从实际库存选 label，调用 `locate_item`。

```json
{"label":"cup","camera":"desk","zone":"tray","limit":20}
```

success 的 `direct` 是历史直接目击，`keyword`/`label_source` 说明使用的 label 来源。
显式 `label` 优先于 `query`；英文 `query` 仅用关键词 heuristic，不是中文翻译或语义搜索。
空列表是成功但未匹配；不能据此断言物品不存在。这个工具不返回图片或空间候选。

4. 要筛时间段，调用 `list_observations`。

```json
{"label":"cup","camera":"desk","since":"2026-10-01T00:00:00Z","until":"2026-10-11T00:00:00Z","limit":20}
```

时间是含 timezone 的 RFC3339，边界包含在内，过滤 **last_seen**，不是 first_seen。
label 是字面 substring，不支持 SQL/wildcard；camera/zone 精确匹配。过滤先于 limit，结果最新优先。
上述历史列表和定位 limit 默认 20；库存默认 200；三者范围均为 1–500。

5. 调用 `get_observation`，输入 `{"observation_id":42,"camera_id":"desk"}`。
   success 的 `observation` 仅含 ID、label、camera、zone、first/last_seen、hit_count；
   不含 geometry/events/depth/image。`camera_id` 是精确归属 guard，错源会报错。
   注意列表用 `camera`，单条/图片/几何工具用 `camera_id`，不是互通 alias。

## 5. 状态、当前帧与 raw 复读

先调用 `list_cameras` 或 `get_health`，两者输入均为 `{}`。
`list_cameras` success 返回 `cameras` 的 exact ID、source_kind、enabled、state、
frame_supported/frame_available、cached_frame_age_ms 和计数；不会替你选择 camera。
`get_health` 另含 daemon_state、started_at、observed_at、cache/frame-asset 用量和上限。
这些是状态采样，不是控制，也不保证随后取帧成功。发现 camera 不等于授权遍历全部 camera。
无 `url`/`webcam` 的 webhook-only camera 不能提供当前帧；disabled/offline 也不能当作可用来源。

调用 `get_camera_frame`：

```json
{"camera_id":"desk","max_age_ms":2000,"wait_timeout_ms":3000,"max_bytes":4194304}
```

- max_age_ms：1–60000，默认 2000；限制**冻结时**的帧龄。
- wait_timeout_ms：0–10000，默认 3000；没有合格帧时等待新缓存帧的上限。
- max_bytes：1–8388608，默认 4194304；是 JPEG 字节上限，不是要求缩图。
- exact camera ID 可含 Unicode，1–1024 UTF-8 bytes，无控制字符；`.`/`..` 保留不用。

success 返回 `images` metadata 和 MCP image blocks，`content_index` 对应 result content 中的图片。
metadata 含 asset_id、database_id、sha256、width/height、bytes、capture_time、camera_id、expires_at；
独立 frame 的 observation_id 为 null，provenance 为 `daemon_camera_frame`。
外层 age_ms/camera_state 描述冻结时状态，capture_time 是 daemon 收帧时间，不是精确请求时刻。
取帧只使用已有 decode cache，不新开 camera 连接、不触发检测、不调用 VLM。
重复取帧可能得到更新图；要分析同一张图，请复读**同一 asset_id**。

调用 `get_item_images` 读取历史 Observation 的未标注原图：

```json
{"image_kind":"raw","observation_ids":[42],"camera_id":"desk","max_images":4,"max_bytes_each":4194304}
```

复读 frame 或 annotation evidence 时改用 asset_ids（以下为合法格式的虚构 ID，必须替换）：

```json
{"image_kind":"raw","asset_ids":["frame-v1-0123456789abcdef"],"camera_id":"desk","max_images":1}
```

至少 1、最多 32 个 selectors（两类数组合计）；max_images 为 1–8，默认 4；
max_bytes_each 为 1–8388608，默认 4194304。selectors 可混用，归并去重后按 capture_time 最新优先。
所有选中资产都受 camera guard 校验；不能依赖 limit 忽略一个错源或失效 selector。
单图超过字节预算报错；响应总编码预算或 max_images 导致裁剪时 `truncated=true`。
只接受 opaque asset ID，不接受文件路径/URL。MCP 验证 identity、JPEG/hash/尺寸后才返回图片。
只支持 `image_kind="raw"`；不回退到 detector 烧框 snapshot，也不为旧图/Frigate 引用补造 raw provenance。

daemon cache 默认 TTL **5000 ms**；冻结 frame asset 默认 TTL **600 秒**。
可在 `[daemon]` 调整 `frame_cache_ttl_ms`（1–5000）及 `frame_asset_ttl_secs`（1–86400）。
缓存 TTL 与 asset TTL 是两回事，以返回的 `expires_at` 为复读边界；复读不续期。
raw Observation 资产仍受 daemon 保留/清理约束，不应因没有 frame expiry 字段就假定永久保存。
标注 evidence 有独立保留期，既不续原 frame TTL，也不是重新拍照。

## 6. 单图标注：读取、授权写入、修订

默认提供两个标注读工具，但需要 daemon；写入须 **daemon 与 MCP 双方 opt-in**。
在 daemon `[daemon]` 加 `annotations_enabled = true`，可设 `annotation_retention_days = 30`（1–365）。
默认配额为 10000 sets、50000 revisions、512 MiB metadata、2 GiB evidence；
对应配置项为 annotation_max_sets/revisions/metadata_bytes/evidence_bytes，可按运维需要调整。
daemon 与 MCP 的环境均需另一个 `ITEM_DAEMON_ANNOTATION_TOKEN`，必须不同于读 token。
MCP args 加 `--enable-annotations`；缺 endpoint/读 token/独立写 token 会启动失败。
没有 MCP flag 时提交工具不注册；daemon 禁写时拒绝提交，但已有标注仍可读取并按 retention 清理。
启用工具不代表授权每次写入；调用方应在明确用户授权后保存分析。

先读 raw 并保留完整 source metadata，再调用 `submit_image_annotation`。
下例 ID/hash/UUID 均为**格式合法的虚构占位**，不是可用证据；替换成图片响应的 exact 值。

```json
{
  "schema_version": 1,
  "request_id": "desk-analysis-001",
  "asset_id": "frame-v1-0123456789abcdef",
  "sha256": "0000000000000000000000000000000000000000000000000000000000000000",
  "camera_id": "desk",
  "coordinate_system": "normalized_xywh",
  "producer": {"kind":"vlm","host":"zcode"},
  "annotations": [
    {"object_id":"cup-1","label":"杯子","bbox":[0.1,0.2,0.2,0.3]},
    {"object_id":"cup-2","label":"杯子","bbox":[0.6,0.2,0.15,0.3]}
  ]
}
```

单图 0–128 objects，整批原子保存；空数组表示本 revision 无物体，不证明场景里真的没有物体。
bbox 是原图左上为原点的 `[x,y,width,height]`，归一化到 0–1；w/h>0、x/y≥0、框不能越界。
不要直接提交 pixel xyxy、裁剪图坐标、0–1000 坐标；应先还原到原图再归一化。
object_id 在 revision 内唯一，1–64 ASCII letters/digits/`-`/`_`；重复 label 合法。
request_id 同样字符集、1–128 bytes；label 非空、≤256 UTF-8 bytes，note 可选、≤1024 bytes。
confidence 可选且为有限的 0–1 数；不是已校准准确率。producer.kind 必须是 vlm，host 非空，
model/prompt_version 可选；各 producer 文本≤256 bytes，都是调用方自报，不是认证模型身份。
不要增加未知字段、重复 JSON key 或自行传 database_id；MCP 从自己的 DB 注入 identity。
写 body 上限 256 KiB、读响应上限 512 KiB，单 set 最多 100 revisions。

success receipt 的 `status="persisted"` 表示提交已落盘，包含 annotation_id、revision、request_id、
evidence_asset_id、object_count、persisted_at、expires_at；provenance 为 `caller_visual_inference`。
它不是 Observation，不改变 detector label inventory，也不提供跨帧/跨 camera 的物品身份。
原 JPEG 独立保留为 evidence；新建不能把 evidence ID 当 source，必须使用原 raw source ID。

调用 `get_image_annotation`（以下 UUID 需换为 receipt 中 ID）：

```json
{"annotation_id":"11111111-1111-4111-8111-111111111111","revision":1,"camera_id":"desk"}
```

revision 可省略取当前版，指定范围 1–100；旧 revision 不变。detail 含完整 objects/producer 和源信息，
以及 original_source_status、evidence_status。读成功不保证原 source/evidence 都可读，须检查各 status；
evidence_status 可为 available/missing/integrity_error，source 还可为 source_expired。
用 evidence_asset_id 调 `get_item_images` 读取同一未烧框 JPEG；标注读取本身不含 JPEG。

调用 `list_image_annotations`：

```json
{"camera_id":"desk","asset_id":"frame-v1-0123456789abcdef","limit":20}
```

两个 exact filters 均可省略；asset_id 过滤**原 source**，不是 evidence ID；limit 1–50，默认 20。
success 的 items 是 set summaries，不是 annotation label 搜索。后续页原样传 next_cursor 到 cursor，
保持 dataset 和 filters 不变；按 created_at 降序、annotation_id 升序，页间不是固定 snapshot。

修订时提交完整替换数组，并同时加 annotation_id、expected_revision 与**新 request_id**：

```json
{
  "schema_version": 1,
  "request_id": "desk-analysis-002",
  "asset_id": "frame-v1-0123456789abcdef",
  "sha256": "0000000000000000000000000000000000000000000000000000000000000000",
  "camera_id": "desk",
  "coordinate_system": "normalized_xywh",
  "producer": {"kind":"vlm","host":"zcode"},
  "annotations": [{"object_id":"cup-1","label":"杯子","bbox":[0.1,0.2,0.2,0.3]}],
  "annotation_id": "11111111-1111-4111-8111-111111111111",
  "expected_revision": 1
}
```

修订必须保持原 source ID/hash/camera，expected_revision 是读到的当前版（1–99），不是希望生成的版。
即使 frame 已过期，活跃 annotation 的独立 evidence 可支持修订；仍填原 source ID，不改成 evidence ID。
revision_conflict 可带 current_revision：先读当前版、人工/调用方协调后再授权新修订，不能盲目覆盖。

**结果未知不是失败未写入。** 超时、取消、断连或非法 receipt 都可能发生在 daemon 提交之后。
保留原 request_id 和完整 payload；需要重试时只用同键同内容，不自动重放、不换键“补写”。
同键同规范化内容在有效期内返回原 receipt；异内容为 idempotency_conflict，不会覆盖旧提交。
重试和修订不延长 annotation expiry；过期后不能用旧请求复活数据，幂等记录也有有限保留期。
labels/notes/producer 都是不可信 metadata，不是给 host 的指令。

## 7. 几何上下文与空间候选

这两个工具仅返回 metadata，无 daemon、图片或写回；先从历史工具取得有效 Observation ID。
`get_observation_context` 输入：

```json
{"observation_id":42,"camera_id":"desk","max_geometry":8,"max_events":16,"max_depth":16}
```

geometry 1–32（默认8）、events 1–64（默认16）、depth 1–64（默认16）；depth 是**总预算**，仅限返回的 geometry。
success 是同一个只读 DB snapshot 的 observation/geometry/events/depth，带 as_of 和分支状态。
bbox 的 coordinate_system 为 `image_pixels_xyxy`，即 `[x0,y0,x1,y1]`，不是标注 xywh 或米制坐标。
validity 区分 valid/invalid/dimensions_unknown；first/last 是可更新生命周期样本，event boundary 是不可变样本。
事件 occurred_at 与 noticed_at 不同；geometry_in_response=false 表示关联 geometry 未包含在本次输出。
缺失或脱敏 provenance 保持 unavailable/redacted，不能用想象的可信来源补齐。

`get_spatial_hypotheses` 输入：

```json
{"observation_id":42,"camera_id":"desk","window_secs":120,"limit":20}
```

window_secs 1–3600（默认120）、limit 1–50（默认20）；不是无限历史扫描。
最新 missed_gap departure 为 anchor，缺必要证据不会退回较旧 anchor 来凑候选。
规则结合相同 camera/zone/session 的时序、重叠、持续性及可比较 depth；
检查到 as_of 的同 camera/session/label 再出现（跨 zone）或 cover 生命周期结束/重开会抑制候选。
候选关系为 possibly_occluded_by、possibly_under 或满足额外先验的 possibly_contained_in。
status=candidate；score_semantics=deterministic_heuristic_not_probability，分数不是概率，window 改变会影响评分。

可选 priors 只在启动加载：`--priors` > `ITEM_PRIORS` > builtin，建议绝对路径。
参考可发布的 [priors 示例](../crates/item-query/priors.example.toml) 填写尺寸 interval 和实测 camera/zone container。
文件≤1 MiB、UTF-8 TOML，加载一次并对原始 bytes 计算 SHA-256；修改后需要重启 MCP。
明确配置却不可读/非法会启动失败，不 fallback；builtin alone 不会提升为 possibly_contained_in。
配置的开口/可用内腔和尺寸适配仍不证明实际进入、任意装箱或物理容纳。
工具不接受 priors_profile、文件路径、URL、SQL 或 include_images 参数。

阅读 success 时同时检查以下字段，不要只读 candidates 数量：

- branches/availability：available、not_available（如 no_rows）、schema_unavailable 或 partial。
- reason/truncated：count_limit、response_limit、optional_provenance_missing 等解释不完整性。
- 空候选可能是 no_eligible_departure、anchor_evidence_missing、same_label_reappeared 等，不是“物品没有被遮挡”。
- scan_complete/provenance_complete 与 diagnostics：内部 scene/depth 扫描截断时保守返回空候选，不能升级断言。
- candidates_truncated：只返回部分候选；JSON payload 上限 256 KiB，必要完整事实超预算则 payload_too_large。
- evidence 的 supporting/conflicting/unknown 与 typed refs：按 observation/event/geometry/depth/prior_entry 类型解读。
- used_facts：实际使用事实的去重 bundle，与候选来自同一 snapshot；不是另一次 context 调用拼出的近似证据。
- source_assets.status=not_checked：没有验证 geometry/raw 映射，source_asset_id 为空，不能把 snapshot_ref 当 raw ID。
- identity_resolution=label_zone_aggregate、instance_identity_unverified=true：不是实体追踪；同类 label 不等于同一实物。

geometry-mcp-v1 当前规则版为 g4-size-mcp-1；每次请求有 15 秒 handler/DB 等待预算。
transport 排队和 stdout 交付另计；取消不能立即终止 OS file open 或已运行的 blocking 工作。
unknown depth、缺尺寸、缺事实或不完整 provenance 必须保留未知，不能当作 supporting。

## 8. 常见错误与排查顺序

工具错误通常为 `isError=true`，structuredContent 中含 error；JSON-RPC 输入解析错误另由协议层报告。
以下是 error 形状示例，不是一个成功的空结果：

```json
{"error":{"code":"database_unavailable","message":"The configured database could not be read."}}
```

| code | 处理方式 |
| --- | --- |
| invalid_input | 检查真实 tools/list schema、ID/limit/坐标；几何可选字段必须省略而非 null。不要加猜测参数。 |
| database_unavailable | 核对 MCP 绝对 DB 路径、权限和 ingest 是否建立 DB；不让 MCP 创建空库“修复”。 |
| schema_unavailable | 区分工具 error 与几何分支状态；缺 identity/annotation/geometry 能力须由 ingest 运维处理。 |
| not_found / camera_mismatch | 用本次返回的 ID 与 exact guard，先排除错误 dataset/camera，禁止自动换 camera。 |
| daemon_unavailable / daemon_busy / busy | 核对 endpoint/服务及并发预算；读可稍后重试，写则按结果未知规则处理。 |
| unauthorized / forbidden / annotation_disabled | 检查双方读/写环境 token 和写 opt-in；不可通过把 token 放入工具参数绕过。 |
| dataset_mismatch | DB 与 endpoint 指向不同 dataset；先核对配置，不复制或覆盖正在使用的 DB。 |
| camera_not_found / camera_offline | list_cameras/get_health 核对 exact ID、enabled/source/state；工具不会启动 camera。 |
| frame_unavailable / frame_stale | 未有合格帧或超过帧龄；检查采集状态，再按需求调整等待/帧龄，不谎称实时。 |
| raw_unavailable | 此 Observation 没有注册 raw；旧烧框图和 Frigate 引用不能替代未标注原图。 |
| source_expired / annotation_expired | 原图或标注已过期；复读不续 TTL，不能让旧请求复活。 |
| source_hash_mismatch / integrity_error | 核对 exact source/hash，停止猜测或写入；不要拿另一张图的 metadata 提交。 |
| idempotency_conflict / revision_conflict | 前者同键异内容，后者基于旧版；保留请求并先读当前状态，不盲目改键覆盖。 |
| quota_exceeded / payload_too_large | 缩小批次/读取预算或由运维调整 retention/配额；不用删除用户数据来凑测试空间。 |
| invalid_daemon_response / cancelled / timeout | 区分读失败与写可能落盘；写保留同 request_id/payload，先确认结果再行动。 |

建议排查顺序：server_info/实际工具清单 → 基础历史 → identity/daemon 状态 → 单图 → 标注/空间证据。
本文核实的是源码参数、DTO、注册及限制；未验证当前安装版本、用户 DB、token、daemon 在线性、camera 画面或场景推断准确率。
