//! 配置加载（`qqpilotEasyAudio.json`）。
//!
//! 配置文件必须是**合法 JSON**（不能带 `//` 注释）。为了兼容早期草稿，
//! 端口既接受 `"port"` 也接受 `"post"`。

use std::fs;
use std::path::Path;
use std::time::Duration;

use serde_json::{Map, Value};

/// 注入给 LLM 的工具名。响应里出现同名 tool_call 才会被转成语音。
pub const TOOL_NAME: &str = "text_to_speech";

/// 一次代理运行的全部配置。
#[derive(Clone, Debug)]
pub struct Config {
    /// 监听地址。
    pub host: String,
    /// 监听端口。
    pub port: u16,
    /// 真正的 LLM 上游。
    pub upstream: UpstreamConfig,
    /// 什么时候把文本转成语音。
    pub select: SelectMode,
    /// 怎么合成语音。
    pub tts: TtsConfig,
    /// 单个请求体的大小上限（字节）。QQPilot 可能带多张图片的 base64。
    pub max_body_bytes: usize,
}

#[derive(Clone, Debug)]
pub struct UpstreamConfig {
    /// 上游站点根地址，例如 `https://api.deepseek.com`。
    pub server: String,
    /// 为空时透传客户端（QQPilot）的 `Authorization` 头。
    pub api_key: String,
    pub timeout: Duration,
}

#[derive(Clone, Debug)]
pub enum SelectMode {
    /// 以 `p` 的概率把回复文本合成语音。
    Random { p: f64 },
    /// 给 LLM 注入 `text_to_speech` 工具，由模型决定是否调用。
    Tool { tool_choice: String },
}

#[derive(Clone, Debug)]
pub enum TtsConfig {
    /// 调用本地程序：`exec` 是完整命令行，支持 `{input}` / `{output}` 占位符。
    Exe {
        program: String,
        args: Vec<String>,
        extension: String,
        timeout: Duration,
    },
    /// 调用远程 HTTP TTS 接口，返回二进制音频。
    Remote {
        server: String,
        api_key: String,
        /// 发送给服务器的 JSON 模板，`{input}` 会被替换成文本。
        input: Value,
        extension: String,
        timeout: Duration,
    },
}

impl Config {
    /// 读取并校验配置文件。`path` 所在目录会用于解析 exe 的相对路径。
    pub fn load(path: &Path) -> Result<Self, String> {
        let raw =
            fs::read(path).map_err(|e| format!("读取配置文件 {} 失败: {e}", path.display()))?;
        let raw = strip_bom(&raw);
        let value: Value = serde_json::from_slice(raw).map_err(|e| {
            format!(
                "配置文件 {} 不是合法 JSON（请删除 // 注释）: {e}",
                path.display()
            )
        })?;
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        Self::from_value(&value, dir)
    }

    fn from_value(value: &Value, config_dir: &Path) -> Result<Self, String> {
        let root = value
            .as_object()
            .ok_or_else(|| "配置根节点必须是 JSON 对象".to_string())?;

        let host = optional_str(root, "host")
            .unwrap_or("127.0.0.1")
            .trim()
            .to_string();
        if host.is_empty() {
            return Err("host 不能为空".to_string());
        }

        let port_value = root
            .get("port")
            .or_else(|| root.get("post"))
            .ok_or_else(|| "缺少 port（端口）字段".to_string())?;
        let port = parse_u16(port_value, "port")?;

        let upstream_obj = root
            .get("upstream")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                "缺少 upstream 对象，例如 \"upstream\": {\"server\": \"https://api.deepseek.com\"}"
                    .to_string()
            })?;
        let upstream_server = required_str(upstream_obj, "server", "upstream.server")?;
        let upstream_api_key = optional_str(upstream_obj, "apiKey")
            .unwrap_or("")
            .trim()
            .to_string();
        let upstream_timeout = seconds(upstream_obj, 300, "upstream.timeoutSeconds")?;

        let select_obj = root
            .get("TTSSelectMode")
            .and_then(Value::as_object)
            .ok_or_else(|| "缺少 TTSSelectMode 对象".to_string())?;
        let select = parse_select(select_obj)?;

        let tts_obj = root
            .get("TTSMode")
            .and_then(Value::as_object)
            .ok_or_else(|| "缺少 TTSMode 对象".to_string())?;
        let tts = parse_tts(tts_obj, config_dir)?;

        let max_body_bytes = match root.get("maxBodyBytes") {
            Some(value) => {
                let n = value
                    .as_u64()
                    .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
                    .ok_or_else(|| "maxBodyBytes 必须是正整数".to_string())?;
                if n < 1024 {
                    return Err("maxBodyBytes 太小（至少 1024）".to_string());
                }
                n as usize
            }
            None => 64 * 1024 * 1024,
        };

        Ok(Self {
            host,
            port,
            upstream: UpstreamConfig {
                server: upstream_server,
                api_key: upstream_api_key,
                timeout: upstream_timeout,
            },
            select,
            tts,
            max_body_bytes,
        })
    }
}

