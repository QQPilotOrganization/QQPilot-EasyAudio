//! qqpilot-easy-audio：QQPilot 的 Chat Completion 中转 + TTS 响应改写。
//!
//! ```
//! QQPilot ──▶ 本程序(127.0.0.1:10201) ──▶ 真正的 LLM
//!                 │
//!                 └─ 按配置把文本换成 audios[] 再返回
//! ```

mod chat;
mod config;
mod http;
mod log;
mod proxy;
mod tts;

use std::io::BufReader;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use config::{Config, SelectMode};

fn main() {
    let config_path = resolve_config_path();
    let log_path = config_path.parent().map(|dir| dir.join("easy-audio.log"));
    log::init(log_path.as_deref());
    log::info(format!("配置文件: {}", config_path.display()));

    let config = match Config::load(&config_path) {
        Ok(config) => config,
        Err(error) => {
            log::error(error);
            eprintln!("\n配置有误，程序退出。可以用 --config <路径> 指定配置文件。");
            std::process::exit(1);
        }
    };

    log::info(format!("版本: {}", env!("CARGO_PKG_VERSION")));
    log::info(format!("监听: http://{}:{}", config.host, config.port));
    log::info(format!(
        "上游: {}（API Key: {}）",
        config.upstream.server,
        if config.upstream.api_key.is_empty() {
            "透传客户端的 Authorization"
        } else {
            "使用配置文件中的 Key"
        }
    ));
    match &config.select {
        SelectMode::Random { p } => log::info(format!("选择模式: random，p = {p}")),
        SelectMode::Tool { tool_choice } => {
            log::info(format!("选择模式: tool，tool_choice = {tool_choice}"))
        }
    }
    match &config.tts {
        config::TtsConfig::Exe {
            program, extension, ..
        } => log::info(format!("TTS: 本地 exe {program}（输出 .{extension}）")),
        config::TtsConfig::Remote {
            server, extension, ..
        } => log::info(format!("TTS: 远程 {server}（默认 .{extension}）")),
    }

    if let Err(error) = Server::new(config).run() {
        log::error(error);
        std::process::exit(1);
    }
}

fn resolve_config_path() -> PathBuf {
    if let Some(path) = cli_config_path() {
        return path;
    }

    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(PathBuf::from))
    {
        let candidate = dir.join("qqpilotEasyAudio.json");
        if candidate.exists() {
            return candidate;
        }
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let candidate = cwd.join("qqpilotEasyAudio.json");
    if candidate.exists() {
        return candidate;
    }
    candidate
}

fn cli_config_path() -> Option<PathBuf> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" | "-c" => {
                return args.next().map(PathBuf::from);
            }
            "--help" | "-h" => {
                println!(
                    "qqpilot-easy-audio {}\n\n用法: qqpilot-easy-audio [--config <配置文件>]\n\n默认读取 exe 同目录或当前目录的 qqpilotEasyAudio.json。",
                    env!("CARGO_PKG_VERSION")
                );
                std::process::exit(0);
            }
            other if other.starts_with("--config=") => {
                return Some(PathBuf::from(&other["--config=".len()..]));
            }
            _ => {}
        }
    }
    None
}

struct Server {
    config: Config,
    upstream: ureq::Agent,
}

impl Server {
    fn new(config: Config) -> Self {
        let upstream = proxy::agent_with_timeout(config.upstream.timeout);
        Self { config, upstream }
    }

    fn run(self) -> Result<(), String> {
        let listener =
            TcpListener::bind((self.config.host.as_str(), self.config.port)).map_err(|error| {
                format!(
                    "监听 {}:{} 失败: {error}（端口被占用或地址不可用）",
                    self.config.host, self.config.port
                )
            })?;
        log::info("等待 QQPilot 请求（Ctrl+C 退出）");
        serve(Arc::new(self), listener)
    }
}

fn serve(server: Arc<Server>, listener: TcpListener) -> Result<(), String> {
    for connection in listener.incoming() {
        match connection {
            Ok(stream) => {
                let server = Arc::clone(&server);
                thread::spawn(move || handle_connection(stream, &server));
            }
            Err(error) => log::warn(format!("accept 失败: {error}")),
        }
    }
    Ok(())
}

fn handle_connection(mut stream: TcpStream, server: &Server) {
    let peer = stream
        .peer_addr()
        .map(|addr| addr.to_string())
        .unwrap_or_else(|_| "?".to_string());
    let _ = stream.set_read_timeout(Some(Duration::from_secs(120)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(120)));

    let request = {
        let cloned = match stream.try_clone() {
            Ok(cloned) => cloned,
            Err(error) => {
                log::warn(format!("{peer} 复制连接失败: {error}"));
                return;
            }
        };
        let mut reader = BufReader::new(cloned);
        http::read_request(&mut reader, server.config.max_body_bytes)
    };

    let request = match request {
        Ok(request) => request,
        Err(error) => {
            log::warn(format!("{peer} 请求解析失败: {error}"));
            let body = error_body(&error);
            let _ =
                http::write_response(&mut stream, 400, "application/json; charset=utf-8", &body);
            return;
        }
    };

    let started = Instant::now();
    let (status, content_type, body) = route(server, &request);
    let result = http::write_response(&mut stream, status, &content_type, &body);
    let elapsed = started.elapsed().as_millis();
    match result {
        Ok(()) => log::info(format!(
            "{peer} {} {} -> {} ({} 字节, {} ms)",
            request.method,
            request.path,
            status,
            body.len(),
            elapsed
        )),
        Err(error) => log::warn(format!("{peer} 回写响应失败: {error}")),
    }
}

