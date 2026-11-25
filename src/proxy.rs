//! 把请求透传给真正的 LLM 上游。

use ureq::Agent;

use crate::config::Config;
use crate::http::HttpRequest;

/// 上游返回的原始响应。
#[derive(Debug)]
pub struct UpstreamResponse {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
}

/// 原样转发一条请求；`Authorization` 的取舍见 [`super::config::UpstreamConfig`]。
pub fn forward(
    agent: &Agent,
    config: &Config,
    request: &HttpRequest,
) -> Result<UpstreamResponse, String> {
    let url = join_url(&config.upstream.server, &request.path);

    let mut builder = ureq::http::Request::builder()
        .method(request.method.as_str())
        .uri(&url);

    for (name, value) in &request.headers {
        if should_skip_header(name) {
            continue;
        }
        if name.eq_ignore_ascii_case("authorization") && !config.upstream.api_key.is_empty() {
            continue;
        }
        builder = builder.header(name.as_str(), value.as_str());
    }
    if !config.upstream.api_key.is_empty() {
        builder = builder.header(
            "authorization",
            format!("Bearer {}", config.upstream.api_key),
        );
    }

    let outgoing = builder
        .body(request.body.clone())
        .map_err(|e| format!("构造上游请求失败: {e}"))?;

    let mut response = agent
        .run(outgoing)
        .map_err(|e| format!("请求上游 {url} 失败: {e}"))?;

    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = response
        .body_mut()
        .read_to_vec()
        .map_err(|e| format!("读取上游响应失败: {e}"))?;

    Ok(UpstreamResponse {
        status,
        content_type,
        body,
    })
}

fn should_skip_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "host"
            | "connection"
            | "content-length"
            | "transfer-encoding"
            | "accept-encoding"
            | "keep-alive"
            | "proxy-connection"
            | "upgrade"
            | "te"
            | "trailer"
            | "expect"
    )
}

/// 拼接上游地址，并去掉重复的 `/v1`、`/api` 前缀。
///
/// 兼容两种填法：`server` 写站点根 `https://host`，或者写 `https://host/v1`。
pub fn join_url(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    let mut path = path.to_string();
    if !path.starts_with('/') {
        path.insert(0, '/');
    }
    for segment in ["/v1", "/api"] {
        if base.ends_with(segment) && path.starts_with(&format!("{segment}/")) {
            path = path[segment.len()..].to_string();
            break;
        }
    }
    format!("{base}{path}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_root_base() {
        assert_eq!(
            join_url("https://api.deepseek.com", "/v1/chat/completions"),
            "https://api.deepseek.com/v1/chat/completions"
        );
    }

    #[test]
    fn avoids_duplicating_version_prefix() {
        assert_eq!(
            join_url("https://api.deepseek.com/v1", "/v1/chat/completions"),
            "https://api.deepseek.com/v1/chat/completions"
        );
        assert_eq!(
            join_url("http://localhost:11434/api", "/api/chat"),
            "http://localhost:11434/api/chat"
        );
    }

    #[test]
    fn keeps_query_string() {
        assert_eq!(
            join_url("https://host", "/v1/models?x=1"),
            "https://host/v1/models?x=1"
        );
    }
}
