//! 手写的 HTTP/1.1 服务端：只做代理需要的最小集合。
//!
//! 支持 `Content-Length` 和 `Transfer-Encoding: chunked` 请求体，
//! 响应一律 `Connection: close`（QQPilot 用 reqwest，每次重新连接没问题）。

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;

/// 一条已解析的 HTTP 请求。
#[derive(Clone, Debug)]
pub struct HttpRequest {
    pub method: String,
    /// 含查询串，例如 `/v1/chat/completions?x=1`。
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpRequest {
    /// 不区分大小写地取第一个匹配的请求头。
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// 读取并解析一条请求。`max_body` 是请求体字节上限。
pub fn read_request<R: BufRead>(reader: &mut R, max_body: usize) -> Result<HttpRequest, String> {
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(end) = find_header_end(&buf) {
            break end;
        }
        if buf.len() > 64 * 1024 {
            return Err("请求头过大（超过 64KB）".to_string());
        }
        let read = reader
            .read(&mut chunk)
            .map_err(|e| format!("读取请求失败: {e}"))?;
        if read == 0 {
            return Err("连接在请求头完成前关闭".to_string());
        }
        buf.extend_from_slice(&chunk[..read]);
    };

    let (head, rest) = buf.split_at(header_end);
    let mut header_buf = [httparse::EMPTY_HEADER; 128];
    let mut parsed = httparse::Request::new(&mut header_buf);
    parsed
        .parse(head)
        .map_err(|e| format!("请求头解析失败: {e}"))?;

    let method = parsed
        .method
        .ok_or_else(|| "请求缺少 method".to_string())?
        .to_string();
    let path = parsed
        .path
        .ok_or_else(|| "请求缺少路径".to_string())?
        .to_string();

    let headers: Vec<(String, String)> = parsed
        .headers
        .iter()
        .filter(|header| !header.name.is_empty())
        .map(|header| {
            (
                header.name.to_string(),
                String::from_utf8_lossy(header.value).trim().to_string(),
            )
        })
        .collect();

    let mut request = HttpRequest {
        method,
        path,
        headers,
        body: Vec::new(),
    };

    let chunked = request
        .header("transfer-encoding")
        .map(|value| value.to_ascii_lowercase().contains("chunked"))
        .unwrap_or(false);

    if chunked {
        request.body = read_chunked(rest, reader, max_body)?;
    } else if let Some(length) = request.header("content-length") {
        let length: usize = length
            .trim()
            .parse()
            .map_err(|_| format!("Content-Length 非法: {length}"))?;
        if length > max_body {
            return Err(format!("请求体过大（{length} 字节，上限 {max_body}）"));
        }
        let mut body = rest.to_vec();
        body.truncate(length.min(body.len()));
        while body.len() < length {
            let mut chunk = [0u8; 8192];
            let want = (length - body.len()).min(chunk.len());
            let read = reader
                .read(&mut chunk[..want])
                .map_err(|e| format!("读取请求体失败: {e}"))?;
            if read == 0 {
                return Err("请求体不完整".to_string());
            }
            body.extend_from_slice(&chunk[..read]);
        }
        request.body = body;
    }

    Ok(request)
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    if let Some(pos) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
        return Some(pos + 4);
    }
    buf.windows(2)
        .position(|window| window == b"\n\n")
        .map(|pos| pos + 2)
}

fn read_chunked<R: Read>(
    prefix: &[u8],
    reader: &mut R,
    max_body: usize,
) -> Result<Vec<u8>, String> {
    let mut source = BufReader::new(io::Cursor::new(prefix.to_vec()).chain(reader));
    let mut body = Vec::new();

    loop {
        let line = read_line(&mut source)?;
        let size_text = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| format!("chunk 长度非法: {size_text}"))?;

        if size == 0 {
            // 吃掉 trailer 直到空行。
            loop {
                let trailer = read_line(&mut source)?;
                if trailer.trim().is_empty() {
                    break;
                }
            }
            break;
        }

        if body.len() + size > max_body {
            return Err(format!("请求体过大（上限 {max_body} 字节）"));
        }
        let start = body.len();
        body.resize(start + size, 0);
        source
            .read_exact(&mut body[start..])
            .map_err(|e| format!("读取 chunk 失败: {e}"))?;

        let mut crlf = [0u8; 2];
        source
            .read_exact(&mut crlf)
            .map_err(|e| format!("读取 chunk 结尾失败: {e}"))?;
    }

    Ok(body)
}

fn read_line<R: BufRead>(reader: &mut R) -> Result<String, String> {
    let mut line = Vec::new();
    reader
        .read_until(b'\n', &mut line)
        .map_err(|e| format!("读取 chunk 行失败: {e}"))?;
    if line.is_empty() {
        return Err("chunked 请求体意外结束".to_string());
    }
    Ok(String::from_utf8_lossy(&line).to_string())
}

/// 向客户端回写一条响应。
pub fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reason(status),
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        411 => "Length Required",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        504 => "Gateway Timeout",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn parses_content_length_request() {
        let raw = b"POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: 7\r\n\r\n{\"a\":1}";
        let mut reader = Cursor::new(raw.to_vec());
        let request = read_request(&mut reader, 1024).unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/v1/chat/completions");
        assert_eq!(request.header("content-type"), Some("application/json"));
        assert_eq!(request.body, b"{\"a\":1}");
    }

    #[test]
    fn parses_chunked_request() {
        let raw = b"POST /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n{\"a\"\r\n3\r\n:1}\r\n0\r\n\r\n";
        let mut reader = Cursor::new(raw.to_vec());
        let request = read_request(&mut reader, 1024).unwrap();
        assert_eq!(request.body, b"{\"a\":1}");
    }

    #[test]
    fn rejects_oversized_body() {
        let raw = b"POST /x HTTP/1.1\r\nContent-Length: 100\r\n\r\n";
        let mut reader = Cursor::new(raw.to_vec());
        assert!(read_request(&mut reader, 10).is_err());
    }
}