fn route(server: &Server, request: &http::HttpRequest) -> (u16, String, Vec<u8>) {
    let path = request.path.split('?').next().unwrap_or("");

    if request.method == "GET" && path == "/health" {
        return json_response(200, health(server));
    }
    if request.method == "GET" && path == "/" {
        let info = format!(
            "qqpilot-easy-audio {}\n监听端口: {}\n上游: {}\n健康检查: GET /health\n",
            env!("CARGO_PKG_VERSION"),
            server.config.port,
            server.config.upstream.server
        );
        return (
            200,
            "text/plain; charset=utf-8".to_string(),
            info.into_bytes(),
        );
    }

    let is_chat = path.ends_with("/chat/completions");
    let tool_choice = match &server.config.select {
        SelectMode::Tool { tool_choice } => Some(tool_choice.clone()),
        SelectMode::Random { .. } => None,
    };

    let mut outgoing = request.clone();
    if is_chat
        && request.method == "POST"
        && let Some(choice) = tool_choice
        && let Ok(mut value) = serde_json::from_slice::<Value>(&request.body)
    {
        chat::inject_tools(&mut value, &choice);
        if let Ok(body) = serde_json::to_vec(&value) {
            outgoing.body = body;
        }
    }

    let upstream = match proxy::forward(&server.upstream, &server.config, &outgoing) {
        Ok(response) => response,
        Err(error) => {
            log::error(error.clone());
            return json_error(502, &error);
        }
    };

    let mut content_type = if upstream.content_type.is_empty() {
        "application/octet-stream".to_string()
    } else {
        upstream.content_type
    };
    let mut body = upstream.body;

    if is_chat
        && (200..300).contains(&upstream.status)
        && let Ok(mut value) = serde_json::from_slice::<Value>(&body)
    {
        let changed = chat::rewrite(&mut value, &server.config);
        if changed > 0
            && let Ok(serialized) = serde_json::to_vec(&value)
        {
            body = serialized;
            content_type = "application/json; charset=utf-8".to_string();
        }
    }

    (upstream.status, content_type, body)
}

fn health(server: &Server) -> Value {
    let mode = match &server.config.select {
        SelectMode::Random { p } => json!({"mode": "random", "p": p}),
        SelectMode::Tool { tool_choice } => json!({"mode": "tool", "toolChoice": tool_choice}),
    };
    let tts_mode = match &server.config.tts {
        config::TtsConfig::Exe { .. } => "exe",
        config::TtsConfig::Remote { .. } => "remote",
    };
    json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "upstream": server.config.upstream.server,
        "select": mode,
        "tts": tts_mode
    })
}

fn json_response(status: u16, value: Value) -> (u16, String, Vec<u8>) {
    let body = serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec());
    (status, "application/json; charset=utf-8".to_string(), body)
}

fn json_error(status: u16, message: &str) -> (u16, String, Vec<u8>) {
    json_response(status, json!({"error": {"message": message}}))
}

