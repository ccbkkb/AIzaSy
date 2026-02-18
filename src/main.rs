use axum::{
    body::Body,
    extract::{State},
    http::{HeaderMap, Method, Uri, StatusCode},
    response::{IntoResponse, Response},
    routing::{any, get},
    Router,
};
use clap::Parser;
use reqwest::{Client, Proxy};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn, debug};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

// --- 配置结构体 ---
#[derive(Parser, Debug, Clone)]
#[command(author, version, about = "Aizasy High-Perf Gateway")]
struct Args {
    /// 监听地址
    #[arg(short, long, env = "AIZASY_LISTEN", default_value = "0.0.0.0:3000")]
    listen: String,

    /// SOCKS5 代理地址
    #[arg(short, long, env = "AIZASY_PROXY")]
    proxy: Option<String>,

    /// 目标 API 地址
    #[arg(short, long, env = "AIZASY_TARGET", default_value = "https://generativelanguage.googleapis.com")]
    target: String,

    /// 忽略 SSL 证书验证
    #[arg(long, env = "AIZASY_INSECURE", default_value = "false")]
    insecure: bool,

    /// 日志级别
    #[arg(long, env = "AIZASY_LOG", default_value = "info")]
    log_level: String,
}

#[derive(Clone)]
struct AppState {
    client: Client,
    target_url: String,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    // 初始化日志
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(tracing_subscriber::EnvFilter::new(args.log_level.clone()))
        .init();

    info!("🚀 启动 Aizasy 高性能网关 (Stable Build)...");
    info!("⚙️  监听: {}", args.listen);
    info!("🎯 目标: {}", args.target);

    // --- 高性能 Client 构建 ---
    let mut client_builder = Client::builder()
        // 连接池优化: 保持 50 个长连接，空闲 90 秒回收
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(50) 
        // TCP 优化: 禁用 Nagle 算法，降低 API 延迟
        .tcp_nodelay(true)
        // 超时设置
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120)) 
        // 不自动解压 gzip，透传数据以降低 CPU 负载
        .no_gzip(); 

    if let Some(proxy_url) = &args.proxy {
        info!("🔌 启用代理: {}", proxy_url);
        match Proxy::all(proxy_url) {
            Ok(proxy) => { client_builder = client_builder.proxy(proxy); }
            Err(e) => {
                error!("❌ 代理配置无效: {}", e);
                std::process::exit(1);
            }
        }
    }

    if args.insecure {
        warn!("⚠️  已开启【忽略 SSL 验证】模式");
        client_builder = client_builder.danger_accept_invalid_certs(true);
    }

    let client = client_builder.build().expect("Client build failed");

    let state = Arc::new(AppState {
        client,
        target_url: args.target.trim_end_matches('/').to_string(),
    });

    let app = Router::new()
        .route("/health", get(health_check))
        .route("/{*path}", any(proxy_handler))
        .route("/", any(proxy_handler))
        .with_state(state);

    let addr: SocketAddr = args.listen.parse().expect("无效的监听地址");
    
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn health_check() -> impl IntoResponse {
    (StatusCode::OK, "Aizasy Gateway is running!")
}

async fn proxy_handler(
    State(state): State<Arc<AppState>>,
    method: Method,
    headers: HeaderMap,
    uri: Uri,
    req_body: Body,
) -> impl IntoResponse {
    let path = uri.path_and_query().map(|x| x.as_str()).unwrap_or("/");
    let target_uri = format!("{}{}", state.target_url, path);

    // --- 修复编译错误的关键 ---
    // Axum Body -> Bytes (内存缓冲) -> Reqwest Body
    // 限制最大 16MB，防止恶意大包攻击
    let req_bytes = match axum::body::to_bytes(req_body, 16 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            error!("❌ 读取请求体失败: {}", e);
            return (StatusCode::BAD_REQUEST, "Body too large or invalid").into_response();
        }
    };

    let mut new_headers = headers.clone();
    new_headers.remove("host");
    new_headers.remove("cf-connecting-ip");
    new_headers.remove("cf-ipcountry");
    new_headers.remove("x-forwarded-for");
    
    debug!("-> {} {}", method, target_uri);

    // Bytes 实现了 Into<reqwest::Body>，所以这里绝对能编译通过
    let request_builder = state.client
        .request(method, target_uri)
        .headers(new_headers)
        .body(req_bytes); 

    match request_builder.send().await {
        Ok(response) => {
            let status = response.status();
            let mut resp_headers = HeaderMap::new();
            for (k, v) in response.headers() {
                resp_headers.insert(k, v.clone());
            }
            
            // 响应依然是流式的，这才是最关键的（因为 Google 回复可能很长）
            let resp_stream = response.bytes_stream();
            let body = Body::from_stream(resp_stream);
            
            (status, resp_headers, body).into_response()
        }
        Err(e) => {
            error!("❌ Gateway Error: {}", e);
            (StatusCode::BAD_GATEWAY, format!("Proxy Error: {}", e)).into_response()
        }
    }
}
