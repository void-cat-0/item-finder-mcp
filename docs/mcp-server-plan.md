# Item Finders MCP Server 方案

> 状态：P0/P1 已实现并完成 ZCode 调用验证（2026-10-08）；P2 的 raw 图片读取与 daemon 内部 HTTP 契约已实现（2026-10-09），新增当前帧缓存与 `get_camera_frame`，验收记录见下文。hypotheses、MCP resources 和标注写入仍待实现。
>
> 当前 raw 读取的具体字段、认证、限额与错误契约以 [Daemon raw asset API v1](daemon-asset-api.md) 为准；下文 `get_item_images` 中 label/geometry selectors 和 annotated evidence 是后续完整目标，本版仅提供 observation/asset selectors 和 raw。
>
> 本文是“把当前项目实现为新的 MCP server”的后续实施方案。它取代历史上只描述 G1–G5 视觉推理能力的计划；G1–G5 是底层能力路线，不是 MCP 接口方案。
>
> 项目决策：当前仓库是原项目的 MCP 专用 fork。后续可以直接调整原有 crate、共享 DTO、查询层和运行方式，不必为了保持上游结构而过度隔离；但仍应保持事实/推断边界清晰。

## P0 实际交付

- 新增 `crates/item-mcp`（library + binary），根 workspace 已接入，官方 SDK 锁定为 `rmcp 3.5.1`，启用 server/macros/transport-io；未启用 HTTP 或 native camera feature。
- 默认 stdio，可显式传 `--stdio`；日志写 stderr，stdin EOF 在握手后关闭服务。
- P0 最初只注册 `server_info`、返回 `sdk_validation` 模式；P1 已扩展为五个只读工具，当前返回 `read_only_queries` 模式。
- 使用 SDK 的 `ServerConfig`、typed Parameters/Json、tool router/handler 和 transport API；握手兼容 `2024-11-05`、`2025-03-26`、`2025-11-25` 已经通过真实子进程测试。
- 已验证 tool schema、structuredContent/text 一致性、未知工具、异常参数、错误后继续调用、stdout 纯净性和 EOF 退出。
- `item-mcp` 声明 Rust >=1.89：SDK 自身最低 1.88，但 Cargo.lock 中的 uuid 1.27.0 最低 1.89；本机验证工具链为 stable 1.98.1，尚未单独运行 Rust 1.89 的 MSRV 测试。
- 验证结果：`cargo +stable fmt --all -- --check`、`check --workspace`、`test --workspace`、`clippy -p item-mcp --all-targets -- -D warnings`、`build -p item-mcp` 均通过；workspace 共 158 passed、1 ignored，其中 MCP 3 项为真实子进程协议测试。
- P0/P1 均已完成 ZCode 真实 host 调用验证：五个工具、筛选、清单截断、无匹配、not_found、invalid_input 和错误后继续调用均正常。

## P1 实际交付

- 当前注册 `server_info`、`list_recorded_items`、`locate_item`、`list_observations`、`get_observation`；所有工具只读，带输入/输出 schema、structuredContent/text 和只读 annotations。
- `--db` > `ITEM_DB` > `data/items.db`，相对路径按启动 cwd 固定解析。每个请求在有并发上限的 spawn_blocking 中打开只读 Store；不会创建、迁移数据库，也不管理 daemon。缺库可在后续由 ingest 创建，同一 MCP 进程继续读取。
- 名称清单按所有匹配 observation 聚合原始 label，计数不是 hit_count；包含 camera/zone 摘要和 truncated。camera/zone 筛选在 LIMIT 前执行。
- `list_observations` 支持 literal label/camera/zone/inclusive last_seen 时间范围，RFC3339 带时区输入转 UTC 输出；`locate_item` 使用 explicit label 或现有 English keyword heuristic，仅返回直接目击。
- `get_observation` 当前返回 basic 字段，不返回快照路径/geometry/events/depth；旧 schema 可查询，reader 不迁移。
- 非法业务输入、未知 ID、不可读数据库分别返回 `invalid_input`、`not_found`、`database_unavailable` 的 tool error；无匹配/空库是成功空结果。内部 SQL 细节仅写 stderr。
- P1 验证：stable fmt/check/test/clippy（workspace）及 build 均通过；最终 167 passed、1 ignored，包含 MCP 9 项真实子进程测试和 core 3 项新增读取回归。覆盖旧 schema、无匹配、参数错误、缺库后出现、活跃 WAL、锁等待错误后恢复、数据库不被修改。
- 本机通过新二进制读取 data/items.db 和 data/validation/items.db：分别返回 8/16 个 labels，聚合计数与直接 SQLite 只读查询一致，locate/get 一致，数据库主文件 hash 不变。未启动或重启 daemon。
- 交付构建目录为 target/mcp-query/debug，避免覆盖正在运行的旧 exe；用户已更新 ZCode 并重启，当前 host 调用返回 read_only_queries，基础查询验证通过。
- P1 交付时，include_hypotheses、图片、priors、snapshots-root、resources、prompts、health 和 HTTP 均未实现。后续 raw 图片和内部 HTTP 的状态见下面的 P2 部分交付；当前 locate_item 仍不能传 include_hypotheses，也不能把 historical sighting 当作当前位置。

## P2 部分交付：daemon raw 读取契约（2026-10-09）

