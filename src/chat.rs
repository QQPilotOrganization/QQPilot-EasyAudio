//! 请求注入与响应改写。
//!
//! * `random`：按概率把 `message.content` 换成音频。
//! * `tool`：给请求注入 `text_to_speech` 工具；模型调用后把参数里的文本换成音频。

use std::time::Instant;

use serde_json::{Map, Value, json};

use crate::config::{Config, SelectMode, TOOL_NAME};
use crate::log;
use crate::tts;

/// 请求体里注入工具（只有 tool 模式调用）。
pub fn inject_tools(body: &mut Value, tool_choice: &str) {
    let Some(root) = body.as_object_mut() else {
        return;
    };

    match root.get_mut("tools") {
        Some(Value::Array(tools)) => {
            let exists = tools.iter().any(|tool| {
                tool.pointer("/function/name").and_then(Value::as_str) == Some(TOOL_NAME)
            });
            if !exists {
                tools.push(tool_definition());
            }
        }
        Some(_) => {
            // 用户自己写了非数组的 tools，不碰，避免把请求弄坏。
            return;
        }
        None => {
            root.insert("tools".to_string(), json!([tool_definition()]));
        }
    }

    if !root.contains_key("tool_choice") {
        root.insert(
            "tool_choice".to_string(),
            Value::String(tool_choice.to_string()),
        );
    }
}

fn tool_definition() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": TOOL_NAME,
            "description": "把要发送给用户的完整回复转换成语音。每次回复前先调用本工具，把准备发送的完整文本原样放进 text；调用后不需要再输出文本。",
            "parameters": {
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "要朗读的完整回复文本"
                    }
                },
                "required": ["text"]
            }
        }
    })
}

/// 按配置改写响应体，返回被改成语音的 choices 数量。
pub fn rewrite(body: &mut Value, config: &Config) -> usize {
    match &config.select {
        SelectMode::Random { p } => {
            if fastrand::f64() < *p {
                rewrite_random(body, config)
            } else {
                0
            }
        }
        SelectMode::Tool { .. } => rewrite_tool(body, config),
    }
}

fn rewrite_random(body: &mut Value, config: &Config) -> usize {
    let Some(choices) = body.get_mut("choices").and_then(Value::as_array_mut) else {
        return 0;
    };

    let mut changed = 0;
    for choice in choices.iter_mut() {
        let text = choice
            .pointer("/message/content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if text.is_empty() {
            continue;
        }

        let speak = normalize(&text);
        let started = Instant::now();
        match tts::synthesize(&speak, &config.tts) {
            Ok(audio) => {
                log::info(format!(
                    "random 命中：合成 {} 字节 {:?}（{} ms）",
                    audio.bytes.len(),
                    audio.extension,
                    started.elapsed().as_millis()
                ));
                if let Some(message) = choice.get_mut("message").and_then(Value::as_object_mut) {
                    message.insert("content".to_string(), Value::String(String::new()));
                    append_audio(message, &audio);
                }
                set_finish_reason(choice, "stop");
                changed += 1;
            }
            Err(error) => log::warn(format!("random 模式 TTS 失败，保留原文: {error}")),
        }
    }
    changed
}

fn rewrite_tool(body: &mut Value, config: &Config) -> usize {
    let Some(choices) = body.get_mut("choices").and_then(Value::as_array_mut) else {
        return 0;
    };

    let mut changed = 0;
    for choice in choices.iter_mut() {
        let Some((index, text)) = take_tool_text(choice) else {
            continue;
        };

        let speak = normalize(&text);
        let started = Instant::now();
        let result = tts::synthesize(&speak, &config.tts);

        let Some(message) = choice.get_mut("message").and_then(Value::as_object_mut) else {
            continue;
        };

        // 不管合成成功与否，都把这条工具调用从响应里摘掉（QQPilot 不认识它）。
        if let Some(Value::Array(calls)) = message.get_mut("tool_calls") {
            if index < calls.len() {
                calls.remove(index);
            }
            if calls.is_empty() {
                message.remove("tool_calls");
            }
        }

        match result {
            Ok(audio) => {
                log::info(format!(
                    "tool 调用：合成 {} 字节 {:?}（{} ms）",
                    audio.bytes.len(),
                    audio.extension,
                    started.elapsed().as_millis()
                ));
                message.insert("content".to_string(), Value::String(String::new()));
                append_audio(message, &audio);
                changed += 1;
            }
            Err(error) => {
                // 合成失败就退回文本，至少消息还能正常发出去。
                let content_is_empty = message
                    .get("content")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .unwrap_or("")
                    .is_empty();
                if content_is_empty {
                    message.insert("content".to_string(), Value::String(text));
                }
                log::warn(format!("tool 模式 TTS 失败，退回文本: {error}"));
            }
        }

        set_finish_reason(choice, "stop");
    }
    changed
}

/// 在 `message.tool_calls` 里找本程序的工具调用，返回 (下标, 文本)。
fn take_tool_text(choice: &Value) -> Option<(usize, String)> {
    let calls = choice.pointer("/message/tool_calls")?.as_array()?;
    for (index, call) in calls.iter().enumerate() {
        if call.pointer("/function/name").and_then(Value::as_str) != Some(TOOL_NAME) {
            continue;
        }
        let Some(arguments) = call.pointer("/function/arguments") else {
            continue;
        };
        let text = match arguments {
            Value::String(raw) => serde_json::from_str::<Value>(raw)
                .ok()
                .and_then(|value| {
                    value
                        .get("text")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_else(|| raw.clone()),
            other => match other.get("text").and_then(Value::as_str) {
                Some(text) => text.to_string(),
                None => continue,
            },
        };
        if text.trim().is_empty() {
            continue;
        }
        return Some((index, text));
    }
    None
}

fn append_audio(message: &mut Map<String, Value>, audio: &tts::Audio) {
    let entry = json!({ "audio_url": { "url": audio.data_url() } });
    match message.get_mut("audios") {
        Some(Value::Array(audios)) => audios.push(entry),
        _ => {
            message.insert("audios".to_string(), json!([entry]));
        }
    }
}

fn set_finish_reason(choice: &mut Value, reason: &str) {
    if let Some(object) = choice.as_object_mut() {
        object.insert(
            "finish_reason".to_string(),
            Value::String(reason.to_string()),
        );
    }
}

/// 朗读前做一点文本清理：qqpilot 用 `[[NEXT]]` 表示分段，读出来会很怪。
fn normalize(text: &str) -> String {
    text.replace("[[NEXT]]", "，").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injects_tool_and_choice() {
        let mut body = json!({"model": "m", "messages": []});
        inject_tools(&mut body, "auto");
        assert_eq!(body["tools"][0]["function"]["name"], TOOL_NAME);
        assert_eq!(body["tool_choice"], "auto");

        // 重复注入不会产生两份
        inject_tools(&mut body, "auto");
        assert_eq!(body["tools"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn extracts_tool_text() {
        let choice = json!({
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "1",
                    "type": "function",
                    "function": {"name": TOOL_NAME, "arguments": "{\"text\":\"你好呀\"}"}
                }]
            }
        });
        let (index, text) = take_tool_text(&choice).unwrap();
        assert_eq!(index, 0);
        assert_eq!(text, "你好呀");
    }

    #[test]
    fn normalizes_next_marker() {
        assert_eq!(normalize(" 第一段[[NEXT]]第二段 "), "第一段，第二段");
    }
}