fn error_body(message: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({"error": {"message": message}})).unwrap_or_else(|_| b"{}".to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn tiny_wav() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&36u32.to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&8000u32.to_le_bytes());
        bytes.extend_from_slice(&16000u32.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes
    }

    fn start_mock<F>(handler: F) -> String
    where
        F: Fn(http::HttpRequest) -> (u16, String, Vec<u8>) + Send + 'static,
    {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            for connection in listener.incoming() {
                let Ok(mut stream) = connection else { break };
                let Ok(cloned) = stream.try_clone() else {
                    continue;
                };
                let mut reader = BufReader::new(cloned);
                match http::read_request(&mut reader, 64 * 1024 * 1024) {
                    Ok(request) => {
                        let (status, content_type, body) = handler(request);
                        let _ = http::write_response(&mut stream, status, &content_type, &body);
                    }
                    Err(error) => {
                        let _ = http::write_response(
                            &mut stream,
                            400,
                            "text/plain; charset=utf-8",
                            error.as_bytes(),
                        );
                    }
                }
            }
        });
        format!("http://{address}")
    }

    fn start_proxy(config: Config) -> String {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = Arc::new(Server::new(config));
        thread::spawn(move || {
            let _ = serve(server, listener);
        });
        format!("http://{address}")
    }

    fn test_config(upstream: String, select: SelectMode, tts: config::TtsConfig) -> Config {
        Config {
            host: "127.0.0.1".to_string(),
            port: 0,
            upstream: config::UpstreamConfig {
                server: upstream,
                api_key: String::new(),
                timeout: Duration::from_secs(10),
            },
            select,
            tts,
            max_body_bytes: 8 * 1024 * 1024,
        }
    }

    fn tts_mock() -> String {
        start_mock(|_| (200, "audio/wav".to_string(), tiny_wav()))
    }

    fn remote_tts(server: String) -> config::TtsConfig {
        config::TtsConfig::Remote {
            server,
            api_key: String::new(),
            input: json!({"text": "{input}", "voice_id": "stella"}),
            extension: "wav".to_string(),
            timeout: Duration::from_secs(10),
        }
    }

    fn chat_reply(content: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "id": "test",
            "object": "chat.completion",
            "created": 0,
            "model": "test-model",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": content},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        }))
        .unwrap()
    }

    fn post_chat(proxy: &str, body: Value) -> Value {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        let mut response = agent
            .post(&format!("{proxy}/v1/chat/completions"))
            .header("Content-Type", "application/json; charset=utf-8")
            .send_json(&body)
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        let text = response.body_mut().read_to_string().unwrap();
        serde_json::from_str(&text).unwrap()
    }

    #[test]
    fn random_mode_replaces_text_with_audio() {
        let upstream = start_mock(|_| {
            (
                200,
                "application/json".to_string(),
                chat_reply("你好，世界"),
            )
        });
        let tts = tts_mock();
        let proxy = start_proxy(test_config(
            upstream,
            SelectMode::Random { p: 1.0 },
            remote_tts(tts),
        ));

        let response = post_chat(
            &proxy,
            json!({"model": "m", "messages": [], "stream": false}),
        );
        let message = &response["choices"][0]["message"];
        assert_eq!(message["content"], "");
        let url = message["audios"][0]["audio_url"]["url"].as_str().unwrap();
        assert!(url.starts_with("data:audio/wav;base64,"), "实际: {url}");
    }

    #[test]
    fn random_mode_keeps_text_when_not_selected() {
        let upstream =
            start_mock(|_| (200, "application/json".to_string(), chat_reply("普通回复")));
        let proxy = start_proxy(test_config(
            upstream,
            SelectMode::Random { p: 0.0 },
            remote_tts("http://127.0.0.1:9".to_string()),
        ));

        let response = post_chat(
            &proxy,
            json!({"model": "m", "messages": [], "stream": false}),
        );
        let message = &response["choices"][0]["message"];
        assert_eq!(message["content"], "普通回复");
        assert!(message.get("audios").is_none());
    }

    #[test]
    fn tool_mode_injects_tool_and_converts_tool_call() {
        let captured: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
        let captured_clone = Arc::clone(&captured);

        let upstream = start_mock(move |request| {
            *captured_clone.lock().unwrap() = serde_json::from_slice(&request.body).ok();
            let body = serde_json::to_vec(&json!({
                "id": "test",
                "object": "chat.completion",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": config::TOOL_NAME,
                                "arguments": "{\"text\":\"你好呀\"}"
                            }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }]
            }))
            .unwrap();
            (200, "application/json".to_string(), body)
        });

        let tts = tts_mock();
        let proxy = start_proxy(test_config(
            upstream,
            SelectMode::Tool {
                tool_choice: "auto".to_string(),
            },
            remote_tts(tts),
        ));

        let response = post_chat(
            &proxy,
            json!({"model": "m", "messages": [], "stream": false}),
        );

        let sent = captured.lock().unwrap().clone().expect("上游应收到请求");
        assert_eq!(sent["tools"][0]["function"]["name"], config::TOOL_NAME);
        assert_eq!(sent["tool_choice"], "auto");

        let message = &response["choices"][0]["message"];
        assert_eq!(message["content"], "");
        assert!(message.get("tool_calls").is_none());
        let url = message["audios"][0]["audio_url"]["url"].as_str().unwrap();
        assert!(url.starts_with("data:audio/wav;base64,"));
    }

    #[test]
    fn tool_mode_keeps_text_when_model_does_not_call_tool() {
        let upstream = start_mock(|_| {
            (
                200,
                "application/json".to_string(),
                chat_reply("没调用工具"),
            )
        });
        let proxy = start_proxy(test_config(
            upstream,
            SelectMode::Tool {
                tool_choice: "auto".to_string(),
            },
            remote_tts("http://127.0.0.1:9".to_string()),
        ));

        let response = post_chat(
            &proxy,
            json!({"model": "m", "messages": [], "stream": false}),
        );
        let message = &response["choices"][0]["message"];
        assert_eq!(message["content"], "没调用工具");
        assert!(message.get("audios").is_none());
    }

    #[test]
    fn exe_tts_copies_output_file() {
        let dir = std::env::temp_dir().join(format!(
            "qqpilot-easy-audio-exe-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("sample.wav");
        std::fs::write(&wav, tiny_wav()).unwrap();

        let config = config::TtsConfig::Exe {
            program: "cmd".to_string(),
            args: vec![
                "/c".to_string(),
                "copy".to_string(),
                "/b".to_string(),
                wav.to_string_lossy().into_owned(),
                "{output}".to_string(),
            ],
            extension: "wav".to_string(),
            timeout: Duration::from_secs(10),
        };

        let audio = tts::synthesize("你好", &config).unwrap();
        assert_eq!(audio.bytes, tiny_wav());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