- 新增共享 raw metadata/error DTO，daemon 负责未标注 JPEG、持久 manifest 和数据库数据源身份；MCP 通过受认证的独立 loopback HTTP API 查询资产与内容，并只读核对数据源身份，防止跨库同 observation ID 误关联。
- 新增 `get_item_images(image_kind=raw, observation_ids|asset_ids, max_images, max_bytes_each)`，返回结构化 metadata 和 MCP image content，核验身份/hash/MIME/长度/尺寸，缺 raw 不回退带框图。
- 基础 SQLite 只读查询保留，daemon 停止时图片请求明确失败；MCP 不直接读取资产目录或写库。
- 本版不注册 MCP resources，不返回尚不能读取的 resource URI；label/geometry selectors、annotated evidence、hypotheses、priors、prompts、health 和 annotation 写入仍待实现。
- 具体 API、配置、错误与生命周期见 [daemon-asset-api.md](daemon-asset-api.md)。
- 2026-10-09 最终验证：stable fmt/check/test/clippy（workspace，`-D warnings`）通过，198 passed、0 failed、1 ignored；既有一万行 retention 验收另外显式运行通过。MCP 和 camera+yolo ingest 普通二进制构建通过。
- 真实链路验证：内置摄像头 + YOLO daemon → 已持久化 raw → loopback HTTP → MCP stdio 返回 1920×1080 JPEG，数据库身份、长度和 SHA-256 一致；测试 daemon 经 Ctrl-Break 优雅退出。重建 daemon 后的 raw 恢复、停止 daemon 后历史查询继续可用、跨数据库同 ID 拒绝已由集成测试覆盖。未替换/重启当前 ZCode 使用的旧 MCP 进程，新的 tool list 仍需用户更新 host 二进制配置并重连。
- 测试期间发现并修复 Windows 假 HTTP 夹具的 accepted socket 继承 nonblocking、Drop 二次 panic 问题；图片协议测试连续 10 轮通过。stdio 发送预算覆盖生成/排队/发送，11 项背压、取消和部分写入失败测试通过。Rust 1.89 MSRV、Linux/macOS 真机采集未单独验证。

## 当前帧工具交付（2026-10-09）

- 新增 `get_camera_frame(camera_id, max_age_ms?, wait_timeout_ms?, max_bytes?)`；使用 daemon 已有采集链路的最新 RGB 缓存，与 detector 是否产生 observation 无关。
- daemon 接收受认证的内部 POST，先校验数据源身份，再等待符合新鲜度要求的帧并按请求保存不可变 JPEG。MCP 不操作摄像头或文件目录。
- Camera frame 资产无 observation/zone，带 `daemon_camera_frame` 来源和固定 `expires_at`。可经 `get_item_images(asset_ids=...)` 复读；复读不续期，不制造观测记录。
- 缓存默认有效期 5 秒，断线/重连/停止立即清空；冻结资产默认 10 分钟，独立清理和限额，不依赖 observation retention。具体配置、大小预算和错误语义见 [daemon-asset-api.md](daemon-asset-api.md)。
- 当前注册七个工具；`get_camera_frame` 因请求 daemon 保存资产声明非只读，其余六个保持只读；`server_info.mode` 为 `queries_and_camera_frames`。标注提交、revision、幂等及长期保留仍待实现。
- 验证通过：stable fmt/check/test/clippy（workspace，`-D warnings`），213 passed、0 failed、1 ignored（既有一万行 retention 验收）；camera+yolo ingest 与 MCP 普通二进制构建通过。缓存测试覆盖采集年龄、等待更新、无检测与节流更新、断线/重连/停止失效、持有帧引用的内存计费及 idle expiry；资产测试覆盖重复冻结不续期、重启复读、源身份、上限不驱逐、认证/JSON 限额和过期清理。
- 最终真机验收：本机 webcam → daemon 最新帧缓存 → HTTP → MCP stdio，返回 1920×1080 未标注 JPEG（483870 bytes），长度/SHA-256 匹配；按 ID 复读通过，测试 TTL 10 秒后无请求期间文件被自动删除，后续返回 `source_expired`。未知 camera 返回 `camera_not_found`；测试 daemon Ctrl-Break 优雅停机后新取帧返回 `daemon_unavailable`，历史查询继续可用。
- 新交付二进制位于 `target/frame-tools/debug/`，避免覆盖正在运行的 `target/debug/item-mcp.exe`；没有修改 ZCode 配置、重连 host 或停止用户进程。Linux/macOS 实机与 Rust 1.89 MSRV 未单独验证。

## 1. 目标与结论

将现有视觉物品记忆系统包装为一个独立的 MCP server，使 Claude Desktop、Cursor、其他 MCP host 或自建 agent 可以通过标准工具查询“物品最后出现在哪里”、读取证据和快照，并明确区分直接观测与推断假设。

推荐的首期形态：

- 新增根 workspace crate：`crates/item-mcp`，产出二进制 `item-mcp`。
- 首期只读，默认打开 SQLite 的 read-only/WAL 连接；不依赖 `item-ingest`，不加载 FFmpeg、摄像头、YOLO 或 VLM grounding native feature。
- 首传输优先实现 MCP `stdio`，让桌面 host 可以直接拉起进程；HTTP 传输作为第二阶段，在 MCP SDK/协议版本确定后再接入。
- MCP SDK 选用官方 Rust SDK `rmcp` 3.5.x（目标版本先按 `3.5` 记录）；P0 直接做目标版本的最小编译验证和 API 锁定，不再把“SDK 尚未确定”作为长期阻塞项。
- 查询逻辑优先复用 `item-query` 和 `item-core`；作为 MCP fork，可以直接把 CLI/Web 中有价值的查询聚合和 DTO 下沉到共享库，不必维持“只能新增适配层、不能改原 crate”的限制。
- 直接观测永远优先；`possibly_occluded_by`、`possibly_under`、`possibly_contained_in` 只能以“qualified hypothesis”返回，并携带证据、规则版本和可用性说明。

非目标：首期不通过 MCP 启停 daemon、不修改相机配置、不写 SQLite、不执行任意 SQL、不把 VLM 生成的自然语言当作事实、不重新实现检测或跟踪。

## 2. 当前状态核对

### 已有、可复用的能力

