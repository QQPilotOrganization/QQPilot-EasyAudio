//! 文本转语音：本地 exe 与远程 HTTP 两种后端。
//!
//! 输出统一成为一段音频字节 + 扩展名，由上层包装成 QQPilot 认识的
//! `data:audio/<ext>;base64,...`。

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::Value;

use crate::config::TtsConfig;

/// 合成好的一段音频。
#[derive(Debug)]
pub struct Audio {
    pub bytes: Vec<u8>,
    pub extension: String,
}

impl Audio {
    /// QQPilot 的 `choices[0].message.audios[].audio_url.url` 接受的 data URL。
    pub fn data_url(&self) -> String {
        format!(
            "data:{};base64,{}",
            mime_of(&self.extension),
            base64::engine::general_purpose::STANDARD.encode(&self.bytes)
        )
    }
}

/// 合成语音。失败时返回给用户看的中文错误。
pub fn synthesize(text: &str, config: &TtsConfig) -> Result<Audio, String> {
    match config {
        TtsConfig::Exe {
            program,
            args,
            extension,
            timeout,
        } => synthesize_with_exe(text, program, args, extension, *timeout),
        TtsConfig::Remote {
            server,
            api_key,
            input,
            extension,
            timeout,
        } => synthesize_with_remote(text, server, api_key, input, extension, *timeout),
    }
}

fn synthesize_with_exe(
    text: &str,
    program: &str,
    args: &[String],
    extension: &str,
    timeout: Duration,
) -> Result<Audio, String> {
    let dir = temp_dir().join(format!(
        "qqpilot-easy-audio-{}-{}",
        std::process::id(),
        fastrand::u64(..)
    ));
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("创建 TTS 临时目录 {} 失败: {e}", dir.display()))?;

    let result = run_exe(text, program, args, extension, timeout, &dir);
    let _ = std::fs::remove_dir_all(&dir);
    result
}

fn run_exe(
    text: &str,
    program: &str,
    args: &[String],
    extension: &str,
    timeout: Duration,
    dir: &std::path::Path,
) -> Result<Audio, String> {
    let input_path = dir.join("input.txt");
    let output_path = dir.join(format!("output.{extension}"));

    std::fs::write(&input_path, text.as_bytes())
        .map_err(|e| format!("写入 TTS 输入文件失败: {e}"))?;

    let input = input_path.to_string_lossy().into_owned();
    let output = output_path.to_string_lossy().into_owned();

    let mut command = Command::new(program);
    for arg in args {
        command.arg(arg.replace("{input}", &input).replace("{output}", &output));
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = command
        .spawn()
        .map_err(|e| format!("启动 TTS 程序 {program} 失败: {e}"))?;

    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return Err(format!("TTS 程序退出码为 {status}"));
                }
                break;
            }
            Ok(None) => {
                if started.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("TTS 程序超时（{} 秒）", timeout.as_secs()));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("等待 TTS 程序失败: {e}")),
        }
    }

    let bytes = std::fs::read(&output_path).map_err(|e| {
        format!(
            "TTS 没有生成输出文件 {}: {e}（请检查 exec 是否包含 {{output}} 占位符）",
            output_path.display()
        )
    })?;
    if bytes.is_empty() {
        return Err("TTS 生成的音频为空".to_string());
    }

    Ok(Audio {
        bytes,
        extension: extension.to_string(),
    })
}

fn synthesize_with_remote(
    text: &str,
    server: &str,
    api_key: &str,
    input: &Value,
    extension: &str,
    timeout: Duration,
) -> Result<Audio, String> {
    let payload = substitute(input, text);
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .build()
        .into();

    let mut request = agent
        .post(server)
        .header("Content-Type", "application/json; charset=utf-8");
    if !api_key.is_empty() {
        request = request.header("Authorization", format!("Bearer {api_key}"));
    }

    let mut response = request
        .send_json(&payload)
        .map_err(|e| format!("请求 TTS 服务器失败: {e}"))?;

    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let body = response
        .body_mut()
        .read_to_vec()
        .map_err(|e| format!("读取 TTS 响应失败: {e}"))?;

    if !(200..300).contains(&status) {
        let preview: String = String::from_utf8_lossy(&body).chars().take(300).collect();
        return Err(format!("TTS 服务器返回 HTTP {status}: {preview}"));
    }
    if body.is_empty() {
        return Err("TTS 服务器返回了空响应".to_string());
    }

    if content_type.contains("json") {
        let value: Value =
            serde_json::from_slice(&body).map_err(|e| format!("解析 TTS JSON 响应失败: {e}"))?;
        let (bytes, detected) = audio_from_json(&value, &agent)?;
        return Ok(Audio {
            bytes,
            extension: detected.unwrap_or_else(|| extension.to_string()),
        });
    }

    Ok(Audio {
        bytes: body,
        extension: extension_from_mime(&content_type)
            .unwrap_or(extension)
            .to_string(),
    })
}

