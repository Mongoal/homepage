//! Homepage 后端：极简单文件存储 + 静态首页托管 + favicon 代理。
//!
//! 设计目标：低常驻内存、单文件持久化、根文件系统只读友好。
//! - HTTP 由 axum 提供；运行时使用 single-thread tokio 以省去多线程 worker 栈池。
//! - 首页 index.html 在编译期 `include_str!` 嵌入二进制作为默认/兜底；运行时可由
//!   HOMEPAGE_INDEX_FILE 指向的挂载文件覆盖（改文件 + 重启即生效，无需重新编译）。
//! - 站点数据存在单个 JSON 文件，写入走 原子 temp + rename。
//! - favicon 由后端代理获取（规避浏览器混合内容拦截），缓存到 data/icons/。
//!
//! 运行环境变量：
//!   HOMEPAGE_DATA_DIR  数据目录（默认 /data），数据与图标缓存均在此。
//!   HOMEPAGE_ADDR      监听地址（默认 0.0.0.0:8080）。
//!   HOMEPAGE_FILE      数据文件名（默认 sites.json）。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use regex::Regex;
use reqwest::Client;
use serde_json::Value;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use ipnet::IpNet;

// 首页静态文件在编译期嵌入；运行时无文件依赖。
const INDEX_HTML: &str = include_str!("../frontend-dist/index.html");
// 首页 favicon（SVG）：编译期内嵌为默认，运行时可由 data/favicon.svg 覆盖。
const FAVICON_SVG: &[u8] = include_bytes!("../frontend-dist/favicon.svg");

