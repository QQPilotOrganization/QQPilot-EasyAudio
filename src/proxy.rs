//! 把请求透传给真正的 LLM 上游。

use std::time::Duration;

use ureq::Agent;

use crate::config::Config;
use crate::http::HttpRequest;

/// 创建统一配置的 HTTP 客户端。
///
/// 使用 Windows 自带的 SChannel（`native-tls`），与 QQPilot 自身的 reqwest 一致：
/// 不需要 clang，也不需要给 aarch64 交叉编译准备 C 工具链。
///
/// 注意 ureq 的 TLS provider 默认是 Rustls，必须显式指定 NativeTls，
/// 否则会 panic（provider is Rustls but feature is not enabled）。
pub fn agent_with_timeout(timeout: Duration) -> Agent {
    let tls = ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::NativeTls)
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .build();

    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .tls_config(tls)
        .build()
        .into()
}

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
/// 兼容三种填法：
/// * 站点根：`https://host`
/// * 带版本：`https://host/v1`
/// * 完整端点：`https://host/v1/chat/completions`
pub fn join_url(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    let (path_only, query) = match path.split_once('?') {
        Some((path_only, query)) => (path_only, Some(query)),
        None => (path, None),
    };
    let mut path = if path_only.starts_with('/') {
        path_only.to_string()
    } else {
        format!("/{path_only}")
    };

    // 直接填了完整端点：原样使用，只把查询串带过去。
    if base.ends_with("/chat/completions") && path.ends_with("/chat/completions") {
        return match query {
            Some(query) => format!("{base}?{query}"),
            None => base.to_string(),
        };
    }

    for segment in ["/v1", "/api"] {
        if base.ends_with(segment) && path.starts_with(&format!("{segment}/")) {
            path = path[segment.len()..].to_string();
            break;
        }
    }
    match query {
        Some(query) => format!("{base}{path}?{query}"),
        None => format!("{base}{path}"),
    }
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

    #[test]
    fn accepts_full_chat_completions_endpoint() {
        assert_eq!(
            join_url(
                "https://api.siliconflow.cn/v1/chat/completions",
                "/v1/chat/completions"
            ),
            "https://api.siliconflow.cn/v1/chat/completions"
        );
        assert_eq!(
            join_url(
                "https://api.siliconflow.cn/v1/chat/completions/",
                "/v1/chat/completions?x=1"
            ),
            "https://api.siliconflow.cn/v1/chat/completions?x=1"
        );
    }
}