fn parse_select(obj: &Map<String, Value>) -> Result<SelectMode, String> {
    let mode = required_str(obj, "mode", "TTSSelectMode.mode")?.to_ascii_lowercase();
    match mode.as_str() {
        "random" => {
            let p = match obj.get("p") {
                Some(value) => value
                    .as_f64()
                    .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
                    .ok_or_else(|| "TTSSelectMode.p 必须是 0~1 之间的数字".to_string())?,
                None => 0.9,
            };
            if !(0.0..=1.0).contains(&p) {
                return Err(format!("TTSSelectMode.p 必须在 0~1 之间，当前为 {p}"));
            }
            Ok(SelectMode::Random { p })
        }
        "tool" => {
            let tool_choice = optional_str(obj, "toolChoice")
                .unwrap_or("auto")
                .trim()
                .to_string();
            if tool_choice.is_empty() {
                return Err("TTSSelectMode.toolChoice 不能为空".to_string());
            }
            Ok(SelectMode::Tool { tool_choice })
        }
        other => Err(format!(
            "TTSSelectMode.mode 只支持 \"random\" 或 \"tool\"，当前为 \"{other}\""
        )),
    }
}

fn parse_tts(obj: &Map<String, Value>, config_dir: &Path) -> Result<TtsConfig, String> {
    let mode = required_str(obj, "mode", "TTSMode.mode")?.to_ascii_lowercase();
    let extension = normalize_extension(optional_str(obj, "extension").unwrap_or("wav"))?;
    let timeout = seconds(obj, 120, "TTSMode.timeoutSeconds")?;

    match mode.as_str() {
        "exe" => {
            let exec = required_str(obj, "exec", "TTSMode.exec")?;
            let mut tokens = split_command_line(&exec);
            if tokens.is_empty() {
                return Err("TTSMode.exec 不能为空".to_string());
            }
            let program = resolve_program(&tokens.remove(0), config_dir);
            Ok(TtsConfig::Exe {
                program,
                args: tokens,
                extension,
                timeout,
            })
        }
        "remote" => {
            let server = required_str(obj, "server", "TTSMode.server")?;
            if !(server.starts_with("http://") || server.starts_with("https://")) {
                return Err(format!("TTSMode.server 必须是 http(s) 地址: {server}"));
            }
            let api_key = optional_str(obj, "apiKey").unwrap_or("").trim().to_string();
            let input = obj
                .get("input")
                .cloned()
                .ok_or_else(|| "TTSMode.input 是发送给服务器的 JSON 模板，不能缺失".to_string())?;
            Ok(TtsConfig::Remote {
                server,
                api_key,
                input,
                extension,
                timeout,
            })
        }
        other => Err(format!(
            "TTSMode.mode 只支持 \"exe\" 或 \"remote\"，当前为 \"{other}\""
        )),
    }
}

fn normalize_extension(raw: &str) -> Result<String, String> {
    let ext = raw.trim().trim_start_matches('.').to_ascii_lowercase();
    match ext.as_str() {
        "wav" | "mp3" | "ogg" | "flac" | "m4a" | "aac" | "opus" | "webm" => Ok(ext),
        _ => Err(format!(
            "不支持的音频扩展名 \"{raw}\"，可用: wav / mp3 / ogg / flac / m4a / aac / opus / webm"
        )),
    }
}

fn required_str(obj: &Map<String, Value>, key: &str, display: &str) -> Result<String, String> {
    optional_str(obj, key)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("缺少必填字段 {display}（或值为空）"))
}

fn optional_str<'a>(obj: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    obj.get(key).and_then(Value::as_str)
}