#[derive(Clone)]
struct AppState {
    data_file: Arc<PathBuf>,
    icons_dir: Arc<PathBuf>,
    // 用户手动放置的图片资源库（data/assets/）：前端选图用作站点图标。
    assets_dir: Arc<PathBuf>,
    // 运行时首页 HTML：优先取挂载文件，否则用编译期内嵌的兜底。
    index_html: Arc<String>,
    // 站点 favicon（SVG 字节）：data/favicon.svg 覆盖内嵌默认。
    favicon: Arc<Vec<u8>>,
    // 公网 HTTP 客户端：连接池复用，严格校验 TLS 证书。
    http: Arc<Client>,
    // LAN 客户端：仅对操作员放行的 lan_cidrs 内字面量 IP 使用，接受自签名/过期证书
    // （局域网自建 HTTPS 应用常无公网 CA 证书）。公网目标永远走 http。
    http_lan: Arc<Client>,
    // 配置的 cookie 转发域后缀（如 ".nas.lan" 或 ""=禁用）。
    // 仅对匹配该后缀的目标主机转发 authelia_session，用于解析受 Authelia 保护的域名应用首页。
    cookie_domain: Arc<String>,
    // 操作员显式允许的 LAN CIDR 列表（HOMEPAGE_LAN_CIDRS）。默认空→所有私网 IP 被拒。
    // 配成如 192.168.0.0/16,10.0.0.0/8 后，LAN 模式的内网应用图标可取。
    lan_cidrs: Arc<Vec<IpNet>>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let data_dir = PathBuf::from(std::env::var("HOMEPAGE_DATA_DIR").unwrap_or_else(|_| "/data".into()));
    let data_file = data_dir.join(std::env::var("HOMEPAGE_FILE").unwrap_or_else(|_| "sites.json".into()));
    let icons_dir = data_dir.join("icons");
    let assets_dir = data_dir.join("assets");
    let addr: SocketAddr = std::env::var("HOMEPAGE_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:8080".into())
        .parse()?;
    // 留空 = 禁用 cookie 转发（最安全，域名应用首页会被 Authelia 挡住，退化为 /favicon.ico 直取）。
    // 配成 ".nas.lan" 后，对 *.nas.lan 目标转发 authelia_session，可解析受保护应用的高清图标。
    let cookie_domain = std::env::var("HOMEPAGE_COOKIE_DOMAIN").unwrap_or_default();
    // 显式 LAN CIDR 允许列表（逗号分隔）。默认空 = 私网 IP 一律拒绝（SSRF 默认安全）。
    let lan_cidrs: Vec<IpNet> = std::env::var("HOMEPAGE_LAN_CIDRS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect();

    // 数据与图标目录必须存在；不存在则创建（首次启动）。
    fs::create_dir_all(&data_dir).await?;
    fs::create_dir_all(&icons_dir).await?;
    fs::create_dir_all(&assets_dir).await?;

    // 首页 HTML：启动时一次性读入内存缓存。
    // HOMEPAGE_INDEX_FILE 指向的文件存在则用它（改文件 + 重启即生效，无需重新编译）；
    // 不存在/读取失败则回落到编译期内嵌的 INDEX_HTML（镜像永远自包含）。
    let index_file = std::env::var("HOMEPAGE_INDEX_FILE")
        .unwrap_or_else(|_| data_dir.join("index.html").to_string_lossy().into_owned());
    let index_html: String = match fs::read_to_string(&index_file).await {
        Ok(s) if !s.trim().is_empty() => {
            eprintln!("首页取自挂载文件 {index_file}（运行时覆盖）");
            s
        }
        _ => {
            eprintln!("首页用编译期内嵌版本（未找到/空 {index_file}）");
            INDEX_HTML.to_string()
        }
    };

    // favicon：data/favicon.svg 存在则用，否则内嵌默认。
    let favicon: Vec<u8> = match fs::read(data_dir.join("favicon.svg")).await {
        Ok(b) if !b.is_empty() => b,
        _ => FAVICON_SVG.to_vec(),
    };

    // 公网客户端：超时 6s。禁用自动重定向——由 get_follow 手动跟随，
    // 以便对每个跳转目标重新做 SSRF 校验与 cookie 重新作用域（避免重定向把 SSO
    // cookie 带到非配置域，或跳进内网/元数据地址）。严格校验 TLS 证书。
    let http = Client::builder()
        .timeout(Duration::from_secs(6))
        .connect_timeout(Duration::from_secs(3))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("homepage-icon-fetcher")
        .build()?;

    // LAN 客户端：同超时/重定向策略，但 danger_accept_invalid_certs=true 接受自签名证书
    // （等价 curl -k）。仅在 is_lan_literal_target 命中（目标为 lan_cidrs 内字面量 IP）时使用；
    // 公网域名/IP 永远走 http，证书校验不打折扣。
    let http_lan = Client::builder()
        .timeout(Duration::from_secs(6))
        .connect_timeout(Duration::from_secs(3))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("homepage-icon-fetcher")
        .danger_accept_invalid_certs(true)
        .build()?;

    let state = AppState {
        data_file: Arc::new(data_file),
        icons_dir: Arc::new(icons_dir),
        assets_dir: Arc::new(assets_dir),
        index_html: Arc::new(index_html),
        favicon: Arc::new(favicon),
        http: Arc::new(http),
        http_lan: Arc::new(http_lan),
        cookie_domain: Arc::new(cookie_domain.clone()),
        lan_cidrs: Arc::new(lan_cidrs.clone()),
    };

    let app = Router::new()
        .route("/", get(index_handler))
        .route("/index.html", get(index_handler))
        .route("/favicon.svg", get(favicon_handler))
        .route("/favicon.ico", get(favicon_handler))
        .route("/api/sites", get(get_sites).post(put_sites))
        .route("/api/export", get(export_handler))
        .route("/api/import", post(import_handler))
        .route("/api/icon", get(icon_handler))
        .route("/api/assets", get(list_assets_handler))
        .route("/assets/:name", get(asset_handler))
        .route("/api/health", get(health_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    eprintln!(
        "homepage 监听 {addr}，数据目录 {}，cookie 转发域：{}",
        std::env::var("HOMEPAGE_DATA_DIR").unwrap_or_else(|_| "/data".into()),
        if cookie_domain.is_empty() { "禁用".into() } else { cookie_domain }
    );

    axum::serve(listener, app.into_make_service()).await?;
    Ok(())
}

// ---- 处理器 ----

async fn index_handler(State(state): State<AppState>) -> Response {
    (
        [(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"))],
        (*state.index_html).clone(),
    )
        .into_response()
}

async fn health_handler() -> impl IntoResponse {
    StatusCode::NO_CONTENT
}

/// 站点 favicon（SVG）：复用图标响应的安全头（防 SVG XSS）。
async fn favicon_handler(State(state): State<AppState>) -> Response {
    icon_response(state.favicon.to_vec(), "image/svg+xml")
}

/// 列出资源库（data/assets/）下的图片文件名，供前端选图。
async fn list_assets_handler(State(state): State<AppState>) -> Response {
    let mut names: Vec<String> = Vec::new();
    if let Ok(mut rd) = fs::read_dir(&*state.assets_dir).await {
        while let Ok(Some(entry)) = rd.next_entry().await {
            if let Some(name) = entry.file_name().to_str() {
                if ctype_for_filename(name).is_some() {
                    names.push(name.to_string());
                }
            }
        }
    }
    names.sort();
    Json(names).into_response()
}

/// 服务单张资源库图片：路径穿越防护 + 仅图片扩展名 + CSP（防 SVG XSS）。
async fn asset_handler(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    // 拒绝路径穿越：只允许纯文件名（无目录分隔、无 ..）。
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let ctype = match ctype_for_filename(&name) {
        Some(ct) => ct,
        None => return StatusCode::NOT_FOUND.into_response(), // 非图片扩展名不暴露
    };
    let path = state.assets_dir.join(&name);
    let bytes = match fs::read(&path).await {
        Ok(b) => b,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    // 复用图标响应的安全头（CSP default-src 'none' 防 SVG 脚本执行）。
    icon_response(bytes, ctype)
}

/// 按文件名扩展名推断图片 MIME；非图片返回 None（用于列表过滤与响应类型）。
fn ctype_for_filename(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".svg") {
        Some("image/svg+xml")
    } else if lower.ends_with(".png") {
        Some("image/png")
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        Some("image/jpeg")
    } else if lower.ends_with(".webp") {
        Some("image/webp")
    } else if lower.ends_with(".gif") {
        Some("image/gif")
    } else if lower.ends_with(".ico") {
        Some("image/x-icon")
    } else {
        None
    }
}

/// 读取站点数据。文件不存在或为空时返回空对象 `{}`，不视为错误。
async fn get_sites(State(state): State<AppState>) -> Response {
    read_json(&state.data_file).await.into_response()
}

/// 整体替换站点数据。要求 body 是合法 JSON（任意结构）。
async fn put_sites(State(state): State<AppState>, Json(value): Json<Value>) -> Response {
    match write_json_atomic(&state.data_file, &value).await {
        Ok(()) => Json(value).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// 导出：等同 GET /api/sites，语义上更显式。
async fn export_handler(State(state): State<AppState>) -> Response {
    read_json(&state.data_file).await.into_response()
}

/// 导入：等同 PUT /api/sites，整体替换。
async fn import_handler(State(state): State<AppState>, Json(value): Json<Value>) -> Response {
    match write_json_atomic(&state.data_file, &value).await {
        Ok(()) => Json(value).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct IconQuery {
    url: String,
}

/// favicon 代理：取原始高清图标。
///
/// 流程：缓存命中 → 直接返回；否则：
///   1. 解析目标首页 HTML 的 <link rel=icon / apple-touch-icon>，按 矢量 > 大尺寸 > 普通图标 排序，
///      取最优那个再抓（域名应用需带 authelia_session 过 Authelia）。
///   2. 全失败 → 回退 origin/favicon.ico。
/// 失败返回 404，前端据此回退首字母色块。
///
/// cookie 转发：仅当配置了 HOMEPAGE_COOKIE_DOMAIN 且目标主机匹配该后缀时，
/// 把入站请求里的 authelia_session 转发给目标。LAN 目标（IP）永远不带 cookie。
async fn icon_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<IconQuery>,
) -> Response {
    let origin = match parse_origin(&q.url) {
        Some(o) => o,
        None => return StatusCode::BAD_REQUEST.into_response(),
    };

    let key = sha1_hex(origin.as_bytes());
    // 先看缓存里有没有任意扩展名的命中。
    if let Some(cached) = load_cached_icon(&state.icons_dir, &key).await {
        return icon_response(cached.bytes, cached.ctype);
    }

    // 抽取入站 authelia_session（仅此一个 cookie）；是否转发交由每次子请求按目标主机判定。
    let session = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(extract_authelia_session);

    // SSRF 防护：拦截私有/回环/链路本地/元数据目标（域名解析后由调用方二次校验见下）。
    if egress_allowed(&origin, &state.lan_cidrs).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }

    // 拉取：先解析 HTML 找高清，回退 /favicon.ico。cookie 由每个子请求按目标主机决定是否带。
    match fetch_icon(
        &state.http,
        &state.http_lan,
        &state.icons_dir,
        &key,
        &origin,
        session.as_deref(),
        &state.cookie_domain,
        &state.lan_cidrs,
    )
    .await
    {
        Some(fetched) => icon_response(fetched.bytes, fetched.ctype),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// 从 Cookie 头抽取 authelia_session=<val>。
fn extract_authelia_session(raw: &str) -> Option<String> {
    for pair in raw.split(';') {
        let p = pair.trim();
        if let Some(v) = p.strip_prefix("authelia_session=") {
            if !v.is_empty() {
                return Some(format!("authelia_session={v}"));
            }
        }
    }
    None
}

/// 判断某 host 是否属于配置的 cookie 域，且锚定在标签边界。
/// 即 host == base 或 host 以 ".base" 结尾（拒绝 evil.nas.lan 冒充 nas.lan）。
fn host_matches_domain(host: &str, cookie_domain: &str) -> bool {
    if cookie_domain.is_empty() {
        return false;
    }
    let base = cookie_domain.trim_start_matches('.').to_ascii_lowercase();
    let h = host.to_ascii_lowercase();
    h == base || h.ends_with(&format!(".{base}"))
}

/// 从 origin 抽取 host（去掉 scheme 与端口）。仅 host，不含 :port。
fn host_of(origin: &str) -> Option<&str> {
    let rest = origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"))?;
    let host_end = rest.find(':').unwrap_or_else(|| {
        rest.find(|c: char| c == '/' || c == '?' || c == '#').unwrap_or(rest.len())
    });
    let h = &rest[..host_end];
    if h.is_empty() { None } else { Some(h) }
}

/// SSRF 防护：判定一个 URL 是否允许抓取。
/// 规则：scheme 必须 http/https；host 不能是已知内网别名（容器服务名/宿主桥/云元数据）；
/// 若 host 是字面量私网 IP，则仅当它落在操作员显式允许的 lan_cidrs 内才放行。
///
/// 已知局限：此为静态校验，无法拦截"公网域名解析到内网 IP"的 DNS 重绑定。本服务位于
/// Authelia(2FA) 之后、单用户，且响应体需通过 looks_like_image 才回传，风险可接受；
/// 若未来开放多用户应改用 reqwest 自定义 resolver 在拨号时校验解析出的 IP。
fn egress_allowed(url: &str, lan_cidrs: &[IpNet]) -> Option<()> {
    let parsed = url::Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    let host = parsed.host_str()?;
    // 阻止已知内网别名（容器服务名 / 宿主桥 / 云元数据），与 CIDR 无关，一律拒。
    const BLOCKED_HOSTS: &[&str] = &[
        "host.docker.internal",
        "metadata.google.internal",
        "metadata",
        "authelia",
        "caddy",
        "homepage",
        "vaultwarden",
        "siyuan",
    ];
    if BLOCKED_HOSTS.iter().any(|b| host.eq_ignore_ascii_case(b)) {
        return None;
    }
    // 字面量 IP：私网地址仅当在操作员允许的 CIDR 内才放行。
    if let Some(ip) = parse_literal_ip(host) {
        if is_non_public_ip(&ip) {
            if !lan_cidrs.iter().any(|net| net.contains(&ip)) {
                return None;
            }
        }
    }
    Some(())
}

/// 宽松解析字面量 IP：支持 IPv4 点分十进制、整数/十六进制等变体（先经 url crate 规范化），
/// 以及 IPv6。无法解析为 IP 的（域名）返回 None。
fn parse_literal_ip(host: &str) -> Option<std::net::IpAddr> {
    // 先尝试直接解析；再尝试把整数形式 IPv4 转成点分。
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return Some(ip);
    }
    // url crate 对 http://2130706433/ 这类会把 host 报为域名串，这里兜底尝试整数 IPv4。
    if let Ok(n) = host.parse::<u32>() {
        return Some(std::net::Ipv4Addr::from(n).into());
    }
    None
}

/// 判定目标 URL 是否为"操作员放行的 LAN 字面量 IP"。
/// 命中时抓取走 http_lan（接受自签名证书，供局域网自建 HTTPS 应用取图标）；
/// 域名目标与公网 IP 永远返回 false（走严格 http，证书校验不打折扣）。
/// 受控放宽：仅对 HOMEPAGE_LAN_CIDRS 显式放行的内网段、且是字面量 IP（非域名）放宽 TLS。
fn is_lan_literal_target(url: &str, lan_cidrs: &[IpNet]) -> bool {
    let parsed = match url::Url::parse(url) {
        Ok(p) => p,
        Err(_) => return false,
    };
    let host = match parsed.host_str() {
        Some(h) => h,
        None => return false,
    };
    match parse_literal_ip(host) {
        Some(ip) => lan_cidrs.iter().any(|net| net.contains(&ip)),
        None => false,
    }
}

/// 判定 IP 是否属于非公网（私有/回环/链路本地/多播/保留/元数据 169.254.169.254）。
fn is_non_public_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_private()          // 10/8, 172.16/12, 192.168/16
                || v4.is_loopback()   // 127/8
                || v4.is_link_local() // 169.254/16（含云元数据 169.254.169.254）
                || v4.is_unspecified() // 0.0.0.0
                || v4.is_broadcast()
                || v4.is_documentation()
                // 100.64/10 CGNAT、240/4 保留
                || v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64
                || v4.octets()[0] >= 240
        }
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_unicast_link_local() // fe80::/10
                || (v6.octets()[0] == 0xfc || v6.octets()[0] == 0xfd) // ULA fc00::/7
        }
    }
}

// ---- 文件 IO 辅助 ----

/// 读取 JSON 文件；不存在/为空返回 `{}`，损坏返回 500。
async fn read_json(path: &PathBuf) -> Result<Json<Value>, (StatusCode, String)> {
    match fs::read(path).await {
        Ok(bytes) if bytes.iter().any(|b| !b.is_ascii_whitespace()) => {
            serde_json::from_slice(&bytes)
                .map(Json)
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("数据文件解析失败: {e}")))
        }
        Ok(_) => Ok(Json(Value::Object(Default::default()))), // 空文件 → 空 {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok(Json(Value::Object(Default::default()))) // 首次启动
        }
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, format!("读取数据失败: {e}"))),
    }
}

