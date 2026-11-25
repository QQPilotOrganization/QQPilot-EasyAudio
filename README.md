# qqpilot-easy-audio

给 [QQPilot](https://github.com/QQPilotOrganization/QQPilot) 用的 **Chat Completion 中转 + TTS 响应改写器**。

```
QQPilot ──POST /v1/chat/completions──▶ 本程序 ──透传──▶ 真正的 LLM
          ◀── content / audios[] ──────┘
```

QQPilot 会把回复里的 `choices[0].message.audios[]` 放成 QQ 语音消息（需要开启“下载远程音频”）。
本程序按配置把文本换成 `data:audio/<ext>;base64,...` 并**清空文本**，于是模型回复就变成了语音。

## 编译

```powershell
cargo build --release
```

产物：`target\release\qqpilot-easy-audio.exe`。把 exe 和 `qqpilotEasyAudio.json` 放在同一个目录即可双击/开机自启。

## 运行

```powershell
# 默认读取 exe 同目录或当前目录的 qqpilotEasyAudio.json
.\qqpilot-easy-audio.exe

# 指定配置文件（例如使用 tool 模式示例）
.\qqpilot-easy-audio.exe --config "qqpilotEasyAudio copy.json"
```

启动后监听 `http://127.0.0.1:10201`，可访问 `GET /health` 自检。

## 配置

配置文件必须是**合法 JSON**（不能写 `//` 注释）。完整字段：

```json
{
    "host": "127.0.0.1",
    "port": 10201,
    "upstream": {
        "server": "https://api.deepseek.com",
        "apiKey": "",
        "timeoutSeconds": 300
    },
    "TTSSelectMode": {
        "mode": "random",
        "p": 0.9
    },
    "TTSMode": {
        "mode": "exe",
        "exec": "tts.exe -i {input} -o {output}",
        "extension": "wav",
        "timeoutSeconds": 120
    }
}
```

| 字段 | 说明 |
|---|---|
| `host` / `port` | 监听地址。端口也兼容旧写法 `post`。 |
| `upstream.server` | 真正的 LLM 地址。写站点根 `https://api.deepseek.com`、带版本 `https://api.deepseek.com/v1`，或完整端点 `https://api.deepseek.com/v1/chat/completions` 都可以。 |
| `upstream.apiKey` | 留空则**透传 QQPilot 发来的 `Authorization`**；填写则使用这里的 Key。 |
| `upstream.timeoutSeconds` | 上游请求超时，默认 300。 |
| `TTSSelectMode` | `random`：按概率合成；`tool`：注入工具由模型决定。 |
| `TTSMode` | `exe`：本地程序；`remote`：HTTP 接口。 |
| `maxBodyBytes` | 可选，请求体上限，默认 64 MB。 |

### 选择模式一：`random`

```json
"TTSSelectMode": { "mode": "random", "p": 0.9 }
```

每次回复有 `p` 的概率合成语音。命中时：

- `content` → `""`
- 追加 `audios: [{ "audio_url": { "url": "data:audio/wav;base64,..." } }]`

没命中或 TTS 失败时原样返回文本。

### 选择模式二：`tool`

```json
"TTSSelectMode": { "mode": "tool", "toolChoice": "auto" }
```

请求里会追加一个函数工具：

```json
{
  "type": "function",
  "function": {
    "name": "text_to_speech",
    "description": "把要发送给用户的完整回复转换成语音……",
    "parameters": {
      "type": "object",
      "properties": { "text": { "type": "string" } },
      "required": ["text"]
    }
  }
}
```

模型调用 `text_to_speech` 后，程序取 `arguments.text` 合成语音，清空正文、移除这条 tool_call 并返回 `audios`。
**模型不调用工具时原样返回文本**（由模型自己决定这条消息要不要语音）。

> `tool` 模式需要模型支持 Function Calling；`toolChoice` 可选 `auto` / `required`，默认 `auto`。

### TTS 后端一：`exe`

```json
"TTSMode": {
    "mode": "exe",
    "exec": "\"C:\\Tools\\tts.exe\" -f {input} -o {output}",
    "extension": "wav",
    "timeoutSeconds": 120
}
```

- `exec` 是**完整命令行**：第一个参数是程序，支持双引号；相对路径按配置文件所在目录解析。
- `{input}` 会替换成写入待朗读文本的 UTF-8 `input.txt` 路径。
- `{output}` 会替换成程序需要生成的音频文件路径（扩展名由 `extension` 决定）。
- 程序退出码必须为 0，且 `{output}` 文件必须存在、非空。
- 音频格式只能 `wav`（QQPilot 用 miniaudio 播放）。

### TTS 后端二：`remote`

```json
"TTSMode": {
    "mode": "remote",
    "server": "https://api.deepseek.com/v1/tools/tts",
    "apiKey": "sk-...",
    "input": {
        "text": "{input}",
        "voice_id": "stella"
    },
    "extension": "wav",
    "timeoutSeconds": 120
}
```

- `input` 会作为 JSON 请求体 POST 到 `server`，其中所有字符串里的 `{input}` 替换为文本（自动 JSON 转义）。
- 请求头：`Content-Type: application/json`，有 `apiKey` 时带 `Authorization: Bearer <key>`。
- 响应按**二进制音频**处理，扩展名取自 `Content-Type`，取不到时用 `extension`。
- 也兼容 JSON 返回的 `audio` / `b64_json` / `data` / `url` 字段。

## QQPilot 侧设置

1. `option3` → **服务器** 填 `http://127.0.0.1:10201/v1`。
2. **API Key** 仍然填你真实 LLM 的 Key（本程序默认透传）。
3. **必须勾选“下载远程音频”**（`config.ini` 里 `remote_audios = true`），否则清空文本后音频会被丢弃。
4. QQ 语音录制需要开启系统“立体声混音”，详见 QQPilot 文档。
5. 模型回复长度受 QQPilot“最大输入字数”限制；只影响文本，不影响已生成的音频。

## 工作细节

- 只改写 OpenAI 兼容的 `POST */chat/completions` 的 **非流式** JSON 响应；其它路径/状态码/非 JSON 响应原样透传。
- `stream: true` 的请求原样透传（目前不做流式 TTS）。
- 合成失败不会丢消息：
  - `random`：保留原文；
  - `tool`：把工具参数里的文本回填成 `content`。
- 日志同时输出到控制台和配置文件旁的 `easy-audio.log`（仅记录启动以来的时间，无额外依赖）。

## 排错

| 现象 | 排查 |
|---|---|
| 启动报“非法 JSON” | 删除配置文件里的 `//` 注释、检查引号/尾逗号。 |
| 端口被占用 | 改 `port`，或结束占用 10201 的进程。 |
| QQPilot 没反应/报错 | 访问 `GET http://127.0.0.1:10201/health` 确认代理在跑；看日志里的上游状态码。 |
| 文本没了但没有语音 | QQPilot 是否开启“下载远程音频”；`data:` URL 的 MIME 是否是支持的音频格式。 |
| `exe` 模式报找不到输出文件 | 确认 `exec` 里包含 `{output}`，且程序真的写入了该路径。 |
| tool 模式模型不调用工具 | 换支持 Function Calling 的模型，或把 `toolChoice` 设为 `required`。 |

## 安全提示

配置文件里的 `apiKey` 是明文；不要提交到公开仓库。`upstream.apiKey` 留空即可透传 QQPilot 的 Key，通常不需要重复填写。
