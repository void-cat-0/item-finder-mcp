# Daemon raw asset API v1

本契约覆盖 daemon 与 `item-mcp` 的本机图片边界。MCP transport 仍为 stdio；下面的 HTTP 是独立的内部业务接口，不是 MCP HTTP transport。资产写入始终归 daemon 所有；当前支持 observation raw 读取及摄像头最新帧的按请求冻结，不实现标注写入。

## 配置与权限

- daemon 的 `[daemon] internal_listen` 未配置时关闭内部 listener 与当前帧缓存；启用时只能绑定数字 loopback 地址，如 `127.0.0.1:8480` 或 `[::1]:8480`。
- `[daemon] frame_cache_ttl_ms` 默认 5000，范围 1–5000；`frame_asset_ttl_secs` 默认 600，范围 1–86400。前者控制内存帧缓存寿命，后者控制冻结资产有效期，二者独立。无需为已有配置补字段即可使用默认值。
- `ITEM_DAEMON_TOKEN` 提供共享 bearer token。内部 listener 不与 Frigate webhook/preview 共用端口；没有 token 时拒绝启用，不把 loopback 当作认证。
- MCP 配置 `--daemon-endpoint http://127.0.0.1:8480` / `ITEM_DAEMON_ENDPOINT` 和 `ITEM_DAEMON_TOKEN`。CLI 显式 endpoint 优先于环境变量。`--daemon-token` 也支持，但环境变量可避免命令行历史中的明文 token。
- MCP endpoint 只接受 HTTP 数字 loopback origin；不接受用户名密码、查询参数、片段或额外路径，不使用代理，不跟随重定向。
- 浏览器 Origin 和不匹配的 Host 被拒绝；不开放 CORS。内部 API 不接受任意路径、URL、SQL 或命令。
- daemon 不可用时，MCP 的现有 SQLite 基础查询仍按只读方式工作。图片工具明确失败，不以空结果替代错误，也不回退为直接读取 snapshot 文件。

## 图片身份与来源

`raw` 表示将采集的未标注 RGB 画面编码为 JPEG；不保证保留相机原始压缩码流。没有 detector 框、zone 或 label 叠加。现有 `sample_snapshot`、geometry `snapshot_ref` 和 `frigate://` 引用不会自动成为 raw。

启用内部 listener 后，raw 在新 observation 建立时保存一张代表帧，不记录每一个解码帧；已有 observation 不自动回填。Observation 提交先于 JPEG 编码和 manifest 发布，因此新记录可短暂返回 `raw_unavailable`，调用方可以有限重试；编码/发布失败不会把直接观测删除。资产 ID 是 daemon 生成的不透明标识，绑定所有者身份和 JPEG SHA-256；客户端不能通过 observation ID 猜测某个文件就是 raw。manifest 保存捕获时间、源 observation/camera/zone、图片尺寸和 hash。发布后不可用可变的 geometry `last` 样本或路径作为资产身份。

daemon 在 SQLite 的独立 identity 表保存随机数据源身份，不改变 observation schema 版本；raw manifest 与 `asset_id` 同时绑定该身份。MCP 只读查询 `--db` 的身份并与 metadata 的 `database_id` 比对；两者不一致返回 `dataset_mismatch`，旧库没有身份返回 `schema_unavailable`。MCP 不创建该表或身份。复制同一数据库保留同一数据源身份；它标识数据集而非文件路径或进程。

客户端必须使用返回的 `database_id`、`asset_id` 和 `sha256` 引用看到的具体版本。图片所见内容仍需调用方判断；数据库直接目击、detector evidence 和调用方 visual inference 的事实边界保持分离。

## HTTP 端点

所有请求需要 `Authorization: Bearer <token>`。

| 方法与路径 | 行为 |
| --- | --- |
| `GET /internal/v1/observations/{id}/raw-asset` | 核验 observation 存在后，取得其已登记 raw metadata。无 raw 返回 `raw_unavailable`。 |
| `GET /internal/v1/assets/{asset_id}` | 按不透明资产 ID 取得已登记 raw metadata，核验 observation 归属或 camera frame 身份与有效期。 |
| `GET /internal/v1/assets/{asset_id}/content` | 返回对应原始 JPEG 字节。 |
| `POST /internal/v1/cameras/{camera_id}/frame` | 等待满足新鲜度要求的最新缓存帧，将其冻结为有期限的不可变 raw 资产，返回帧 metadata。 |

成功 metadata 是 `item_core::assets::RawAssetMetadata`：

```json
{
  "asset_id": "raw-v1-12-<owner-hash>-<jpeg-sha256>",
  "database_id": "<daemon-persisted database identity>",
  "kind": "raw",
  "mime_type": "image/jpeg",
  "bytes": 123456,
  "sha256": "<64 lowercase hex characters>",
  "width": 1920,
  "height": 1080,
  "capture_time": "2026-10-09T12:00:00+00:00",
  "observation_id": 12,
  "camera_id": "desk",
  "zone": "frame",
  "status": "available",
  "provenance": "daemon_raw_asset"
}
```