fn seconds(obj: &Map<String, Value>, default: u64, key: &str) -> Result<Duration, String> {
    let value = match obj.get(key.split('.').next_back().unwrap_or(key)) {
        Some(value) => value
            .as_u64()
            .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
            .ok_or_else(|| format!("{key} 必须是正整数（秒）"))?,
        None => default,
    };
    if value == 0 {
        return Err(format!("{key} 必须大于 0"));
    }
    Ok(Duration::from_secs(value))
}

fn parse_u16(value: &Value, key: &str) -> Result<u16, String> {
    let port = value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
        .ok_or_else(|| format!("{key} 必须是 1~65535 的整数"))?;
    if port == 0 {
        return Err(format!("{key} 必须在 1~65535 之间"));
    }
    u16::try_from(port).map_err(|_| format!("{key} 超出范围（1~65535）: {port}"))
}

fn resolve_program(program: &str, config_dir: &Path) -> String {
    let path = Path::new(program);
    if path.is_absolute() {
        return program.to_string();
    }
    if program.contains(['\\', '/']) {
        return config_dir.join(path).to_string_lossy().into_owned();
    }
    // 裸文件名：优先用配置文件旁边的同名程序，其次交给系统 PATH。
    let beside_config = config_dir.join(program);
    if beside_config.exists() {
        return beside_config.to_string_lossy().into_owned();
    }
    program.to_string()
}

fn strip_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes)
}

/// 按 Windows 习惯切分命令行：双引号分组，反斜杠当普通字符（仅 `\"` 转义）。
///
/// 例：`"C:\Program Files\tts.exe" -i {input} -o {output}`
pub fn split_command_line(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut has_token = false;
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                has_token = true;
            }
            '\\' => {
                if in_quotes && chars.peek() == Some(&'"') {
                    current.push('"');
                    chars.next();
                } else {
                    current.push('\\');
                }
                has_token = true;
            }
            ch if ch.is_whitespace() && !in_quotes => {
                if has_token {
                    tokens.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            _ => {
                current.push(ch);
                has_token = true;
            }
        }
    }

    if has_token {
        tokens.push(current);
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_config() -> Value {
        serde_json::json!({
            "host": "127.0.0.1",
            "post": "10201",
            "upstream": {"server": "https://example.com"},
            "TTSSelectMode": {"mode": "random", "p": 0.9},
            "TTSMode": {"mode": "exe", "exec": "tts.exe -i {input} -o {output}"}
        })
    }

    #[test]
    fn accepts_post_alias_and_string_port() {
        let cfg = Config::from_value(&base_config(), Path::new(".")).unwrap();
        assert_eq!(cfg.port, 10201);
        assert_eq!(cfg.upstream.timeout, Duration::from_secs(300));
        match cfg.select {
            SelectMode::Random { p } => assert!((p - 0.9).abs() < f64::EPSILON),
            _ => panic!("应为 random"),
        }
    }

    #[test]
    fn parses_full_command_line_with_quotes_and_backslashes() {
        let tokens =
            split_command_line(r#""C:\Program Files\tts\tts.exe" -i {input} -o "{output}""#);
        assert_eq!(tokens[0], r"C:\Program Files\tts\tts.exe");
        assert_eq!(tokens[1], "-i");
        assert_eq!(tokens[3], "-o");
        assert_eq!(tokens[4], "{output}");
    }

    #[test]
    fn parses_tool_and_remote() {
        let value = serde_json::json!({
            "port": 10201,
            "upstream": {"server": "https://example.com/v1", "apiKey": "sk-x"},
            "TTSSelectMode": {"mode": "tool"},
            "TTSMode": {
                "mode": "remote",
                "server": "https://example.com/v1/tools/tts",
                "apiKey": "sk-tts",
                "input": {"text": "{input}", "voice_id": "stella"},
                "extension": ".wav"
            }
        });
        let cfg = Config::from_value(&value, Path::new(".")).unwrap();
        assert!(matches!(cfg.select, SelectMode::Tool { .. }));
        match cfg.tts {
            TtsConfig::Remote {
                input, extension, ..
            } => {
                assert_eq!(input["text"], "{input}");
                assert_eq!(extension, "wav");
            }
            _ => panic!("应为 remote"),
        }
    }

    #[test]
    fn rejects_bad_probability_and_missing_upstream() {
        let mut value = base_config();
        value["TTSSelectMode"]["p"] = serde_json::json!(1.5);
        assert!(Config::from_value(&value, Path::new(".")).is_err());

        let mut value = base_config();
        value.as_object_mut().unwrap().remove("upstream");
        assert!(Config::from_value(&value, Path::new(".")).is_err());
    }
}
