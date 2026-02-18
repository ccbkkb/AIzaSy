use axum::{
    body::Body,
    extract::{Request, State},
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
use tracing::{error, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

// --- 配置结构体 ---
#[derive(Parser, Debug, Clone)]
#[command(author, version, about = "Aizasy Gemini Gateway")]
struct Args {
    /// 监听地址
    /// 环境变量: AIZASY_LISTEN
    #[arg(short, long, env = "AIZASY_LISTEN", default_value = "0.0.0.0:3000")]
    listen: String,

    /// SOCKS5 代理地址 (例如: socks5://127.0.0.1:40000)
    /// 环境变量: AIZASY_PROXY
    #[arg(short, long, env = "AIZASY_PROXY")]
    proxy: Option<String>,

    /// 目标 API 地址
    /// 环境变量: AIZASY_TARGET
    #[arg(short, long, env = "AIZASY_TARGET", default_value = "https://generativelanguage.googleapis.com")]
    target: String,

    /// 日志级别 (info, debug, warn, error)
    /// 环境变量: AIZASY_LOG
    #[arg(long, env = "AIZASY_LOG", default_value = "info")]
    log_level: String,
}

// --- 应用状态 ---
#[derive(Clone)]
struct AppState {
    client: Client,
    target_url: String,
}

#[tokio::main]
async fn main() {
    // 1. 解析参数 (CLI > ENV > Default)
    let args = Args::parse();

    // 2. 初始化日志系统
    let log_level = args.log_level.parse().unwrap_or(tracing::Level::INFO);
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(tracing_subscriber::EnvFilter::new(args.log_level.clone()))
        .init();

    info!("🚀 正在启动 Aizasy Gateway...");
    info!("⚙️ 配置: 监听={}, 目标={}", args.listen, args.target);

    // 3. 构建 HTTP 客户端
    let mut client_builder = Client::builder()
        .timeout(Duration::from_secs(120)) // 稍微调大超时，防止流式传输断开
        .pool_idle_timeout(Duration::from_secs(90))
        .no_gzip(); // 禁用自动解压，透传流量

    // 配置代理
    if let Some(proxy_url) = &args.proxy {
        info!("🔌 启用代理: {}", proxy_url);
        match Proxy::all(proxy_url) {
            Ok(proxy) => {
                client_builder = client_builder.proxy(proxy);
            }
            Err(e) => {
                error!("❌ 代理配置无效: {}", e);
                std::process::exit(1);
            }
        }
    } else {
        warn!("⚠️ 未配置代理，将使用直连 (如服务器在国内或IP被封可能无法访问)");
    }

    let client = client_builder.build().expect("无法构建 HTTP 客户端");

    // 4. 设置共享状态
    let state = Arc::new(AppState {
        client,
        target_url: args.target.trim_end_matches('/').to_string(), // 去掉末尾斜杠
    });

    // 5. 构建路由
    let app = Router::new()
        .route("/health", get(health_check)) // 健康检查
        .route("/{*path}", any(proxy_handler)) // 捕获所有路径
        .route("/", any(proxy_handler)) // 捕获根路径
        .with_state(state);

    // 6. 启动服务
    let addr: SocketAddr = args.listen.parse().expect("无效的监听地址格式");
    info!("🎧 服务监听于: http://{}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

// --- 处理函数 ---

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
    // 1. 拼接目标 URL
    let path = uri.path_and_query().map(|x| x.as_str()).unwrap_or("/");
    let target_uri = format!("{}{}", state.target_url, path);

    // 2. 清洗 Headers
    // 必须移除 Host，否则 Google 会报错
    // 必须移除 CF 相关头，保护隐私并防止上游混淆
    let mut new_headers = headers.clone();
    new_headers.remove("host");
    new_headers.remove("cf-connecting-ip");
    new_headers.remove("cf-ipcountry");
    new_headers.remove("x-forwarded-for");
    new_headers.remove("x-real-ip");

    // 记录简略日志 (Debug 级别)
    tracing::debug!("-> {} {}", method, target_uri);

    // 3. 发起请求
    let request_builder = state.client
        .request(method, target_uri)
        .headers(new_headers)
        .body(req_body);

    match request_builder.send().await {
        Ok(response) => {
            let status = response.status();
            let mut resp_headers = HeaderMap::new();
            
            // 复制响应头
            for (k, v) in response.headers() {
                resp_headers.insert(k, v.clone());
            }

            // 返回流式 Body (支持打字机效果)
            let body = Body::from_stream(response.bytes_stream());
            
            (status, resp_headers, body).into_response()
        }
        Err(e) => {
            error!("❌ 请求上游失败: {}", e);
            (
                StatusCode::BAD_GATEWAY,
                format!("Gateway Error: {}", e),
            ).into_response()
        }
    }
}