/// 原子写入：写到同目录临时文件 → sync → rename。
/// rename 在同一文件系统上是原子的，崩溃不会留下半截文件。
async fn write_json_atomic(path: &PathBuf, value: &Value) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let bytes = serde_json::to_vec_pretty(value)?;

    let tmp = parent.join(format!(
        ".{}.tmp",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("sites")
    ));

    {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)
            .await?;
        f.write_all(&bytes).await?;
        f.flush().await?;
        // 落盘：拿到底层 std 句柄 sync_all。
        let std_file = f.into_std().await;
        std_file.sync_all()?;
    }
    fs::rename(&tmp, path).await?;
    Ok(())
}

// ---- favicon 抓取辅助 ----

struct FetchedIcon {
    bytes: Vec<u8>,
    ctype: &'static str,
}

struct CachedIcon {
    bytes: Vec<u8>,
    ctype: &'static str,
}

/// 仅取 origin（scheme://host[:port]）。任何路径/查询都丢弃。
fn parse_origin(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let (scheme, rest) = {
        if let Some(rest) = raw.strip_prefix("https://") {
            ("https://", rest)
        } else if let Some(rest) = raw.strip_prefix("http://") {
            ("http://", rest)
        } else {
            return None; // 必须显式 http/https
        }
    };
    // host[:port] 结束于第一个 / ? #
    let host_end = rest.find(|c: char| c == '/' || c == '?' || c == '#').unwrap_or(rest.len());
    let host = &rest[..host_end];
    if host.is_empty() {
        return None;
    }
    Some(format!("{scheme}{host}"))
}