- `item-core` 已提供 `Store::open_read_only`、`recent`、geometry/event/depth 查询、快照引用和 schema 版本读取。持久化单位是去重后的 `Observation`，不是每帧 detection。
- `item-query` 已提供：
  - `keyword_of`：从自然语言问题提取查询词；
  - `candidates_for`：读取生命周期事件并生成保守的遮挡/覆盖候选；
  - `Priors`：可选的尺寸/容器先验；
  - `Candidate`、evidence、rule version 等可审计结构；
  - 可选的 OpenAI-compatible VLM 文案客户端。
- README 当前路线图和项目记忆显示，G1（geometry/event/depth）、G2（2-D candidates）、G3（relative depth）、G4（size/affordance）和 G5（Web evidence cards）均已落地；仍开放的是持久化推断、推断过期和真实场景验收。MCP 应消费这些已落地的只读能力，不把它们重新实现为 MCP 专属逻辑。
- `item-web` 已证明只读数据库、直接观测优先、证据卡片和 geometry snapshot 的数据形状，但它是浏览器展示层，不应成为 MCP 的依赖层。
- `item-ingest` 已负责常驻采集、事件、geometry、depth、快照和 retention；MCP 只读侧应容忍数据库尚未创建、schema 低于 evidence schema、快照文件缺失等情况。

### 尚不存在的能力

- P0/P1 已补上 MCP crate、stdio 协议入口和基础查询工具，P2 已补上 raw 图片读取；下面完整契约中的推断、其他图片类型/筛选、资源、提示词和 health 仍待实现。
- `item-web` 的 `/api/*` 是普通 HTTP JSON API，不是 MCP server，不能只改路由名就宣称完成 MCP。
- `.zcode/plans/` 中的历史方案主要是 G1–G5、snapshot mode 等底层功能计划；它们没有 MCP 生命周期、能力声明、错误契约、权限边界和兼容性验收。
- 原先根目录下的嵌套 `item-finders/` 副本已删除；当前 fork 的根 workspace 是唯一 source of truth，不再存在需要同步的嵌套仓库。

## 3. 首期产品契约

### 3.1 Server 启动参数

`item-mcp` 至少支持：

```text
item-mcp --db data/items.db
item-mcp --priors crates/item-query/priors.example.toml
item-mcp --snapshots-root data
```

建议支持环境变量作为部署默认值，但命令行优先级必须明确：

- `ITEM_DB`：SQLite 路径；
- `ITEM_PRIORS`：先验 TOML，可省略；
- `ITEM_SNAPSHOTS_ROOT`：相对 snapshot 引用的安全根目录；
- `ITEM_MCP_LOG` 或 `RUST_LOG`：日志级别。

stdio 模式下 stdout 只能输出 MCP 消息；日志必须写 stderr。当前 P1 在启动时固定数据库路径，缺库不阻止 MCP 初始化，但业务查询返回 database_unavailable，不能静默创建数据库。后续 priors 等配置错误应在启用对应能力时明确报错。

### 3.2 MCP tools（首期）

工具名使用稳定、动作导向的命名；返回结构化 JSON，同时提供适合模型阅读的简短 `text` 内容。具体 SDK 属性/宏以实施时锁定的 MCP Rust SDK 版本为准，以下是协议层契约，不绑定某个 crate 的 API 写法。

MCP server 不假设调用方一定是 VLM，也不需要探测调用方类型：对支持 image content 的 host，图片工具直接返回 MCP image content；文本模型或不支持图片的 host 至少得到同一批图片的 metadata、URI 和 provenance。这样由 host/model 自己决定是否进行视觉标注或推断。

#### `list_recorded_items`

用途：获取数据库当前实际记录过的物品名称，供模型在自然语言查询前发现可用 label。

输入建议：

```json
{
  "camera": "desk",
  "zone": "drawer",
  "limit": 200
}
```

返回每个记录 label 的摘要，而不把它描述成经过人工确认的物品目录：

```json
{
  "items": [
    {
      "label": "keys",
      "observation_count": 3,
      "last_seen": "...",
      "cameras": ["desk"],
      "zones": ["drawer", "frame"]
    }
  ],
  "source": "database_observation_labels",
  "note": "labels come from detector/webhook input and may contain false positives"
}
```

查询必须使用 Store 的参数化 typed API；不要让调用方传入 SQL、LIKE pattern 或任意表名。label 是 detector/webhook 的原始名称，可以存在误检、同义词或大小写差异，不能自动宣称是规范物品名。

#### `locate_item`

用途：查询一个物品的直接最后目击及可用推断。

输入：

```json
{
  "query": "where are my keys?",
  "label": "keys",
  "camera": "desk",
  "limit": 20,
  "include_hypotheses": true
}
```

规则：

- `label` 显式提供时优先使用；否则使用 `item_query::keyword_of(query)`。
- 返回 direct observations，按现有 `recent` 语义排序。
- `include_hypotheses=true` 时调用 `candidates_for`；候选窗口、上限和先验来源必须在结果中说明。
- 没有直接观测时不能用候选假设伪装成“找到”；返回空 direct list 和明确的 hypothesis/availability 状态。

返回核心字段：

```json
{
  "query": "where are my keys?",
  "keyword": "keys",
  "direct": [
    {
      "observation_id": 12,
      "label": "keys",
      "camera_id": "desk",
      "zone": "drawer",
      "first_seen": "...",
      "last_seen": "...",
      "hit_count": 8,
      "snapshot": {"available": true, "resource_uri": "item://snapshot/12"}
    }
  ],
  "hypotheses": [],
  "evidence": {"schema_version": 2, "rule_version": "...", "priors": "builtin"},
  "availability": {"direct": "ok", "hypotheses": "ok", "note": null}
}
```

#### `list_observations`

用途：按 label/camera/zone 和时间范围读取观测记录，供 agent 做复核。