/// 兼容少数返回 JSON 的 TTS 服务（首选仍是二进制音频）。
fn audio_from_json(
    value: &Value,
    agent: &ureq::Agent,
) -> Result<(Vec<u8>, Option<String>), String> {
    let base64_text = value
        .get("audio")
        .and_then(Value::as_str)
        .or_else(|| value.get("b64_json").and_then(Value::as_str))
        .or_else(|| value.get("data").and_then(Value::as_str))
        .or_else(|| value.pointer("/data/0/b64_json").and_then(Value::as_str));

    if let Some(text) = base64_text {
        return decode_base64_audio(text).map(|bytes| (bytes, None));
    }

    if let Some(url) = value
        .get("url")
        .and_then(Value::as_str)
        .or_else(|| value.pointer("/data/0/url").and_then(Value::as_str))
    {
        let mut response = agent
            .get(url)
            .call()
            .map_err(|e| format!("下载 TTS 音频失败: {e}"))?;
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string();
        let bytes = response
            .body_mut()
            .read_to_vec()
            .map_err(|e| format!("读取 TTS 音频失败: {e}"))?;
        return Ok((
            bytes,
            extension_from_mime(&content_type).map(str::to_string),
        ));
    }

    Err("TTS 返回的 JSON 里找不到音频（audio / b64_json / data / url）".to_string())
}

fn decode_base64_audio(text: &str) -> Result<Vec<u8>, String> {
    let payload = text
        .split_once(',')
        .filter(|(meta, _)| meta.starts_with("data:"))
        .map(|(_, payload)| payload)
        .unwrap_or(text)
        .trim();
    let cleaned: String = payload.chars().filter(|ch| !ch.is_whitespace()).collect();
    base64::engine::general_purpose::STANDARD
        .decode(cleaned.as_bytes())
        .map_err(|e| format!("TTS 返回的 base64 解码失败: {e}"))
}

/// 递归替换 JSON 模板里的 `{input}`。交给 serde 序列化时会自动转义。
fn substitute(value: &Value, text: &str) -> Value {
    match value {
        Value::String(raw) => Value::String(raw.replace("{input}", text)),
        Value::Array(items) => {
            Value::Array(items.iter().map(|item| substitute(item, text)).collect())
        }
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, item) in map {
                out.insert(key.clone(), substitute(item, text));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

fn temp_dir() -> PathBuf {
    std::env::temp_dir()
}

fn mime_of(extension: &str) -> &'static str {
    match extension.to_ascii_lowercase().as_str() {
        "wav" => "audio/wav",
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "flac" => "audio/flac",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "opus" => "audio/ogg",
        "webm" => "audio/webm",
        _ => "audio/wav",
    }
}

fn extension_from_mime(content_type: &str) -> Option<&'static str> {
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    Some(match mime.as_str() {
        "audio/wav" | "audio/x-wav" | "audio/wave" => "wav",
        "audio/mpeg" | "audio/mp3" => "mp3",
        "audio/ogg" | "audio/opus" => "ogg",
        "audio/flac" | "audio/x-flac" => "flac",
        "audio/mp4" | "audio/x-m4a" | "audio/aac" => "m4a",
        "audio/webm" => "webm",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitutes_text_recursively() {
        let template = serde_json::json!({
            "text": "{input}",
            "nested": {"voice": "stella", "note": ["{input}！"]},
            "number": 1
        });
        let value = substitute(&template, "你好");
        assert_eq!(value["text"], "你好");
        assert_eq!(value["nested"]["note"][0], "你好！");
        assert_eq!(value["number"], 1);
    }

    #[test]
    fn data_url_has_audio_mime() {
        let audio = Audio {
            bytes: vec![1, 2, 3],
            extension: "mp3".to_string(),
        };
        assert!(audio.data_url().starts_with("data:audio/mpeg;base64,"));
    }
}