/// 从某条 Content-Type 推断图标扩展名（决定缓存文件后缀与响应类型）。
fn ctype_kind(ct: &str) -> Option<(&'static str, &'static str)> {
    let ct = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    match ct.as_str() {
        "image/svg+xml" => Some(("svg", "image/svg+xml")),
        "image/png" => Some(("png", "image/png")),
        "image/webp" => Some(("webp", "image/webp")),
        "image/jpeg" | "image/jpg" => Some(("jpg", "image/jpeg")),
        "image/gif" => Some(("gif", "image/gif")),
        "image/x-icon" | "image/vnd.microsoft.icon" | "image/icon" => Some(("ico", "image/x-icon")),
        _ => None,
    }
}

/// 仅凭字节内容做轻量"是否真图片"判定：HTML/登录页一律拒绝。
/// - < 拒绝（favicon 至少几百字节）。
/// - 开头是 `<!` / `<h` / `<H` 视为 HTML，拒绝（Authelia 302 登录页会返回 HTML）。
fn looks_like_image(bytes: &[u8], ctype: Option<&str>) -> bool {
    if bytes.len() < 32 {
        return false;
    }
    // 跳过可能的前导空白。
    let start = bytes.iter().position(|b| !b.is_ascii_whitespace()).unwrap_or(0);
    let head = &bytes[start..];
    // HTML / XML 登录页特征。
    if head.starts_with(b"<") {
        // SVG 例外：<svg ... 是合法图标。
        let low: Vec<u8> = head.iter().take(8).map(|b| b.to_ascii_lowercase()).collect();
        if !low.starts_with(b"<svg") && !low.starts_with(b"<?xml") {
            return false;
        }
        // 即便头部像 xml，也必须 Content-Type 声明是 svg 或字节确认 svg。
        return ctype.map(|c| c.contains("svg")).unwrap_or(true);
    }
    true
}