输入建议：`label`, `camera`, `zone`, `since`, `until`, `limit`。首期可以先实现现有 `recent` 能力，再补 Store 的 camera/zone/time 查询 API；不得通过拼接 SQL 暴露任意过滤语句。

#### `get_observation`

用途：读取一个 observation 的完整可审计资料：基本字段、bounded geometry、lifecycle events、depth evidence、快照可用性。

输入：`observation_id`。不存在时返回明确的 not-found tool error，不返回空成功结果。

#### `get_item_images`（raw observation/asset 读取已实现，其余为后续目标）

用途：按 label 或 observation/geometry id 获取图片内容，供调用方 VLM 观察；该读取工具不做视觉判断、不重新标注图片、不执行写入。保存调用方标注由独立写入工具转交 daemon，见 §4.1。

接口必须明确图片类型，不能用一个含义模糊的“snapshot”同时代表原图和带框图。建议输入：

```json
{
  "label": "keys",
  "observation_ids": [12, 18],
  "geometry_ids": [],
  "image_kind": "raw",
  "max_images": 4,
  "max_bytes_each": 4000000
}
```

`image_kind` 只能是：

- `raw`：未经过 detector/zone/label 绘制的原始画面，返回 MCP image content（base64/blob + `image/jpeg`）和结构化 metadata；
- `annotated_evidence`：已有 ingest 标注框、区域或 observation 高亮的证据图，明确标记为 annotated evidence。

规则：

- `label`、`observation_ids`、`geometry_ids` 至少提供一种筛选条件；按 ID 获取时必须先通过数据库关系校验，不能读取任意路径。
- 请求 `raw` 时只能返回 raw；如果 raw 不存在，返回 `raw_unavailable`，绝不能静默回退到 annotated evidence。
- 请求 `annotated_evidence` 时才允许返回现有带框 snapshot；不能把现有 `sample_snapshot` 自动称作 raw。
- 结果必须限制图片数量、单图大小和总响应大小，并按 observation/geometry 的时间和 ID 稳定排序；超限时返回截断说明。
- host 不支持 image content 时，仍返回受控 `item://snapshot/...` resource URI、图片类型、时间、camera、zone、observation id 和可用性状态。
- 返回中携带 `image_kind`、capture time、source/provenance 和 observation/geometry id，让调用方能区分 `database_fact`、`annotated_evidence` 与后续的 `visual_inference`。

图片契约与下文 §4.1/§4.2 一致：daemon 负责采集和图片资产持久化，MCP 经本机 loopback HTTP 读取图片。raw 的代表帧策略、持久 manifest、读取接口和清理行为已在 API v1 定稿；提交标注、共享帧资产及派生图的表结构/retention 仍待实施设计。

#### `get_evidence`

用途：读取一个 candidate/hypothesis 的证据链。

输入至少包括 target observation/event id、cover event id 或 candidate identity。返回关系、分数（注明 heuristic）、rule version、每项 supporting/conflicting/unknown evidence、geometry id、depth provenance 和快照资源引用。

如果首期不希望暴露 candidate identity，可先让 `locate_item` 内嵌 evidence，并将 `get_evidence` 放在第二个里程碑；但不要返回只有一句不可审计的 VLM 文案。

#### `get_health`

用途：读取 ingest health snapshot，帮助 host 判断数据是否新鲜。

输入：无，或可选 health file path（更推荐只使用 server 启动时配置的固定路径）。返回更新时间、daemon/camera state、last frame age、last error；stale health 必须标为 stale，而不是报告“相机正常”。

### 3.3 MCP resources

首期实现快照资源，避免把本地路径直接交给模型。图片资源与 `get_item_images` 的 image content 是同一份受控文件引用；tool 适合模型按需取图，resource 适合 host 展示、缓存或后续引用：

- `item://observation/{id}`：观测 JSON；
- `item://geometry/{id}`：geometry JSON；
- `item://snapshot/observation/{id}`：观测代表性 JPEG；
- `item://snapshot/geometry/{id}`：geometry 对应 JPEG；
- 可选 `item://health`：最近一次 health JSON。

资源读取必须：

1. 只接受数据库中已存在的 observation/geometry/snapshot ref；
2. 将相对路径解析到 `--snapshots-root`，拒绝 `..` 逃逸、任意绝对路径（除非显式允许且通过根目录校验）和 `://` 外部 URI；
3. 文件不存在时返回可区分的 not-found/stale 结果；
4. MIME 类型正确，JPEG 作为 binary/blob 内容返回；
5. 不把 `frigate://` 当作本地文件，也不尝试从外部 URL 拉取。

### 3.4 MCP prompts（建议首期提供）

工具提示词用于编排，而不是替代事实校验。建议提供：

- `where_is_item`：先调用 `list_recorded_items`；如果用户提到的名称不是精确 label，再选择最接近的已记录 label 并向用户说明；随后调用 `locate_item`；若需要视觉复核，再调用 `get_item_images` 获取 raw images。回答时先给数据库 direct observation，再给 VLM 对图片的标注/推断。
- `inspect_recorded_scene`：调用 `list_recorded_items` 后按 label/最近 observation 获取有限数量 raw images，让调用方 VLM 输出可见物体清单和空间关系；每个视觉结论标记为 `visual_inference`。需要保存时通过独立提交工具交由 daemon 存为派生标注，不能写回 observation 或伪装成直接目击。
- `explain_item_evidence`：读取 observation/evidence 并要求模型区分 database fact、annotated evidence 和 visual inference。

推荐的提示词顺序：

```text
1. list_recorded_items —— 发现服务端实际出现过的原始 labels；
2. locate_item —— 获取数据库直接目击和可审计 hypothesis；
3. get_item_images(image_kind=raw) —— 需要视觉判断时取得未标注图片；
4. 由 ZCode 等调用方 VLM 查看图片、生成结构化标注/推断，必要时编辑派生图片；
5. 需要保存时，通过明确的标注提交工具把结果交给 daemon，等待持久化回执；不直接修改 observation 或覆盖原图；
6. 回答时区分 database_fact、detector annotated_evidence 与调用方 visual_inference，并标明结果是否已保存。
```

