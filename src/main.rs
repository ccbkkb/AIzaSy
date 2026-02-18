use axum::{
    body::Body,
    extract::{State},
    http::{HeaderMap, Method, Uri, StatusCode},
    response::{IntoResponse, Response},
    routing::{any, get},
    Router,
};
use clap::Parser;
use futures_util::TryStreamExt; // 关键：让流可以被转换
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

    /// 忽略 SSL 证书验证 (用于自签证书场景)
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

    info!("🚀 启动 Aizasy 高性能网关...");
    info!("⚙️  监听: {}", args.listen);
    info!("🎯 目标: {}", args.target);

    // --- 高性能 Client 构建 ---
    let mut client_builder = Client::builder()
        // 1. 连接池配置 (复用连接，减少握手)
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(50) // 针对 Google 保持 50 个长连接
        // 2. TCP 层面优化
        .tcp_keepalive(Duration::from_secs(60))
        .tcp_nodelay(true)
        // 3. 超时设置
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120)) // 总超时，流式传输需要长一点
        // 4. HTTP2 支持
        .http2_keep_alive_interval(Duration::from_secs(20))
        .no_gzip(); // 透传压缩数据，减少 CPU 消耗

    // 配置代理
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

    // 配置 SSL 忽略
    if args.insecure {
        warn!("⚠️  已开启【忽略 SSL 验证】模式，请确保你了解安全风险！");
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
    req_body: Body, // Axum Body
) -> impl IntoResponse {
    let path = uri.path_and_query().map(|x| x.as_str()).unwrap_or("/");
    let target_uri = format!("{}{}", state.target_url, path);

    // --- 核心优化: 零拷贝流式转换 ---
    // 将 Axum 的 Body Stream 映射为 Reqwest 可接受的 Stream
    // 这样数据来多少发多少，不占用网关内存
    let req_stream = req_body.into_data_stream().map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::Other, e)
    });
    let reqwest_body = reqwest::Body::wrap_stream(req_stream);

    // Header 清洗
    let mut new_headers = headers.clone();
    new_headers.remove("host");
    new_headers.remove("cf-connecting-ip");
    new_headers.remove("cf-ipcountry");
    new_headers.remove("x-forwarded-for");
    
    debug!("-> {} {}", method, target_uri);

    // 发起请求
    let request_builder = state.client
        .request(method, target_uri)
        .headers(new_headers)
        .body(reqwest_body); // 直接传入流

    match request_builder.send().await {
        Ok(response) => {
            let status = response.status();
            let mut resp_headers = HeaderMap::new();
            for (k, v) in response.headers() {
                resp_headers.insert(k, v.clone());
            }
            
            // 响应体也是流式的
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
