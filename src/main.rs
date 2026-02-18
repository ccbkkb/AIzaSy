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

// --- 1. 配置参数 ---
#[derive(Parser, Debug, Clone)]
#[command(author, version, about = "Aizasy Gateway")]
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

    /// 忽略 SSL 验证 (不安全模式)
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

    info!("🚀 Aizasy Gateway 启动中...");
    info!("⚙️  Config: Listen={}, Target={}", args.listen, args.target);

    // --- 2. 高性能 Client 构建 ---
    let mut client_builder = Client::builder()
        // 连接池配置: 复用 TCP 连接，极大降低延迟
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(50)
        // TCP 配置: 禁用 Nagle 算法，适合 API 类请求
        .tcp_nodelay(true)
        // 超时配置
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        // 禁用 Gzip 自动解压: 直接透传，省 CPU
        .no_gzip();

    // 代理设置
    if let Some(proxy_url) = &args.proxy {
        info!("🔌 代理已启用: {}", proxy_url);
        let proxy = Proxy::all(proxy_url).expect("代理地址格式错误");
        client_builder = client_builder.proxy(proxy);
    }

    // SSL 设置
    if args.insecure {
        warn!("⚠️  不安全模式: SSL 证书验证已禁用!");
        client_builder = client_builder.danger_accept_invalid_certs(true);
    }

    let client = client_builder.build().expect("Client 构建失败");

    let state = Arc::new(AppState {
        client,
        target_url: args.target.trim_end_matches('/').to_string(),
    });

    // --- 3. 路由构建 ---
    let app = Router::new()
        .route("/health", get(health_check))
        .route("/{*path}", any(proxy_handler))
        .route("/", any(proxy_handler))
        .with_state(state);

    let addr: SocketAddr = args.listen.parse().expect("监听地址无效");
    info!("🎧 服务监听于: {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn health_check() -> impl IntoResponse {
    (StatusCode::OK, "OK")
}

// --- 4. 核心代理逻辑 ---
async fn proxy_handler(
    State(state): State<Arc<AppState>>,
    // 使用 Request<Body> 获取完整的请求对象
    req: Request<Body>, 
) -> impl IntoResponse {
    // 提取 URI
    let path = req.uri().path_and_query().map(|x| x.as_str()).unwrap_or("/");
    let target_uri = format!("{}{}", state.target_url, path);
    let method = req.method().clone();
    let headers = req.headers().clone();

    debug!("-> {} {}", method, target_uri);

    // 提取 Body
    // 关键步骤：显式读取 Body 为 Bytes，解决类型不匹配问题
    // 限制最大 64MB (防止恶意内存攻击)
    let req_body = req.into_body();
    let req_bytes = match axum::body::to_bytes(req_body, 64 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(e) => {
            error!("❌ 读取请求体失败: {}", e);
            return (StatusCode::BAD_REQUEST, "Request body too large or invalid").into_response();
        }
    };

    // 清洗 Headers
    let mut new_headers = headers.clone();
    new_headers.remove("host");
    new_headers.remove("cf-connecting-ip");
    new_headers.remove("cf-ipcountry");
    new_headers.remove("x-forwarded-for");
    // 让 Reqwest 重新计算 Content-Length
    new_headers.remove("content-length"); 

    // 发起请求
    // 注意：.body(req_bytes) 绝对安全，因为 req_bytes 是 bytes::Bytes 类型
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

            // 响应部分保持流式 (Streaming)
            // 这样 Google 的流式回复可以实时传回给用户，不需要等待
            let resp_stream = response.bytes_stream();
            let body = Body::from_stream(resp_stream);
            
            (status, resp_headers, body).into_response()
        }
        Err(e) => {
            error!("❌ 上游请求错误: {}", e);
            (StatusCode::BAD_GATEWAY, format!("Gateway Error: {}", e)).into_response()
        }
    }
}