Prompt 只是交互模板，不能替代 server 的事实边界；但与“先列出已记录名称、再取图”的 tool contract 一起提供时，可显著减少模型猜测不存在的 label。

## 4. 架构与依赖边界

建议依赖方向：

```text
item-mcp ──> item-query ──> item-core
item-ingest ───────────────> item-core
item-web ──> item-query ──> item-core
```

`item-mcp` 不依赖 `item-ingest` 或 `item-web`。建议拆成以下模块：

- `config.rs`：CLI/env 合并、路径解析、只读模式和安全根目录；
- `server.rs`：MCP initialize/capabilities、tool/resource/prompt 注册；
- `state.rs`：只读 `Store`、`Priors`、snapshot resolver、health reader；
- `tools.rs`：输入 schema、调用 item-query、输出 DTO 和 image content；
- `inventory.rs`：`list_recorded_items` 的 typed label/camera/zone 聚合查询；
- `resources.rs`：URI 解析和受限文件读取；
- `error.rs`：稳定的错误类别与内部日志上下文；
- `main.rs`：stdio 启动和 tracing stderr 配置。

不要把 `Store` 长时间放在跨请求的可变 mutex 中。只读 SQLite 可以为每个请求打开短生命周期连接，或在确认 rusqlite `Connection`/WAL 并发约束后使用受控共享；工具执行期间不得持有写锁。MCP handler 的 async 边界不能直接阻塞 Tokio worker，必要时使用 `spawn_blocking` 包装 SQLite/纯 CPU 查询。

### 4.1 调用方 VLM 标注与 daemon 持久化（已确认方向，尚未实现）

#### 职责与运行生命周期

这里的 VLM 指 ZCode 等 MCP host 中的调用方模型，例如能够查看图片、输出标注并编辑图片的对话 agent。这个功能不要求项目自行调用另一个 VLM，也不引入服务端 annotation worker、模型推理任务队列或替调用方恢复未提交对话的机制。原项目已有的 VLM detector/sidecar 路径不因此被移除，但它与本功能的调用方标注来源必须区分。

已确认：**由 daemon 统一负责原图、已有检测证据以及调用方提交标注/派生图片的持久化和生命周期管理。** MCP 保持接入层角色，stdio 进程可以随 host 启停，不作为第二个写库进程，也无需为保存标注而改为始终在线的 MCP 服务。

```text
item-ingest daemon                         ZCode / 调用方 VLM
  摄像头/Frigate → 观测、raw、检测证据 ── MCP 读取工具 ──> 看图、标注、推断、编辑
  标注校验与持久化、版本、清理       <── MCP 提交工具 ── 结构化结果 / 派生图片
```

- daemon 独立运行，host/MCP 断开不应停止采集或清理；已确认保存的数据在 daemon/MCP/host 重启后仍可查询。
- MCP 的现有只读 SQLite 查询可以保留；未来所有写请求通过 daemon 的受限内部接口处理，不通过 MCP 直接执行 SQL 或写资产目录。
- daemon 内部接口与对 host 的 MCP transport 是两个边界。已选择本机 loopback HTTP 作为第一版内部通信方式，见 §4.2；这不要求对 host 的 MCP transport 改为 Streamable HTTP。
- daemon 接口负责鉴权、输入/归属校验、幂等、版本冲突和持久化回执；调用方只提供 asset/annotation ID、声明式标注及受限图片内容，不指定服务端路径、SQL 或命令。
- 标注保存失败不能阻塞相机采集。大图解析、校验和文件写入不应长期持有采集的 Store 锁。

#### 读图与提交协议

读取图片时，除了现有 camera/time/observation/geometry 字段，还应返回稳定的 `asset_id`、内容 hash、图片尺寸和 MIME 类型。这些字段把调用方实际看到的版本与提交结果绑定；geometry 中可变的 last sample 或本地文件路径不能充当稳定图片身份。

建议后续 MCP 工具（名称与字段尚待实施时定稿）：

- `submit_image_annotation`：提交源 asset ID/hash、结构化标注、来源说明和幂等键；可附编辑后的派生图片内容。MCP 转交 daemon，不自行运行视觉推理。
- `get_image_annotation` / `list_image_annotations`：读取已持久化标注及其版本、来源、源资产状态。
- 修订已有标注时创建新 revision，并传 `expected_revision`；不原地覆盖原始提交，不覆盖原图或 detector evidence。

写工具与读取工具分别声明权限；保存派生标注是明确的业务写操作。启用/授权方式在实现阶段确定，不能因为调用方能读图就默认拥有任意编辑权限。

一次提交的基本流程：

1. host 获取 raw asset，并在当前会话里完成标注或编辑。
2. host 用独立提交工具发送结果及 `asset_id`/hash/幂等键。
3. daemon 检查资产仍有效、hash 匹配、引用属于同一源、结果 schema/坐标/图片尺寸及大小合法。
4. daemon 完成持久化后返回 annotation ID、revision、服务端时间和 `persisted` 状态；仅收到请求或内存排队不能报告保存成功。
5. 回执因断线丢失时，调用方可用同一幂等键重试，获得同一持久化结果；同键不同 payload 返回冲突。

MCP/host 断线前尚未提交的内容属于 host 会话，daemon 不负责恢复。daemon 不可用时写工具明确返回不可用，不在 MCP 内建立隐藏的本地离线队列，也不降级成直接写数据库。

#### 数据来源与版本