Observation 资产成功响应必须包含正整数 observation ID、非零尺寸和 RFC3339 capture time，provenance 为 `daemon_raw_asset`。Camera frame 资产的 provenance 为 `daemon_camera_frame`，`observation_id` 和 `zone` 为 null，必须有 camera ID 和 RFC3339 `expires_at`；不制造观测归属。metadata 不暴露本机路径。不可读/过期通过错误返回；`status` 成功时为 `available`。

content 响应为二进制 JPEG，包含 `Content-Type: image/jpeg`、`Content-Length`、`x-asset-id` 和 `x-asset-sha256`。MCP 同时核验 metadata、内容长度、SHA-256、JPEG 尺寸和返回的资产身份。metadata/content 之间发生删除或损坏，调用失败，不返回另一张图片。

错误封装：

```json
{"error":{"code":"raw_unavailable","message":"No raw asset is registered for this observation."}}
```

稳定业务类别包括 `invalid_input`、`unauthorized`、`forbidden`、`not_found`、`raw_unavailable`、`source_expired`、`payload_too_large`、`busy`、`integrity_error`、`invalid_source` 和 `internal`。当前帧接口另外返回 `camera_not_found`、`camera_offline`、`frame_unavailable`、`frame_stale` 和 `dataset_mismatch`：分别表示摄像头 ID 未配置、源不可用、尚无帧、等待后仍没有符合新鲜度要求的帧和数据源不一致。MCP 使用固定安全消息映射错误，不向 host 转发 daemon 的内部堆栈、路径或凭证。网络错误/超时映射为 `daemon_unavailable`，不一致的协议或图片响应映射为 `invalid_daemon_response`。

## 当前摄像头帧

内部 POST 使用如下 JSON；`camera_id` 是已配置摄像头的精确 ID，作为单个 URL path segment 编码。MCP 拒绝空 ID、控制字符、超过 1024 字节或仅为 `.` / `..` 的 ID，支持普通 Unicode 摄像头名称：

```json
{
  "database_id": "<MCP 只读查询到的数据源身份>",
  "max_age_ms": 2000,
  "wait_timeout_ms": 3000,
  "max_bytes": 4194304
}
```

数据库身份在冻结前校验，防止请求配置错误时向另一数据集写入资产。成功响应为 `{ "asset": <RawAssetMetadata>, "age_ms": 123, "camera_state": "running" }`。`max_age_ms` 为 1–60000，`wait_timeout_ms` 为 0–10000，`max_bytes` 为 1–8388608；缓存 TTL 同时限制有效新鲜度，较大的请求值不会延长缓存寿命。

缓存由已有 camera runner 在取得 RGB 帧后、检测前更新；没有检测结果、检测节流或 detector 失败都不阻止缓存更新。请求不额外打开相机，不触发检测，也不向 observations/geometry/events 写入记录。新鲜度按单调时钟计算，`capture_time` 表示 daemon 收到画面的墙上时间，不承诺相机曝光时间或调用瞬间拍摄。过旧的帧可以等待更新，超过等待期限明确失败；源重连、失败或停止会使旧缓存立即失效，不能在重连后沿用旧帧。

按请求编码并保存 JPEG，而非每帧写盘。资产拥有稳定身份、hash 和固定到期时间（capture time + 配置的 asset TTL），同一帧的重复冻结不延长寿命；同一帧的并发冻结也复用相同身份。到期前可以按资产 ID 复读，摄像头断线不改变已经冻结资产的有效期。当前帧缓存与持久资产采用独立期限和预算，定期清理不依赖新请求或 Observation retention；预算不足时拒绝请求，不提前删除仍在有效期内的资产。

## MCP 工具

`get_camera_frame` 的输入示例：

```json
{"camera_id":"laptop","max_age_ms":2000,"wait_timeout_ms":3000,"max_bytes":4194304}
```

后三个字段可省略，默认值分别为 2000、3000、4194304。返回结构化 metadata 和 MCP image content，包含资产到期时间、帧年龄及摄像头状态。因为请求会让 daemon 保存临时图片资产，该工具声明 `readOnlyHint=false`；不声明幂等，因为后续调用可能取得更新的帧。`get_item_images` 继续只读，并支持通过 `asset_ids` 复读当前帧资产，不刷新其有效期。标注工具尚未实现，未来可以引用同一 asset ID/hash。

`get_item_images` 的输入示例：

```json
{
  "image_kind": "raw",
  "observation_ids": [12, 18],
  "asset_ids": [],
  "max_images": 4,
  "max_bytes_each": 4194304
}
```