async fn load_cached_icon(dir: &PathBuf, key: &str) -> Option<CachedIcon> {
    for (ext, ctype) in [
        ("svg", "image/svg+xml"),
        ("png", "image/png"),
        ("ico", "image/x-icon"),
        ("webp", "image/webp"),
        ("jpg", "image/jpeg"),
        ("gif", "image/gif"),
    ] {
        let p = dir.join(format!("{key}.{ext}"));
        if let Ok(bytes) = fs::read(&p).await {
            return Some(CachedIcon { bytes, ctype });
        }
    }
    None
}

/// 尝试拉取并落盘。流程：
///   1. 抓首页 HTML，解析 <link>，按评分挑最优高清图标，再抓它。
///   2. 失败回退 origin/favicon.ico。
///
/// 安全：每个子请求独立判定 cookie（仅当其目标主机匹配 cookie_domain 才带），
/// 避免 <link href> 把 SSO cookie 带到攻击者主机。
/// SSRF：每个目标 URL 再过一次 egress_allowed（覆盖 <link href> 解析出的新主机）。
async fn fetch_icon(
    http: &Client,
    http_lan: &Client,
    dir: &PathBuf,
    key: &str,
    origin: &str,
    session: Option<&str>,
    cookie_domain: &str,
    lan_cidrs: &[IpNet],
) -> Option<FetchedIcon> {
    // 路径 1：解析 HTML 找高清图标（best 可能指向别的主机）。
    if let Some(best) =
        best_icon_from_html(http, http_lan, origin, session, cookie_domain, lan_cidrs).await
    {
        if let Some(f) = try_fetch(http, http_lan, dir, key, &best, session, cookie_domain, lan_cidrs).await {
            return Some(f);
        }
    }
    // 路径 2：回退 favicon.ico。
    if let Some(f) = try_fetch(
        http,
        http_lan,
        dir,
        key,
        &format!("{origin}/favicon.ico"),
        session,
        cookie_domain,
        lan_cidrs,
    )
    .await
    {
        return Some(f);
    }
    // 两条路径都失败：打印诊断，便于排查（目标需认证 / 无 favicon / 超时 / 服务不在线）。
    eprintln!("[icon] 抓取失败：{origin}（首页 <link> 与 /favicon.ico 均未取到有效图标）");
    None
}