- raw asset 保留未画框的视觉证据；detector annotated evidence 和调用方编辑图片均为独立派生资产，关联源图但不覆盖它。
- 调用方新识别的 label、bbox、空间关系和推断存为 `visual_inference`/annotation，不能自动提升为直接 Observation，也不能混进 `list_recorded_items` 的 detector/webhook label 集合。
- 持久化元数据至少包含 annotation ID/revision、源 asset ID/hash、关联 observation/geometry、结果 schema 版本、提交来源、模型/prompt 版本（若可提供）、服务端保存时间、幂等键及派生资产引用。
- 区分服务端实际验证的 asset hash/时间与调用方声明的 model/prompt/作者；后者未验证时保持 unknown 或 caller-reported，不伪造可信身份或置信度。
- bbox 必须携带坐标约定并对应源图尺寸；经过裁剪/缩放/编辑的图像记录变换和派生关系，不能把其坐标直接套回 raw frame。

#### 生命周期约束与待定参数

生命周期管理归 daemon，内部传输已选定本机 loopback HTTP；具体表结构、同库或独立标注库、保存间隔/配额/retention 时长仍待实施设计。以下约束适用于两种存储布局：

- 原图、检测证据、派生图片和标注分类型计数、版本化关联；同帧多个 observation 可共享资产，不能删一条观测就误删其他引用仍依赖的文件。
- 正在提交时遇到 retention 删除，daemon 必须给出确定结果：提交事务/资产发布成功，或返回 source_expired/conflict；不能保存一个悬空成功引用。
- 文件和数据库不能靠一次 SQLite 事务原子提交。需要临时文件、完整性校验、发布/引用顺序以及崩溃后的对账机制；未完成文件不作为可读资产，失败残留由 daemon 清理。
- 源图被清理或丢失后，若保留标注，则显式标记 `source_asset_missing/expired`，查询不能继续声称原图可复核。若选择联动删除，应同时清理标注/派生图引用；保留还是联动删除及其期限尚未定稿。
- 新模型/新 prompt/人工修订形成新版本；旧标注是否长期保留、如何失效和清理要有统一策略，不用覆盖旧记录代替生命周期管理。
- daemon 重启后可识别完整保存、未完成和待清理数据；派生数据存在并不要求 MCP session 或 host 对话始终存活。

### 4.2 MCP 与 daemon 的内部通信（本机 loopback HTTP；raw 只读子集已实现）

第一版采用 **本机 loopback HTTP**：daemon 暴露受限业务 API，MCP 使用 HTTP 客户端访问。复用现有 axum/reqwest，Windows、Linux、macOS 使用同一种交互方式；不同时实现 Unix Domain Socket 和 Windows Named Pipe，也不把第一版定位为跨机器部署。

```text
ZCode / 调用方 VLM
    │ MCP stdio
    ▼
item-mcp
    ├─ 基础历史查询 ──> SQLite read-only（现有路径保留）
    └─ 资产/标注请求 ──> loopback HTTP ──> daemon ──> 元数据与图片资产
```

#### 监听、认证与配置

- 使用独立、可配置的内部监听地址，未配置则关闭；配置示例为 `127.0.0.1:8480`。需要 IPv6 loopback 时显式配置 `[::1]:8480`，拒绝 `0.0.0.0` 或 LAN 地址。
- 内部 API 与现有 Frigate webhook/preview 的监听器分开；不能因 webhook 需要对外接入而顺带暴露资产及标注写接口。
- 第一版使用本机共享 token 对请求认证，例如 Authorization bearer header；loopback 只限制网络可达范围，不能代替调用方认证。token 不放入 URL、不提交到仓库、不写日志；具体配置字段在实施阶段确定。
- MCP 配置 daemon endpoint、认证信息和请求超时；HTTP/文件上传的单次大小、总响应量和并发数均有限制。浏览器跨域不默认放开，校验 Host/Origin 等请求边界。
- 远程 daemon 不在第一版支持范围内；以后需单独增加 HTTPS/认证部署方案，并迁移仍直读 SQLite 的基础查询，不能只改 endpoint 就宣称支持跨机器运行。

#### 内部业务接口（raw 路径已在 API v1 定稿，annotation 路径仍为示意）

| 接口 | 职责 |
| --- | --- |
| `GET /internal/v1/assets/{id}` | 返回类型、尺寸、MIME、内容 hash、来源关系和可用状态 |
| `GET /internal/v1/assets/{id}/content` | 返回授权访问的图片二进制内容 |
| `POST /internal/v1/annotations` | 校验并持久化调用方标注，必要时关联派生图片 |
| `GET /internal/v1/annotations/{id}` | 查询保存结果、revision 及源资产状态 |

- daemon 不必实现 MCP 协议。内部 API 只表达 asset/annotation 业务，MCP adapter 负责转换为 tools/resources/image content；内部 API 版本与 MCP protocol version 分别管理。
- 图片下载使用二进制响应，派生图片使用受限上传；不要求内部 HTTP 全程 base64。MCP 读取实际图片字节后在 host 边界构造 image content。
- 接口只接受服务端资产/标注 ID 和受限数据，不接受任意磁盘路径、SQL 或命令；所有写请求都由 daemon 执行。
- 请求带幂等键和版本约束；重复同 payload 返回同一结果，同键不同 payload 或旧 revision 返回冲突。daemon 完成持久化后才发成功回执。
- daemon 已保存但回执丢失时，MCP 可携同一幂等键重试；不能生成新键盲目重放。读取与写入分别设置超时和错误映射。
- daemon 返回稳定业务错误，例如 source_expired、revision_conflict、invalid_input、unauthorized、unavailable；具体 HTTP status 和 JSON schema 在实施阶段统一定义，MCP 再映射到工具错误，不直接暴露内部堆栈/本机路径。
- HTTP 图片解析/校验/写入不阻塞相机采集；数据库锁仅覆盖短事务，文件与数据库一致性沿用 §4.1 的发布/对账约束。

