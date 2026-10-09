# 当前已知限制与缺陷

核对日期：2026-10-11。本文说明当前使用边界与数据模型缺口，不表示这些问题已修复。阅读结果时应保留歧义，而不是用猜测补齐缺失证据。安装、工具参数与维护步骤分别见 [入门](getting-started.md)、[MCP 使用](mcp-usage.md) 和 [运维](operations.md)。

## 同类物品与实例身份

### 1. `list_recorded_items` 返回标签目录，不是物品实例清单

- 实际查询对筛选后的 `observations` 按原始 `label` 聚合；同标签只有一个汇总条目，保留原始拼写和大小写，不做类别归一化。
- 每条返回 `label`、`observation_count`、`last_seen`、`cameras`、`zones`，没有稳定实体 ID，也没有逐物品外观、边界框或身份关联。
- `observation_count` 是 Observation 记录数，不是物品数量，不是帧检测次数；同一个物品可能贡献多条记录，多个同类物品也可能合并为一条记录。
- `last_seen` 是该标签在匹配记录中的最近历史目击时间，不是当前可见状态；camera/zone 集合也不是每一件物品的当前所在位置。

**影响：** 两个杯子不能从该工具的结果中区分；不能把返回的 `items` 当作家庭物品台账，或用 `observation_count` 推断场景中有几个杯子。工具名称及 `items` 字段容易让调用方误解其语义，当前以工具说明/文档约束使用。

**代码依据：** `crates/core/src/store.rs` 的 `Store::recorded_items` 使用 `GROUP BY label`；MCP 返回结构定义于 `crates/item-mcp/src/query.rs` 的 `RecordedItemJson`/`InventoryResponse`。

### 2. Observation 去重没有物品实例身份

- 当前按 `(camera_id, zone, label)` 和时间窗口匹配/合并 Observation，默认去重窗口 300 秒；不是按稳定物品实体或 tracking ID 匹配。
- 同一 camera/zone 中的多个同类物品，在符合时间合并条件时可能共享一条 Observation；仅凭 observation_id、hit_count 或行数无法恢复物品数量和逐实例历史。

**影响：** 历史记录能说明“这个区域记录过该标签”，但不能可靠回答“具体是哪一个杯子”“该杯子是否移动到另一位置”。`locate_item` 等直接观测查询不能据同标签自动建立同一物品身份，也不能保证多候选已经按实例拆分。

**代码依据：** `crates/core/src/store.rs` 的 `DEFAULT_DEDUP_WINDOW` 和 sighting 合并查询。

### 3. 标注 `object_id` 只在单图标注集及其 revision 内有意义

- 一张图片可以提交多个相同 label 的对象，通过不同 `object_id` 和 bbox 区分，这是初版已支持的能力，不是同图数组去重缺陷。
- `object_id` 不提供跨标注集、跨帧或跨 camera 的全局身份；两张图使用相同字符串不自动建立关联。
- 同图 revision 保留 object_id 表示调用方对同一物体的身份声明，daemon 不验证视觉身份或物体匹配正确性。
- 没有用户确认的物品实体目录，也没有跨图片实体关联/重识别接口。颜色、外观和参照物可以暂存在 `note`，但不是结构化、可查询的身份属性。

**影响：** 能分别保存“这一张图里的两个杯子”，不能据此追踪“我的蓝色杯子”在后续图片中的位置。遇到多个无法区分的候选应保留歧义，不能任选一条当作用户指定物品。

### 4. 标注物体尚未接入寻物查询

- `list_recorded_items`、`list_observations` 和 `locate_item` 仍查询直接 Observation，不检索调用方 annotation 中的 label/描述。
- `list_image_annotations` 返回标注集摘要并支持精确 camera/source 筛选，不是逐物体清单，也不提供 annotation label/外观搜索；完整对象数组需通过 `get_image_annotation` 读取。
- 用户显式保存的新识别物体不会自动出现在 recorded label 目录或现有寻物结果中。这是保持直接观测与 visual inference 分离的既有边界，但当前缺少独立的标注检索入口。

**影响：** 标注保存/复读闭环已具备，但“保存识别结果后按物品名称寻找”尚未形成完整产品闭环。

## 证据、假设与运行范围

- **历史与当前不同**：last_seen、保存的raw图、frame capture_time描述过去的记录。最近一张图不保证请求结束时物品仍在原地。
- **几何不是物理定位**：pixel bbox、ordinal depth、配置的尺寸先验不提供房间坐标或单目米制重建。`possibly_*`与heuristic score不能升级成“已确认inside”。
- **歧义标志覆盖有限**：`same_label_ambiguous`主要反映规则所比较的target/cover标签相同；false不证明场景中只有一个目标，也不解决Observation已合并实例的问题。
- **读取范围有界**：context和hypotheses有数量/字节/时间预算，truncated/partial意味着证据不完整；schema_unavailable与完整扫描无候选不同。空结果不是不存在证明。
- **旧数据不会自动补证据**：历史库缺少raw、geometry、lifecycle或depth时，reader不迁移/重建事实。Frigate引用不是本地raw JPEG；离散webhook不能证明连续帧disappearance。
- **几何源图未精确登记**：context/hypotheses不核验source assets。Observation级raw不能自动当作某event-boundary geometry的同帧原图。
- **CLI/Web与MCP候选不同**：共用基本算法，但MCP新facade增加最新anchor、完整性及后续生命周期gate；不同窗口/接口的score与候选集不保证一致。CLI的limit也不是MCP候选输出条数。
- **模型与硬件相关**：检测/grounding可能误检或漏检，VLM词汇开放不代表准确识别。平台服务模板、原生feature、模型格式和性能需要在实际机器独立验证。
- **运维会删除数据**：retention/maintenance和asset expiry不可当作只读操作。当前snapshot reconcile不能保证删除所有带事件/检测后缀的文件；应监测存储增长，不自动删正在使用的数据。

仍不提供跨摄像头实体追踪、标注物体名称搜索、持久化空间推断、MCP resources/prompts或daemon/camera管理工具。

## 后续设计约束（未实施）

- 分开建模类别标签、单图对象和用户确认的具体物品实体，不把 label 或当前左右位置当作永久身份。
- 如引入标注物体检索，保持独立来源说明及 evidence 引用，不静默并入 detector/webhook 的直接目击集合。
- 跨帧/跨 camera 匹配应显式携带证据和不确定性；没有足够依据时返回多候选或身份不确定，而不是宣称已确认同一物品。
- 修复顺序、接口名称、迁移方案和验收标准尚未决定；后续实施需另行授权。

相关使用说明：[MCP 工具与标注](mcp-usage.md)、[采集流程与结果解释](detection-flow.md)、[常驻运行与维护](operations.md)。设计文件仅存于本机忽略目录，不是使用本文所需的依赖。