/// 为给定 URL 决定是否带 cookie：仅当配置了 cookie_domain、目标主机匹配（标签边界）
/// 且入站确有 session 时返回 Cookie 头值。LAN/IP 目标天然不匹配域名 → 不带。
fn cookie_for(url: &str, session: Option<&str>, cookie_domain: &str) -> Option<String> {
    let session = session?;
    let host = host_of(url)?;
    if host_matches_domain(host, cookie_domain) {
        Some(format!("authelia_session={session}"))
    } else {
        None
    }
}

/// 手动跟随重定向（最多 4 跳）。每一跳都对目标 URL 重新做 SSRF 校验与 cookie 作用域，
/// 避免重定向把 SSO cookie 带到非配置域、或跳进内网/元数据地址。
/// 成功（2xx）返回最终响应；3xx 用尽或遇非法目标返回 None。
async fn get_follow(
    http: &Client,
    http_lan: &Client,
    url: &str,
    session: Option<&str>,
    cookie_domain: &str,
    lan_cidrs: &[IpNet],
) -> Option<reqwest::Response> {
    let mut current = url.to_string();
    for _ in 0..5 {
        if egress_allowed(&current, lan_cidrs).is_none() {
            return None;
        }
        // 仅对 lan_cidrs 内的字面量 IP 用宽松 client（接受自签名证书）；其余严格校验。
        let client = if is_lan_literal_target(&current, lan_cidrs) { http_lan } else { http };
        let mut req = client.get(&current);
        if let Some(c) = cookie_for(&current, session, cookie_domain) {
            req = req.header(reqwest::header::COOKIE, c);
        }
        let resp = req.send().await.ok()?;
        let status = resp.status();
        if status.is_success() {
            return Some(resp);
        }
        if !status.is_redirection() {
            return None;
        }
        // 取 Location（相对则按当前 URL 解析）。
        let loc = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())?
            .trim()
            .to_string();
        let base = url::Url::parse(&current).ok()?;
        current = base.join(&loc).ok()?.to_string();
    }
    None
}