#### 渐进迁移与不可用语义

保留当前只读 SQLite 基础查询，不因增加 HTTP 写接口而一次性重写所有查询。新图片和标注能力走 daemon API；daemon 停止时，已有历史基础查询在 SQLite 可读的条件下仍可用，资产读取与标注提交则明确返回 daemon_unavailable，不伪造空结果或保存成功，也不回退到 MCP 直接写入。

业务层围绕资产和标注契约实现，不把 TCP 细节散落到工具逻辑。未来如需操作系统级权限隔离，可增加 Unix Socket/Named Pipe adapter；这属于后续传输扩展，不改变 daemon 的持久化所有权。

## 5. 传输策略

### Milestone M1：stdio

- 目标：能被桌面 MCP host 以 command + args 拉起。
- stdout 严格只发 MCP protocol 消息，stderr 记录诊断日志。
- 用内存 SQLite fixture 做 initialize、tools/list、tools/call、resources/read 的端到端测试。
- 不开启监听端口，默认不接受网络输入。

### Milestone M2：Streamable HTTP

在 M1 契约稳定后再实现 HTTP：

- 复用同一组 handler/DTO，不复制业务逻辑；
- 默认绑定 `127.0.0.1`，显式 `--listen` 才改变；
- 明确 session、并发、超时、请求体大小和 CORS 策略；
- 若对外暴露，必须增加认证/反向代理说明；不能把无认证的 MCP HTTP 默认绑定到 `0.0.0.0`；
- SSE 仅在选定 SDK/host 仍需要且协议版本明确时增加，不把多种传输同时作为首期目标。

P0 直接以 `rmcp 3.5.x` 做一个小型 SDK spike：验证 protocol version、stdio transport、typed tool schema、resource content API，以及当前项目 Rust toolchain 的编译要求，并把最终补丁版本写入 `Cargo.toml`/`Cargo.lock`。如果个别 API 与预期不同，应调整 `item-mcp`/共享 query 层，而不是回退到自研 JSON-RPC；只有目标版本无法在项目支持的 toolchain 上构建时，才重新评估兼容版本。

## 6. 错误、事实边界与安全策略

### 稳定错误类别

至少区分：

- `invalid_input`：limit、时间范围、ID、URI 不合法；
- `database_unavailable`：文件缺失、锁/SQLite 读取失败；
- `schema_unavailable`：数据库低于 geometry/event 所需 schema；
- `not_found`：观测、geometry 或快照不存在；
- `snapshot_unavailable`：数据库引用存在但文件已删除/过期；
- `internal`：未预期错误，细节写 stderr，返回不泄漏本机路径/凭证。

工具错误应让 host 能决定重试与否；“没有匹配”是成功的空结果，不是内部错误。

### 不可信输入与输出

label、camera、zone、detector label、Frigate payload 和 VLM 文本都视为不可信数据：

- 结构化输出中保留原值，但文本摘要要做长度上限和控制字符处理；
- 不把数据库值拼入 SQL；使用现有 Store 参数化查询或新增 typed API；
- 不执行用户提供的路径/SQL/命令；
- VLM 若用于 wording，必须作为可选增强，不能创建不存在的 observation、geometry、relation 或数值；
- 结果携带 `generated_by`/`source` 或等价 provenance，让 host 看得出 direct vs derived vs VLM wording。

### 标注写入与管理操作边界

已实现的 P0/P1 工具仍然只读。未来的调用方标注提交按照 §4.1 交由 daemon 持久化，MCP 不直接写 SQLite 或资产目录；需明确写工具启用/授权语义、审计、幂等与冲突处理，具体开关名称尚未定稿。

保存标注不等于允许启停 daemon、修改相机配置、触发采集或执行系统命令；这些管理操作仍不在当前已确认范围内。

## 7. 分阶段实施计划

### P0：契约和 SDK spike（已实现）

1. 确认 MCP protocol/SDK 版本与传输 API；新增 `crates/item-mcp` 的最小骨架。
2. 先实现 initialize、tools/list、一个静态 ping/read-only capability，验证 stdio 与 host 兼容。
3. 增加根 README 的启动示例和 MCP 配置片段。

验收：`cargo check --workspace`、MCP host 可以启动进程并完成 initialize；stdout 无 tracing 污染。

### P1：直接观测查询与 label inventory（已实现）

1. 实现 `list_recorded_items`、仅直接目击的 `locate_item`、`list_observations`、basic `get_observation`。
2. 为 distinct recorded labels 增加 typed Store/query API，并返回 observation_count、last_seen、camera/zone 摘要。
3. 复用 item-query 的 keyword_of，新增共享 Store DTO/query，不复制 CLI main。
4. 为 camera/zone/time filter 补 typed Store read API；保留 limit 上限。
5. 锁定缺库、空库、schema 旧库、数据库 busy、非法输入的错误语义。

验收：内存/临时 SQLite fixture 的 protocol-level tools/call 测试；结果与 `item-query log/ask` 的 direct 部分一致。

### P2：假设、证据、图片与资源（接口先行）

1. 接入 `candidates_for`、Priors 和 evidence DTO；实现 `include_hypotheses`。
2. 实现 `get_evidence`（或明确将它合并到 `locate_item` 并记录取舍）。
3. 按已记录的 `image_kind=raw|annotated_evidence` 契约实现 `get_item_images`；先解决 raw 证据的采集/归属/持久化设计，再接入 MCP image content。
4. 实现 observation/geometry/snapshot resources 和路径逃逸测试。
5. 缺 snapshot、raw image、`frigate://`、schema v1、unknown depth、target reappearance 等场景都返回可解释状态。
6. 依照 §4.1/§4.2，由 daemon 管理 raw/annotated/调用方派生资产，经本机 loopback HTTP 提供内部 API；表结构、保存策略、API 字段和 retention 参数在实施前定稿。

