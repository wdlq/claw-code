//! `/webui` — 在浏览器中查看当前项目的历史会话。
//!
//! 用 `std::net::TcpListener` 起一个极简 HTTP server（零额外依赖），只服务：
//!   - `GET /`            → 内嵌的单页 HTML
//!   - `GET /api/sessions`→ 当前项目 `.claw/sessions/<hash>/` 下的会话列表
//!   - `GET /api/session?id=<file>` → 指定会话文件的消息序列
//!
//! 服务器在独立的后台线程里运行，`WebuiServer::stop()` 可以关停它。

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::SystemTime;

use serde::Serialize;

/// `/webui` 默认端口。
pub const WEBUI_DEFAULT_PORT: u16 = 18395;

/// 运行中的 webui server 句柄；drop 即关停。
pub struct WebuiServer {
    running: Arc<AtomicBool>,
    port: u16,
}

impl WebuiServer {
    /// 返回 server 实际监听的端口。
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// 关停 server。
    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }
}

impl Drop for WebuiServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 启动 webui server，返回句柄。`sessions_dir` 是当前项目的
/// `.claw/sessions/<hash>/` 目录。
pub fn start_server(
    sessions_dir: PathBuf,
    host: &str,
    start_port: u16,
) -> Result<WebuiServer, String> {
    // 扫描端口，避免冲突。
    let (listener, actual_port) = bind_scanning(host, start_port, 100)
        .map_err(|e| format!("端口绑定失败（{host}:{start_port} 起）：{e}"))?;

    let running = Arc::new(AtomicBool::new(true));
    let running_clone = running.clone();

    thread::Builder::new()
        .name("claw-webui".into())
        .spawn(move || {
            while running_clone.load(Ordering::SeqCst) {
                // accept 设置超时，以便能周期性检查 running 标志。
                let stream = match listener.accept() {
                    Ok((s, _)) => s,
                    Err(_) => {
                        thread::sleep(std::time::Duration::from_millis(100));
                        continue;
                    }
                };
                let dir = sessions_dir.clone();
                thread::spawn(move || {
                    let _ = handle_request(stream, &dir);
                });
            }
        })
        .map_err(|e| format!("启动后台线程失败：{e}"))?;

    Ok(WebuiServer {
        running,
        port: actual_port,
    })
}

/// 从 `start_port` 起尝试绑定，遇 `AddrInUse` 递增端口。
fn bind_scanning(
    host: &str,
    start_port: u16,
    max_tries: u16,
) -> std::io::Result<(TcpListener, u16)> {
    let mut last_err: Option<std::io::Error> = None;
    for offset in 0..max_tries {
        let Some(port) = start_port.checked_add(offset) else {
            break;
        };
        let addr = format!("{host}:{port}");
        match TcpListener::bind(&addr) {
            Ok(listener) => {
                let actual = listener.local_addr()?.port();
                return Ok((listener, actual));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                last_err = Some(e);
                continue;
            }
            Err(e) => return Err(e),
        }
    }
    Err(last_err
        .unwrap_or_else(|| std::io::Error::new(std::io::ErrorKind::AddrInUse, "no free port")))
}

/// 处理单个 HTTP 请求。
fn handle_request(mut stream: TcpStream, sessions_dir: &Path) -> std::io::Result<()> {
    // 读取请求行 + headers（直到空行）。
    let mut reader = BufReader::new(&stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;

    // 读取 headers（忽略 body）。
    let mut headers: HashMap<String, String> = HashMap::new();
    loop {
        let mut header_line = String::new();
        let n = reader.read_line(&mut header_line)?;
        if n == 0 || header_line.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = header_line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    let parts: Vec<&str> = request_line.split_whitespace().collect();
    let (method, path) = match (parts.first(), parts.get(1)) {
        (Some(m), Some(p)) => (*m, *p),
        _ => return Ok(()),
    };

    let body: Vec<u8> = match (method, path) {
        ("GET", "/") => index_html().as_bytes().to_vec(),
        ("GET", "/api/sessions") => {
            let sessions = list_sessions(sessions_dir);
            serde_json::to_vec(&sessions).unwrap_or_default()
        }
        ("GET", p) if p.starts_with("/api/session") => {
            // 解析 ?id=<file>
            let query = p.split('?').nth(1).unwrap_or("");
            let id = parse_query_value(query, "id");
            let session = load_session(sessions_dir, id);
            serde_json::to_vec(&session).unwrap_or_default()
        }
        _ => Vec::new(),
    };

    let status = if body.is_empty() && !matches!(path, "/") {
        "404 Not Found"
    } else {
        "200 OK"
    };
    let content_type = if path == "/" {
        "text/html; charset=utf-8"
    } else if path.starts_with("/api/") {
        "application/json; charset=utf-8"
    } else {
        "application/octet-stream"
    };
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );

    stream.write_all(header.as_bytes())?;
    stream.write_all(&body)?;
    stream.flush()?;
    Ok(())
}

fn parse_query_value(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key {
                return Some(url_decode(v));
            }
        }
    }
    None
}

