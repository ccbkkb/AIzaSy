use axum::{
    body::Body,
    extract::{State, Request},
    http::{HeaderMap, Method, Uri, StatusCode},
    response::{IntoResponse, Response},
    routing::{any, get},
    Router,
};
use clap::Parser;
use futures_util::TryStreamExt; // 关键：让流支持 map_err
use reqwest::{Client, Proxy};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn, debug};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

// --- 配置部分 ---
#[derive(Parser, Debug, Clone)]
#[command(author, version, about = "Aizasy High-Perf Gateway")]
struct Args {
    #[arg(short, long, env = "AIZASY_LISTEN", default_value = "0.0.0.0:3000")]
    listen: String,

    #[arg(short, long, env = "AIZASY_PROXY")]
    proxy: Option<String>,

    #[arg(short, long, env = "AIZASY_TARGET", default_value = "https://generativelanguage.googleapis.com")]
    target: String,

    #[arg(long, env = "AIZASY_INSECURE", default_value = "false")]
    insecure: bool,

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

    info!("🚀 启动 Aizasy 流式网关 (Stream Mode)...");

    // --- 构建高性能 Client ---
    let mut client_builder = Client::builder()
        // 1. 连接池调优：保持 50 个长连接，空闲 90 秒回收
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(50)
        // 2. TCP 调优：禁用 Nagle 算法，降低 API 延迟
        .tcp_nodelay(true)
        // 3. 超时设置：连接 10s，传输不设硬限(为了流式)，但可设 keepalive
        .connect_timeout(Duration::from_secs(10))
        .http2_keep_alive_interval(Duration::from_secs(20))
        // 4. 关键：禁用自动 gzip 解压，直接透传二进制流，极大降低 CPU 消耗
        .no_gzip();

    // 代理配置
    if let Some(proxy_url) = &args.proxy {
        info!("🔌 启用代理: {}", proxy_url);
        let proxy = Proxy::all(proxy_url).expect("代理地址格式错误");
        client_builder = client_builder.proxy(proxy);
    }

    // 忽略 SSL
    if args.insecure {
        warn!("⚠️  警告：已忽略 SSL 证书验证！");
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

    let addr: SocketAddr = args.listen.parse().expect("Invalid address");
    info!("🎧 监听于: {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    // 优雅关闭支持
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();
}

async fn health_check() -> impl IntoResponse {
    (StatusCode::OK, "Aizasy Gateway is running (Stream Mode)")
}

// --- 核心处理逻辑 ---
async fn proxy_handler(
    State(state): State<Arc<AppState>>,
    method: Method,
    headers: HeaderMap,
    uri: Uri,
    req: Request<Body>, // 获取原始 Request 以便提取 Body Stream
) -> impl IntoResponse {
    let path = req.uri().path_and_query().map(|x| x.as_str()).unwrap_or("/");
    let target_uri = format!("{}{}", state.target_url, path);

    // --- 1. 处理请求头 ---
    let mut new_headers = headers.clone();
    new_headers.remove("host");
    new_headers.remove("cf-connecting-ip");
    new_headers.remove("cf-ipcountry");
    new_headers.remove("x-forwarded-for");
    // 移除 content-length，因为如果是 http2 流式传输，长度可能是未知的
    // reqwest 会自动根据 body 类型决定是加 content-length 还是 chunked
    new_headers.remove("content-length");

    debug!("-> {} {}", method, target_uri);

    // --- 2. 真正优雅的流式转换 (Zero-Copy) ---
    // Axum Body -> Data Stream -> IO Error Mapped Stream -> Reqwest Body
    let req_body = req.into_body();
    
    // into_data_stream() 提取数据帧，忽略 Trailers
    // map_err 将 Axum 的错误转换为 std::io::Error，这是 Reqwest 接受流的前提
    let stream = req_body.into_data_stream().map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::Other, e)
    });

    // 将流封装为 Reqwest Body
    let reqwest_body = reqwest::Body::wrap_stream(stream);

    // --- 3. 发送请求 ---
    let request_builder = state.client
        .request(method, target_uri)
        .headers(new_headers)
        .body(reqwest_body); // 这里传入的是流，不是内存块

    match request_builder.send().await {
        Ok(response) => {
            let status = response.status();
            let mut resp_headers = HeaderMap::new();
            for (k, v) in response.headers() {
                resp_headers.insert(k, v.clone());
            }

            // --- 4. 响应流式透传 ---
            // 同样，这里直接把 Reqwest 的下载流丢给 Axum 的响应
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

// 优雅关闭信号监听
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    info!("🛑 正在关闭服务...");
}