验收：用 `item-web/examples/evidence_demo.rs` 或等价 fixture 验证 direct → hypothesis → evidence 的顺序和字段；禁止把 heuristic score 称作 probability。

### P3：运维可见性与发布

1. 实现 `get_health`，明确 stale 判定和 health 文件不存在语义。
2. 加入 stdio host 配置样例、Windows/Linux/macOS 启动说明和日志排障说明。
3. 将 `item-mcp` 加入 CI 的 workspace check/test/clippy；默认构建不得拉入 FFmpeg/native camera features。
4. 评估并实现 Streamable HTTP；默认 loopback，补认证/超时/并发/请求大小测试。

验收：默认 workspace 全绿；至少一个真实 MCP host 完成 initialize、find、resource read；重启 ingest/缺失快照时 MCP 仍能给出诚实结果。

### P4：调用方标注提交与 daemon 持久化（方向已确认，尚未实现）

1. 在 daemon 实现独立监听、带认证的 loopback HTTP 内部 API（§4.2），统一管理原图、结构化标注和派生图片；先完成 schema、资产身份、幂等、并发及清理契约。
2. MCP 注册标注提交/读取工具，把调用方 VLM 的结果转交 daemon；不引入服务端 VLM worker，不直接写数据库。
3. 实现幂等键、revision 冲突、保存回执、源资产过期处理与崩溃对账；MCP/host/daemon 重启后已保存结果仍可查询。
4. 配合 prompts 区分直接目击、detector evidence、调用方 visual_inference 与是否已持久化。

daemon 控制和通用自动化不随标注写入一起授权，继续作为独立的后续需求。

## 8. 测试矩阵

- **协议**：initialize、capabilities、tools/list、resources/list/read、未知 tool/resource、malformed args、并发请求、stdio stderr/stdout 隔离。
- **查询**：空库、单 observation、多 camera/zone、distinct label inventory、limit 边界、特殊 label、旧 schema、SQLite busy/损坏文件。
- **推理**：无事件、遮挡候选、containment priors、unknown/conflicting depth、重复候选、窗口外 reappearance；结果必须保留 evidence ids 和 rule version。
- **图片内容**：支持 image content 的 host 能收到 raw JPEG；raw 与 annotated provenance 不混淆；图片数量/单图/总响应大小受限；无 raw 文件时不回退伪装成 raw。
- **资源安全**：绝对路径、`..`、符号链接/重解析点（按平台策略）、外部 URI、缺失文件、超大文件、非 JPEG 引用。
- **事实边界**：无 VLM 时仍有确定性 direct/evidence；调用方对 raw image 的新标注/推断只能经提交工具由 daemon 存为独立 visual_inference，不能覆盖原图、直接目击或 detector/webhook 标签集合。
- **标注生命周期**：提交成功后 host/MCP/daemon 重启仍可读；回执丢失后同幂等键重试不重复写入；同键异内容/旧 revision 返回冲突；源图过期与提交并发、共享资产引用、文件发布中崩溃及对账、无 daemon 时拒绝隐藏离线写入。
- **内部通信**：内部 listener 与 webhook/preview 隔离；仅 loopback、正确/错误/缺失 token、Host/Origin、请求大小/并发限制、HTTP 超时与回执丢失重试均覆盖；daemon 不可用时基础 SQLite 查询仍可读，资产/写工具明确失败。
- **回归**：与 `item-query`/`item-web` 共享 fixture，确保 MCP 与现有 CLI/API 对同一数据库给出一致 direct observation。
- **真实路径**：先用 `item-web` evidence demo 和本地回环/内置摄像头产出的数据库，再做一次 MCP host 黑盒测试；不把仅有单元测试当成摄像头路径验证。

## 9. 文档与历史方案修正清单

1. 在根 `README.md` 增加 MCP crate、stdio 启动示例、只读边界、`list_recorded_items`/`get_item_images` 和当前支持的 tools/resources；不要把 MCP 混入 resident ingest 或 G4/G5 的完成状态。
2. 在本文件维护 MCP 契约版本和变更记录；历史 `.zcode/plans/` 仅作为过程记录，不再作为当前路线图。
3. 将“G1–G5 已完成/仍开放”的状态继续以 `README.md` 与 `docs/geometry-containment.md` 为准；MCP 只是消费这些能力，不改变其推理免责声明。
4. 已删除原先的嵌套 `item-finders/` 副本；后续只维护当前 fork 根 workspace，避免再次引入嵌套仓库。
5. 当前 fork 允许 MCP 实现直接修改 ingest/core/query/web 的共享边界；但每个 change 仍应说明为何需要改动，并保持 MCP 只读工具不意外取得写入权限。不要为了形式上的隔离保留重复 DTO 或重复查询实现。

## 10. 推荐的第一批文件变更

```text
Cargo.toml                         # 加入 crates/item-mcp，并声明 MCP SDK 依赖
Cargo.lock                         # 锁定 SDK/传输依赖
crates/item-mcp/Cargo.toml
crates/item-mcp/src/main.rs
crates/item-mcp/src/config.rs
crates/item-mcp/src/server.rs
crates/item-mcp/src/state.rs
crates/item-mcp/src/tools.rs
crates/item-mcp/src/inventory.rs
crates/item-mcp/src/resources.rs
crates/item-mcp/src/error.rs
crates/item-mcp/tests/protocol.rs
docs/mcp-server-plan.md             # 本文
README.md                           # M1 完成后补充使用说明
```

首个实现 change 建议以 P0/P1 为主，但不排斥为共享 query service、DTO 或现有 API 做必要重构；只要这些改动服务于 MCP fork 的统一契约，就不必人为维持上游的重复路径。仍不建议在首个 change 同时引入无关的 native feature 或管理型写操作。