- `image_kind` 必须显式为 `raw`；本版不接受 `annotated_evidence`。
- 至少一个 observation/asset selector，合计最多 32 个；observation ID 必须为正整数，asset ID 只能使用长度不超过 192 的 ASCII 字母、数字、`-` 和 `_`。
- `max_images` 默认 4，上限 8；`max_bytes_each` 默认 4 MiB，上限 8 MiB。图片 base64 内容合计上限 16 MiB，metadata 另有有界开销。
- 先解析全部 selector；未知 observation/asset、无 raw、失效源或坏 metadata 都返回 tool error，不默默忽略失败 selector。
- 按 capture time 从新到旧、asset ID 升序稳定排序，按资产 ID 去重；数量或总大小导致截断时返回 `truncated: true`。单图超限返回明确错误。
- result 包含相同的 JSON text 与 `structuredContent`，随后是 MCP image content。每张 metadata 的 `content_index` 指向对应 content block；模型不能依靠 selector 输入顺序猜测图片身份。
- 图片读取没有写副作用。不会创建 annotation，不修改 Observation，不将新 label 混入 `list_recorded_items`。

## 限额与生命周期

内部 POST body 上限 16 KiB，认证 middleware 在 JSON 解析前执行，非法或超限 JSON 也使用安全业务错误封装。等待新帧与后续编码共用四个 daemon 图片工作预算，已经开始的 blocking 工作在请求取消后仍持有 permit 直到结束；取消不承诺回滚已经开始的资产发布，可能留下按 TTL 正常清理的图片，但不会产生无界后台队列。编码前后均重新核验缓存新鲜度与源 generation，不把编码期间断线或已经过旧的画面宣称为新鲜成功。

内部读取限制并发、单图大小、manifest 大小、JPEG 最大尺寸及像素数；文件读取/解码在有界 blocking 工作中完成，HTTP 路由不长期持有采集 Store 锁。metadata/content 分别核验完整 JPEG，第一版会重复读取与解码，优先保证来源和完整性；大目录通过有界扫描降级为忙碌，尚未建立资产索引。daemon 与 MCP 均限制四个并发图片计算；JPEG 上限 16384×16384 且总像素不超过 64,000,000，manifest 上限 16 KiB。Observation 资产查找在超过 100,000 个目录条目时返回忙碌，不做无界扫描。

MCP 图片结果生成默认超时 15 秒，可通过 `--daemon-timeout-secs` 配置；取消会停止后续 HTTP 工作，已启动的有界 blocking 计算保留预算直到结束。stdio 另设发送背压，最多四个从请求处理到发送完成的请求及一个未交给 handler 的前瞻请求。stdout 写入有独立期限；部分 JSON 写入失败或超时会终止连接，不能继续发送损坏的协议流。饱和时后续输入受背压，排在前瞻请求之后的取消通知也需要等待容量释放。16 MiB 是 base64 内容上限，不是整个进程的内存峰值。

资产采用临时文件和不可变发布，再发布 manifest；没有完整 manifest 的文件不向客户端暴露。daemon 重启后仍能解析有效 manifest，但内存最新帧缓存从空状态开始，不能将磁盘旧图当作当前画面。Observation retention 按源 observation 管理其 JPEG 和 manifest；camera frame 资产按自身固定有效期管理，启动清理及每秒空闲维护不依赖新请求。逻辑到期立即拒绝读取，文件删除可能滞后一个维护周期或因占用而重试。源被清理或文件丢失时明确报告不可复核。失败发布残留由 daemon 对账清理。

本版 raw 文件发布依赖同卷硬链接以避免覆盖已有资产；不支持硬链接的文件系统会明确发布失败，不退化为覆盖写。文件已同步、manifest 最后发布；不承诺所有平台文件系统在断电时具有目录级事务原子性，重启后的完整性校验和对账负责识别缺失/未完成资产。

摄像头缓存最多 32 台，每帧 RGB 最多 8 MiB，总预算 256 MiB；正在冻结的帧引用也占预算。典型 1920×1080 RGB 帧可缓存，4K RGB 帧会超过单帧限额。摄像头冻结资产最多 128 份、JPEG 总计 256 MiB；这套预算独立于 observation raw。达到上限不会驱逐尚未过期的资产。第一版通过受限目录扫描计算预算和清理；扫描上限 100,000 个条目，超限明确失败或延后清理，不承诺大目录性能。frame 资产身份绑定到期时间，已删除的到期资产仍返回 `source_expired`。

本版不实现 annotation 提交、派生图、MCP resources 或 prompts。后续 annotation revision/幂等/过期契约应绑定这里的稳定资产 ID/hash，而不是 snapshot 路径；当前有效期只保证到 `expires_at`，不自动为尚未实现的标注流程永久保留。
