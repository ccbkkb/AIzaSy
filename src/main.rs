use axum::{
    body::Body,
    extract::{State, Request},
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

// --- 配置参数 ---
#[derive(Parser, Debug, Clone)]
#[command(author, version, about = "Aizasy Gateway")]
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

    info!("🚀 Aizasy Gateway Starting...");

    // --- 构建 HTTP 客户端 ---
    let mut client_builder = Client::builder()
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(50)
        .tcp_nodelay(true)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        .no_gzip();

    if let Some(proxy_url) = &args.proxy {
        info!("🔌 Proxy: {}", proxy_url);
        let proxy = Proxy::all(proxy_url).expect("Invalid proxy URL");
        client_builder = client_builder.proxy(proxy);
    }

    if args.insecure {
        warn!("⚠️  Insecure Mode: SSL validation disabled");
        client_builder = client_builder.danger_accept_invalid_certs(true);
    }

    let client = client_builder.build().expect("Failed to build client");

    let state = Arc::new(AppState {
        client,
        target_url: args.target.trim_end_matches('/').to_string(),
    });

    let app = Router::new()
        .route("/health", get(health_check))
        .route("/{*path}", any(proxy_handler))
        .route("/", any(proxy_handler))
        .with_state(state);

    let addr: SocketAddr = args.listen.parse().expect("Invalid listen address");
    info!("🎧 Listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn health_check() -> impl IntoResponse {
    (StatusCode::OK, "OK")
}

// --- 核心处理函数 ---
async fn proxy_handler(
    State(state): State<Arc<AppState>>,
    // 这里我们接收一个通用的 Request
    req: Request, 
) -> impl IntoResponse {
    // 1. 提取路径和查询参数
    let path = req.uri().path_and_query().map(|x| x.as_str()).unwrap_or("/");
    let target_uri = format!("{}{}", state.target_url, path);
    let method = req.method().clone();
    let headers = req.headers().clone();

    debug!("-> {} {}", method, target_uri);

    // 2. 关键修复：显式读取 Body
    // 将 Axum 的 Body 转换为 Bytes。Reqwest 原生支持 Bytes。
    // 设置 64MB 限制，防止内存溢出
    let req_body = req.into_body();
    let req_bytes = match axum::body::to_bytes(req_body, 64 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(e) => {
            error!("Failed to read request body: {}", e);
            return (StatusCode::BAD_REQUEST, "Body too large or invalid").into_response();
        }
    };

    // 3. 清洗 Headers
    let mut new_headers = headers.clone();
    new_headers.remove("host");
    new_headers.remove("cf-connecting-ip");
    new_headers.remove("cf-ipcountry");
    new_headers.remove("x-forwarded-for");
    new_headers.remove("content-length"); // 让 reqwest 重新计算

    // 4. 发送请求
    // .body(req_bytes) 这里传入的是 bytes::Bytes 类型
    // 编译器看到这里会非常高兴，因为 reqwest::Body 实现 From<Bytes>
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

            // 5. 响应流式转发 (Streaming)
            // 这里我们保持流式，以支持打字机效果
            let resp_stream = response.bytes_stream();
            let body = Body::from_stream(resp_stream);
            
            (status, resp_headers, body).into_response()
        }
        Err(e) => {
            error!("Proxy error: {}", e);
            (StatusCode::BAD_GATEWAY, format!("Gateway Error: {}", e)).into_response()
        }
    }
}
