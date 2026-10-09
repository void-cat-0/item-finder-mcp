# VLM grounding sidecar 使用指南

`item-ingest --detector vlm` 将图片发送给外部多模态服务，取得 label 和 bounding box。项目不内置该模型，也不负责下载模型或启动推理服务。本文描述源码 `detector/vlm.rs` 实际发送和解析的协议，不保证任意号称 OpenAI-compatible 的服务都满足 grounding 要求。

相关入口：[开始使用](getting-started.md) · [运行与维护](operations.md) · [MCP 使用](mcp-usage.md) · [检测流程](detection-flow.md) · [已知限制](known-limitations.md)。

## 1. 先准备兼容服务

服务至少需要同时满足：

1. 提供 `POST {base_url}/chat/completions`，支持 OpenAI 风格 `messages` 和多模态 `image_url` 内容。
2. 接受 `data:image/jpeg;base64,...`，不只是图片 URL 或纯文本。
3. 模型本身具有图像理解与 grounding 能力，能按提示返回 JSON 检测数组；普通文本模型不能替代。
4. 返回 `choices[0].message.content` 字符串，其中包含检测数组。
5. 允许客户端的请求大小、推理耗时与并发规模；只通过文本问候不足以证明 grounding 可用。

可选择支持相应模型的 llama.cpp、vLLM、Ollama 等服务，但应按**所用服务与模型版本**的官方文档准备模型文件、视觉组件、依赖和运行设备。不是只填一个模型名称就自动部署，也不要求固定 GPU/NPU 后端。

若使用 llama.cpp，通常需匹配的多模态模型与视觉 projector。`llama-server` 的模型、`--mmproj`、host/port、设备和 image-token 等选项随版本变化，**以已安装版本的 `llama-server --help` 和模型说明为准**。这里不提供未经核验的服务启动或下载脚本，也不承诺某种量化或硬件的速度、召回率。

客户端当前没有 API-key/token 配置，也不主动发送 `Authorization`。需要鉴权的服务必须通过受控网关等方式解决；不能仅因为支持 `image_url` 就宣称云 API 可直接连接。图片会离开 ingest 进程，远程 endpoint 的隐私、访问权限和保留策略需自行确认。

## 2. 配置 endpoint 与模型

以下为 Bash/Git Bash 示例；模型名必须换成服务真实接受的 ID：

```sh
export ITEM_VLM_BASE_URL="http://127.0.0.1:8080/v1"
export ITEM_VLM_MODEL="your-grounding-model"
```

PowerShell 对应设置方式：

```powershell
$env:ITEM_VLM_BASE_URL = 'http://127.0.0.1:8080/v1'
$env:ITEM_VLM_MODEL = 'your-grounding-model'
```

客户端在 base URL 后追加 `/chat/completions`，去掉尾部 `/`；不要把完整 `/chat/completions` 路径填成 base URL。服务若使用其他 API 前缀，应填实际前缀。

`--vlm-base-url`、`--vlm-model` 显式参数分别优先于上述 env；两项都必须非空。这些变量配置的是服务 endpoint/model，不是检测器选择开关。必须明确传 `--detector vlm`，否则默认仍为 `yolo`。

即使 query/Web 的问答也使用同名 endpoint/model 变量，其文本问答请求也不是这里的 grounding 协议；不要把问答措辞、输出或 caller VLM 能力当作 detector 契约。

## 3. 单图 CLI：不需要摄像头

```sh
cargo +stable run -p item-ingest --features vlm -- \
  --detect path/to/photo.jpg --detector vlm \
  --targets "remote,keys" --vlm-timeout 60 --out out.png
```

- `vlm` 是构建 feature；`--detector vlm` 是运行时选择，两者缺一不可。该单图路径不需要 `camera` 或 `rtsp`。
- 输出包括载入/推理耗时、raw/kept 检测数量、每个框的 label、分数与**像素 xyxy** 坐标。这里的 raw 数量指 NMS 前检测，不是 raw 图片资产数量。
- `--out` 按扩展名保存烧框副本，画所有 NMS 存活框并高亮第一个；它不是 raw evidence，也不是结构化 caller annotation。
- 单图分支在打开观测数据库之前返回，不创建 Observation、regions 或 lifecycle 事件。失败会返回错误，不会像流式 runner 那样持续重试。
- 图片按原尺寸编码 JPEG，不自动缩图。高分辨率可能增加请求大小和 image-token 成本；可先制作较小测试图，再检查框的位置与坐标约定。

运行前应先按服务自身文档确认模型可用；本项目不会自动启动该sidecar。

## 4. 请求示例：客户端发给服务什么

请求采用以下结构，`<JPEG_BASE64>` 必须替换为真实 JPEG 的 base64；这是**输入请求**，不是服务输出格式或严格 JSON schema：

```json
{
  "model": "your-grounding-model",
  "temperature": 0.0,
  "messages": [{
    "role": "user",
    "content": [
      {"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,<JPEG_BASE64>"}},
      {"type": "text", "text": "Detect every instance of the following object types: remote, keys. For each instance output one JSON object {\"label\": \"...\", \"bbox_2d\": [xmin, ymin, xmax, ymax]} where xmin/ymin/xmax/ymax are integers normalized to 0-1000 relative to image width/height. Respond with ONLY the JSON array -- no markdown, no explanation. If none are found, respond with []."}
    ]
  }]
}
```

源码 HTTP 夹具覆盖了 data URL、目标提示词、OpenAI 风格回复和坐标转换。这证明客户端协议/解析路径的预期，不证明任一外部模型的检测准确率。

## 5. 预期回复：服务返回什么

HTTP body 是 OpenAI 风格外层对象；`content` 必须是字符串，而不是直接把检测数组放在 HTTP body 顶层：