/// 抓首页 HTML，解析 <link rel=icon|apple-touch-icon|shortcut icon>，
/// 返回得分最高的图标绝对 URL（origin 相对路径已解析为绝对）。
/// cookie/SSRF 由 get_follow 逐跳处理。
async fn best_icon_from_html(
    http: &Client,
    http_lan: &Client,
    origin: &str,
    session: Option<&str>,
    cookie_domain: &str,
    lan_cidrs: &[IpNet],
) -> Option<String> {
    let resp = get_follow(http, http_lan, origin, session, cookie_domain, lan_cidrs).await?;
    // 限制只读前 256KB：图标 link 通常在 <head>，省内存且防超大页。
    let body = resp.bytes().await.ok()?;
    if body.len() > 262_144 {
        return None;
    }
    let html = String::from_utf8_lossy(&body);
    pick_best_link(&html, origin)
}

/// 一个候选图标的评分：数值越大越优先。
/// 矢量(svg) > apple-touch-icon(≈180) > 大尺寸 PNG > 任意 icon。
struct Candidate {
    href: String,
    score: u32,
}

fn pick_best_link(html: &str, base: &str) -> Option<String> {
    // 抓取所有 <link ...> 完整标签（含自闭合 >），每个标签内再解析属性。
    // 不依赖完整 HTML 解析器：图标 link 形态规整，正则足够。
    let tag_re = Regex::new(r"(?is)<link\b[^>]*>").unwrap();
    let attr_re = Regex::new(r#"(?i)(href|rel|sizes|type)\s*=\s*("([^"]*)"|'([^']*)')"#).unwrap();

    let mut best: Option<Candidate> = None;
    for tag in tag_re.captures_iter(html) {
        let tag_text = tag.get(0).unwrap().as_str();
        let mut href: Option<String> = None;
        let mut rel = String::new();
        let mut sizes: Option<String> = None;
        let mut type_ = String::new();
        for cap in attr_re.captures_iter(tag_text) {
            let key = cap.get(1).map(|m| m.as_str().to_ascii_lowercase()).unwrap_or_default();
            // 值可能用双引号(group3)或单引号(group4)
            let val = cap.get(3).or_else(|| cap.get(4)).map(|m| m.as_str()).unwrap_or("");
            match key.as_str() {
                "href" => href = Some(val.to_string()),
                "rel" => rel = val.to_ascii_lowercase(),
                "sizes" => sizes = Some(val.to_string()),
                "type" => type_ = val.to_ascii_lowercase(),
                _ => {}
            }
        }
        if let Some(score) = score_link(&rel, sizes.as_deref(), &type_) {
            if let Some(h) = href {
                let c = Candidate { href: h, score };
                if best.as_ref().map_or(true, |b| c.score > b.score) {
                    best = Some(c);
                }
            }
        }
    }

    best.and_then(|c| resolve_url(&c.href, base))
}

/// 给一个 <link> 打分。None 表示不是图标 link。
/// svg(矢量) 100000；apple-touch-icon 90000+尺寸；普通 icon 10000+尺寸。
fn score_link(rel: &str, sizes: Option<&str>, type_: &str) -> Option<u32> {
    let rel = rel.split_whitespace().collect::<Vec<_>>();
    let is_icon = rel.iter().any(|r| matches!(*r, "icon" | "apple-touch-icon" | "shortcut" | "apple-touch-icon-precomposed"));
    if !is_icon {
        return None;
    }
    let size = sizes
        .and_then(|s| s.split('x').next())
        .and_then(|n| n.parse::<u32>().ok())
        .unwrap_or(0);
    let is_svg = type_ == "image/svg+xml" || rel.iter().any(|r| r.contains("svg"));
    let is_apple = rel.iter().any(|r| *r == "apple-touch-icon" || *r == "apple-touch-icon-precomposed");
    let score = if is_svg { 100_000 + size }
        else if is_apple { 90_000 + size }
        else { 10_000 + size };
    Some(score)
}

/// 把可能相对的 href 解析成绝对 URL（相对 origin）。
fn resolve_url(href: &str, base: &str) -> Option<String> {
    let base_url = url::Url::parse(base).ok()?;
    base_url.join(href.trim()).ok().map(|u| u.to_string())
}

async fn try_fetch(
    http: &Client,
    http_lan: &Client,
    dir: &PathBuf,
    key: &str,
    url: &str,
    session: Option<&str>,
    cookie_domain: &str,
    lan_cidrs: &[IpNet],
) -> Option<FetchedIcon> {
    let resp = get_follow(http, http_lan, url, session, cookie_domain, lan_cidrs).await?;
    // 先取出 header 元数据，再消费 body，避免借用冲突。
    let ctype_raw = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let bytes = resp.bytes().await.ok()?;

    let (ext, ctype) = ctype_kind(&ctype_raw).unwrap_or(guess_from_magic(&bytes));
    if !looks_like_image(&bytes, Some(ctype)) {
        return None;
    }

    // 落盘缓存（best-effort；失败不影响本次返回）。
    let p = dir.join(format!("{key}.{ext}"));
    if let Ok(mut f) = fs::File::create(&p).await {
        let _ = f.write_all(&bytes).await;
        let _ = f.flush().await;
        let std_file = f.into_std().await;
        let _ = std_file.sync_all();
    }

    Some(FetchedIcon { bytes: bytes.to_vec(), ctype })
}

/// 无 Content-Type 时用 magic bytes 推断图片类型。
fn guess_from_magic(bytes: &[u8]) -> (&'static str, &'static str) {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        ("png", "image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        ("jpg", "image/jpeg")
    } else if bytes.starts_with(b"GIF8") {
        ("gif", "image/gif")
    } else if bytes.starts_with(b"RIFF") && bytes.len() > 11 && &bytes[8..12] == b"WEBP" {
        ("webp", "image/webp")
    } else if bytes.starts_with(&[0, 0, 1, 0]) {
        ("ico", "image/x-icon")
    } else if bytes.starts_with(b"<svg") || bytes.starts_with(b"<?xml") {
        ("svg", "image/svg+xml")
    } else {
        ("ico", "image/x-icon")
    }
}

fn icon_response(bytes: Vec<u8>, ctype: &'static str) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(ctype));
    // 图标内容稳定（按 url 缓存），可被浏览器/proxy 长缓存。
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=604800, immutable"),
    );
    // 防 SVG 文档型 XSS：图标可能含 <script>（来自第三方 SVG）。
    // 经 <img> 加载时浏览器本就禁脚本，但若用户直接导航到 /api/icon?url=...，
    // SVG 会以同源文档执行脚本（可窃取 authelia_session）。
    // default-src 'none' 阻止脚本/网络，同时 <img> 仍可渲染其矢量形状。
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; img-src 'none'; style-src 'none'"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    (StatusCode::OK, headers, bytes).into_response()
}

// ---- 极简 SHA-1 ----
// 避免为单次 key 计算引入额外 crate；SHA-1 实现很短，仅用于缓存文件名，无安全要求。

fn sha1_hex(data: &[u8]) -> String {
    hex(&sha1(data))
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn sha1(data: &[u8]) -> [u8; 20] {
    const H0: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let mut h = H0;

    let ml = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&ml.to_be_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 80];
        for (i, b) in chunk.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for i in 0..80 {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let t = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(w[i]);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    let mut out = [0u8; 20];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}