fn url_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '+' {
            out.push(' ');
        } else if c == '%' {
            let h1 = chars.next();
            let h2 = chars.next();
            if let (Some(a), Some(b)) = (h1, h2) {
                if let Ok(byte) = u8::from_str_radix(&format!("{a}{b}"), 16) {
                    out.push(byte as char);
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// 会话列表里的一项。
#[derive(Serialize)]
pub struct SessionListItem {
    pub file: String,
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub created_at_ms: Option<u64>,
    pub updated_at_ms: Option<u64>,
    pub workspace_root: Option<String>,
    pub line_count: usize,
    pub byte_count: u64,
}

/// 扫描 `sessions_dir`，返回所有 `.jsonl` 会话文件的信息，按更新时间倒序。
pub fn list_sessions(sessions_dir: &Path) -> Vec<SessionListItem> {
    let mut items: Vec<(SessionListItem, SystemTime)> = Vec::new();
    let entries = match fs::read_dir(sessions_dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        let metadata = match fs::metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let mtime = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let byte_count = metadata.len();
        // 读首行 session_meta 提取元信息。
        let info = read_session_meta(&path);
        items.push((
            SessionListItem {
                file: file_name,
                session_id: info.session_id,
                model: info.model,
                created_at_ms: info.created_at_ms,
                updated_at_ms: info.updated_at_ms,
                workspace_root: info.workspace_root,
                line_count: info.line_count,
                byte_count,
            },
            mtime,
        ));
    }
    items.sort_by_key(|b| std::cmp::Reverse(b.1));
    items.into_iter().map(|(item, _)| item).collect()
}

/// 从 jsonl 首行 session_meta 提取的元信息 + 行数统计。
struct SessionMetaInfo {
    session_id: Option<String>,
    model: Option<String>,
    created_at_ms: Option<u64>,
    updated_at_ms: Option<u64>,
    workspace_root: Option<String>,
    line_count: usize,
}

impl SessionMetaInfo {
    const fn empty() -> Self {
        Self {
            session_id: None,
            model: None,
            created_at_ms: None,
            updated_at_ms: None,
            workspace_root: None,
            line_count: 0,
        }
    }
}

/// 读 jsonl 首行 session_meta 和统计行数。
fn read_session_meta(path: &Path) -> SessionMetaInfo {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return SessionMetaInfo::empty(),
    };
    let reader = BufReader::new(file);
    let mut info = SessionMetaInfo::empty();
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        info.line_count += 1;
        if info.session_id.is_none() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                info.session_id = v
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                info.model = v
                    .get("model")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                info.created_at_ms = v.get("created_at_ms").and_then(|v| v.as_u64());
                info.updated_at_ms = v.get("updated_at_ms").and_then(|v| v.as_u64());
                info.workspace_root = v
                    .get("workspace_root")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
            }
        }
    }
    info
}

/// 单条会话消息（前端渲染用）。
#[derive(Serialize)]
pub struct SessionMessage {
    pub role: String,
    pub parts: Vec<SessionPart>,
}

#[derive(Serialize)]
pub struct SessionPart {
    pub kind: String,
    pub text: Option<String>,
    pub tool_name: Option<String>,
    pub tool_use_id: Option<String>,
    pub input: Option<String>,
    pub output: Option<String>,
    pub is_error: Option<bool>,
}

/// 加载指定会话文件，返回消息序列。
pub fn load_session(sessions_dir: &Path, file: Option<String>) -> Vec<SessionMessage> {
    let file = match file {
        Some(f) => f,
        None => return Vec::new(),
    };
    // 防止路径穿越：只取文件名部分。
    let safe_name = Path::new(&file)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    if safe_name.is_empty() {
        return Vec::new();
    }
    let path = sessions_dir.join(safe_name);
    let f = match fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let reader = BufReader::new(f);
    let mut messages: Vec<SessionMessage> = Vec::new();
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let typ = v.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if typ == "message" {
            if let Some(msg) = parse_message(&v) {
                messages.push(msg);
            }
        } else if typ == "compaction" {
            // 把 compaction 的 summary 作为一条 system 消息插入。
            if let Some(summary) = v.get("summary").and_then(|s| s.as_str()) {
                messages.push(SessionMessage {
                    role: "system".to_string(),
                    parts: vec![SessionPart {
                        kind: "text".to_string(),
                        text: Some(format!("[compaction]\n{summary}")),
                        tool_name: None,
                        tool_use_id: None,
                        input: None,
                        output: None,
                        is_error: None,
                    }],
                });
            }
        }
    }
    messages
}

/// 把 JSONL 的一行 message 解析成 `SessionMessage`。
fn parse_message(v: &serde_json::Value) -> Option<SessionMessage> {
    let msg = v.get("message")?;
    let role = msg
        .get("role")
        .and_then(|r| r.as_str())
        .unwrap_or("unknown")
        .to_string();
    let blocks = msg.get("blocks").and_then(|b| b.as_array())?;
    let mut parts: Vec<SessionPart> = Vec::new();
    for block in blocks {
        let kind = block
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("text")
            .to_string();
        let text = block
            .get("text")
            .and_then(|t| t.as_str())
            .map(|s| s.to_string());
        let tool_name = block
            .get("name")
            .and_then(|t| t.as_str())
            .map(|s| s.to_string());
        let tool_use_id = block
            .get("id")
            .and_then(|t| t.as_str())
            .map(|s| s.to_string());
        let input = block
            .get("input")
            .map(|i| serde_json::to_string_pretty(i).unwrap_or_default());
        // tool_result：output 字段是文本内容。
        let output = block
            .get("output")
            .and_then(|o| o.as_str())
            .map(|s| s.to_string());
        let is_error = block.get("is_error").and_then(|e| e.as_bool());
        parts.push(SessionPart {
            kind,
            text,
            tool_name,
            tool_use_id,
            input,
            output,
            is_error,
        });
    }
    Some(SessionMessage { role, parts })
}

/// 内嵌的单页 HTML。通过 CDN 引入 marked.js 做 markdown 渲染，
/// 样式与 atomcode webui 保持一致（深色背景、对话气泡、工具调用折叠块）。
fn index_html() -> &'static str {
    include_str!("../assets/webui_index.html")
}