```json
{
  "choices": [{
    "message": {
      "role": "assistant",
      "content": "[{\"label\":\"remote\",\"bbox_2d\":[100,200,500,800]}]"
    }
  }]
}
```

上例 `content` 解码后的预期模型输出是：

```json
[
  {"label": "remote", "bbox_2d": [100, 200, 500, 800]},
  {"label": "keys", "bbox_2d": [100, 600, 300, 900], "score": 0.88}
]
```

两段展示的是同一种结构，后一段另加一个可选目标作为说明。无目标的规范输出是 `[]`。

实际 parser 的容错边界：

- 只读第一个 choice；没有 choice 为错误。`content` 缺失/null 视为空串，非字符串不能按此协议解析。
- 从回复中找第一个 `[` 起始的完整数组，允许外围文字或 Markdown fence。没有数组、空串或数组未闭合被当作零检测；找到了完整数组但 JSON/字段类型错误则报错。
- `bbox_2d` 是四个数的 xyxy 数组，也接受 `bbox`、`box_2d` 别名。缺失/null bbox 的条目忽略，错误长度或类型会导致解析失败。
- label 去首尾空白并小写；缺失/空 label 使用 `object`。不做同义词合并。
- `score` 优先于 `confidence`，有值时 clamp 到 0–1，均缺失时为 1.0。这个默认值不表示“百分之百确定”，也不是标定概率。
- `--targets` 只进入提示词，返回的其他 label 不会被客户端硬过滤。回复内容仍需检查，纯文本被当作零检测也不证明目标真的不存在。

### 坐标转换

默认 `--vlm-coords norm1000`：按图像宽高分别乘 `W/1000`、`H/1000`。例如 200×100 图片中 `[100,200,500,800]` 转为像素 `[20,20,100,80]`。

若**整份回复中的任意 bbox 坐标大于 1000**，整份回复按像素解释，不是只转换那个框。`--vlm-coords pixel` 则始终按像素解释。客户端交换反向端点并裁剪至图像边界；流式 ingest 还丢弃非有限/零面积框，再执行 NMS。

这个启发式无法识别“所有坐标都小于 1000 的像素回复”；需按模型实际约定显式设置 `pixel`。VLM grounding 使用的 xyxy 不能直接提交给要求 **0–1 normalized_xywh** 的 caller annotation 工具。

## 6. 参数与流式使用

| 参数 | 默认/来源 | 作用 |
|---|---|---|
| `--detector vlm` | 默认 detector 是 `yolo` | 显式选择 grounding 后端 |
| `--vlm-base-url` | `ITEM_VLM_BASE_URL` | API base URL；客户端追加 `/chat/completions` |
| `--vlm-model` | `ITEM_VLM_MODEL` | 服务接受的模型 ID |
| `--vlm-timeout` | 60 秒 | 单次 HTTP 总 timeout；连接 timeout 固定 5 秒 |
| `--vlm-coords` | `norm1000` | `norm1000` 或 `pixel` |
| `--targets` | 下列 12 类 | 逗号分隔，trim/小写/去重；显式空串进入开放列举模式 |
| `--detect-fps` | VLM 0.2，YOLO/null 1.0 | 仅流式检测节流；需要 `camera`/`rtsp` 编译支持，不适用于单图 |

默认 targets：`remote,keys,scissors,charger,glasses,wallet,umbrella,medicine,bottle,cup,laptop,phone`。

流式路径另外启用帧源 feature：本地摄像头使用 `camera,vlm`，RTSP 使用 `rtsp,vlm`；帧源依赖和启动步骤见 [开始使用](getting-started.md)。daemon 中在对应 `[[camera]]` 设置 `detector="vlm"`，可配置该行的 `targets` / `detect_fps`，并给进程配置 endpoint/model。

VLM 每次 detect 都有同步 HTTP 往返。降低 FPS 可降低请求频率，但不会消除一次慢推理的延迟。HTTP 状态错误、超时或解析失败使 runner 跳帧并由 supervisor 退避，2 秒起步、指数增长至 60 秒；成功步骤重置退避，不是永远固定每 2 秒重试。

结果进入共用 NMS、zone、Observation 去重、几何/事件证据和所选快照模式。并非所有帧或每次检测的图片都会保留，详见 [检测流程](detection-flow.md)。

## 7. 与相对深度、caller VLM 的区别

- **Grounding detector**：`vlm` feature + `--detector vlm`，生成 label 和框，流式路径可创建 Observation。
- **相对深度 provider**：独立 `relative-depth` feature + `--depth-provider relative-vlm`，默认关闭；可以配 YOLO 或 VLM grounding。使用 `--depth-base-url`、`--depth-model`、`--depth-timeout`（默认 30 秒），单图配 `--depth-out depth.json` 才执行并输出该证据。显式 depth 设置优先；仅启用 relative-vlm 时可回用上述 VLM endpoint/model env。
- **Caller VLM**：ZCode 等 host 中的模型读取 raw 图片、进行视觉判断，必要时提交独立 caller annotation。它不要求本项目另启 grounding sidecar，也不是 daemon 中的后台 VLM worker；标注不变成 detector Observation。

相对深度只输出同图序数关系及质量/来源，不是米制距离、概率、标定坐标或 depth map。它使用另一份提示词与 parser，不能用本页 grounding JSON 代替其输出。流式 depth 失败会保留检测，但没有有效深度证据；单图 depth 失败则返回错误。

排错先区分 endpoint 不通、鉴权、非多模态模型、HTTP 协议不符、JSON 解析、坐标模式和模型召回问题。`RUST_LOG=item_ingest=debug` 会记录模型原始 reply，可能含场景信息；只在受控环境使用，勿将敏感回复写入公开诊断材料。
