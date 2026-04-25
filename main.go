package main

import (
	"context"
	"crypto/tls"
	"fmt"
	"log"
	"net"
	"net/http"
	"net/http/httputil"
	"net/url"
	"os"
	"os/signal"
	"regexp"
	"strings"
	"sync"
	"syscall"
	"time"
)

// 内存复用池
type bytesPool struct {
	pool sync.Pool
}

func (p *bytesPool) Get() [ ]byte {
	return p.pool.Get().([ ]byte)
}

func (p *bytesPool) Put(b [ ]byte) {
	p.pool.Put(b)
}

const indexHTML = `
<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>AIzaSy - Gemini API Gateway</title>
    <style>
        body { font-family: -apple-system, BlinkMacSystemFont, sans-serif; max-width: 720px; margin: 0 auto; padding: 50px 20px; }
        h1 { font-size: 24px; border-bottom: 1px solid #eaecef; padding-bottom: 10px; }
        code, pre { background-color: #f6f8fa; border-radius: 6px; font-family: monospace; }
        pre { padding: 16px; overflow: auto; font-size: 13px; border: 1px solid #d0d7de; }
        .status { color: #1a7f37; font-size: 14px; font-weight: normal; float: right; margin-top: 8px; }
    </style>
</head>
<body>
    <h1>AIzaSy Gateway <span class="status">[🟢 system.status: ok]</span></h1>
    <p>A lightweight, high-performance proxy for the Google Gemini API. Zero logging of keys.</p>
    <p>Usage: Replace <code>generativelanguage.googleapis.com</code> with this gateway's address.</p>
</body>
</html>
`

// 【修复 1】: 全局预编译正则表达式，避免每次请求重复编译导致高并发下 CPU 飙升
var apiKeyRegexp = regexp.MustCompile(`([?&]key=)[^&]+`)

func main() {
	targetURL, _ := url.Parse("https://generativelanguage.googleapis.com")

	// 1. 读取基础配置
	corsOrigin := os.Getenv("CORS_ALLOWED_ORIGINS")
	if corsOrigin == "" {
		corsOrigin = "*"
	}

	// 读取最大流式传输时间配置 (防止僵尸连接，默认 1 小时)
	streamTimeoutEnv := os.Getenv("MAX_STREAM_DURATION")
	maxStreamDuration := 1 * time.Hour
	if streamTimeoutEnv != "" {
		if parsed, err := time.ParseDuration(streamTimeoutEnv); err == nil {
			maxStreamDuration = parsed
		} else {
			log.Printf("⚠️ MAX_STREAM_DURATION 解析失败，将使用默认值 1h: %v", err)
		}
	}

	// 2. 代理路由挂载
	var proxyFunc func(*http.Request) (*url.URL, error)
	if proxyEnv := os.Getenv("PROXY_URL"); proxyEnv != "" {
		proxyURL, err := url.Parse(proxyEnv)
		if err != nil {
			log.Fatalf("❌ 解析 PROXY_URL 错误: %v", err)
		}
		proxyFunc = http.ProxyURL(proxyURL)
		log.Printf("🌐 已挂载出站代理: %s", proxyURL.Redacted())
	} else {
		proxyFunc = http.ProxyFromEnvironment
	}

	// 3. 极致传输层配置
	optimizedTransport := &http.Transport{
		Proxy: proxyFunc,
		DisableCompression: true, // 彻底禁用自动压缩，解决流式卡顿
		DialContext: (&net.Dialer{
			Timeout:   10 * time.Second,
			KeepAlive: 30 * time.Second,
		}).DialContext,
		ForceAttemptHTTP2:     true,
		MaxIdleConns:          10000,
		MaxIdleConnsPerHost:   1000,
		IdleConnTimeout:       90 * time.Second,
		TLSHandshakeTimeout:   10 * time.Second,
		ExpectContinueTimeout: 1 * time.Second,
		TLSClientConfig: &tls.Config{
			ClientSessionCache: tls.NewLRUClientSessionCache(1000),
		},
	}

	reverseProxy := httputil.NewSingleHostReverseProxy(targetURL)
	reverseProxy.Transport = optimizedTransport
	reverseProxy.FlushInterval = -1 // 结合禁用压缩，实现真·毫秒级流式推送
	reverseProxy.BufferPool = &bytesPool{
		pool: sync.Pool{
			New: func() interface{} { return make([ ]byte, 4*1024) },
		},
	}

	// 【修复 3】: 自定义错误处理器，捕获 50MB 载荷超限异常，精准返回 413
	reverseProxy.ErrorHandler = func(w http.ResponseWriter, r *http.Request, err error) {
		// 如果是因为达到 1 小时上限被强杀，或者用户主动关闭了网页/终止了请求，保持静默即可
		if err == context.Canceled || err == context.DeadlineExceeded {
			return
		}
		
		// 拦截 MaxBytesReader 产生的错误
		if strings.Contains(err.Error(), "http: request body too large") {
			log.Printf("⚠️ 请求体超限 (大于 50MB): %s", r.RemoteAddr)
			w.WriteHeader(http.StatusRequestEntityTooLarge)
			fmt.Fprint(w, `{"error": "Payload Too Large: Exceeds 50MB limit"}`)
			return
		}

		log.Printf("⚠️ 代理转发异常: %v", err)
		w.WriteHeader(http.StatusBadGateway)
	}

	// 4. 拦截器：安全处理与脱敏
	originalDirector := reverseProxy.Director
	reverseProxy.Director = func(req *http.Request) {
		originalDirector(req)
		req.Host = targetURL.Host
		req.Header.Del("Accept-Encoding") // 剥离压缩请求
		req.Header.Del("X-Forwarded-For")
		req.Header.Del("X-Real-IP")
		req.Header.Del("True-Client-IP")
		req.Header.Del("CF-Connecting-IP")
		logRequest(req)
	}

	// 【修复 4 & 5】: 清理可能的重复 CORS 标头，并针对流式输出禁用中间代理缓存
	reverseProxy.ModifyResponse = func(resp *http.Response) error {
		// 抹除上游 Google 可能返回的跨域头，防止头冲突导致浏览器报错
		resp.Header.Del("Access-Control-Allow-Origin")
		resp.Header.Del("Access-Control-Allow-Methods")
		resp.Header.Del("Access-Control-Allow-Headers")

		// 重新设置我们网关的跨域规则
		resp.Header.Set("Access-Control-Allow-Origin", corsOrigin)

		// 如果是流式响应（SSE），显式禁止中间节点（如 Nginx、CDN）缓存
		if strings.Contains(resp.Header.Get("Content-Type"), "text/event-stream") {
			resp.Header.Set("Cache-Control", "no-cache, no-transform")
			resp.Header.Set("Connection", "keep-alive")
			resp.Header.Set("X-Accel-Buffering", "no") // 穿透 Nginx 缓存
		}

		return nil
	}

	// 5. 服务器配置
	server := &http.Server{
		Addr:              ":8080",
		ReadHeaderTimeout: 5 * time.Second,
		// 【修复 2】: 移除 ReadTimeout，防止弱网环境下由于大文件上传过慢导致连接被网关切断
		// 仅保留 ReadHeaderTimeout 用于防 Slowloris 攻击。整体生命周期由内部 Context 控制。
		IdleTimeout:       120 * time.Second,
		Handler: http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			// 致命防御：限制单次请求体最大为 50MB
			r.Body = http.MaxBytesReader(w, r.Body, 50*1024*1024)

			if r.Method == "OPTIONS" {
				w.Header().Set("Access-Control-Allow-Origin", corsOrigin)
				w.Header().Set("Access-Control-Allow-Methods", "GET, POST, PUT, DELETE, OPTIONS")
				w.Header().Set("Access-Control-Allow-Headers", "*")
				w.WriteHeader(http.StatusNoContent)
				return
			}

			if r.URL.Path == "/" && r.Method == "GET" {
				w.Header().Set("Content-Type", "text/html; charset=utf-8")
				w.WriteHeader(http.StatusOK)
				fmt.Fprint(w, indexHTML)
				return
			}

			if !strings.HasPrefix(r.URL.Path, "/v1beta/") && !strings.HasPrefix(r.URL.Path, "/v1/") {
				w.WriteHeader(http.StatusForbidden)
				fmt.Fprint(w, `{"error": "Forbidden"}`)
				return
			}

			// 挂载最大流式传输时间 Context
			ctx, cancel := context.WithTimeout(r.Context(), maxStreamDuration)
			defer cancel() // 无论函数正常结束还是异常退出，立即释放资源

			// 携带带有倒计时的 Context 进行转发
			reverseProxy.ServeHTTP(w, r.WithContext(ctx))
		}),
	}

	// 6. 优雅停机实现
	go func() {
		log.Printf("⚡ AIzaSy Kernel-Level Gateway is running on :8080 (Stream limit: %s)...", maxStreamDuration)
		if err := server.ListenAndServe(); err != nil && err != http.ErrServerClosed {
			log.Fatalf("❌ 服务异常: %v", err)
		}
	}()

	quit := make(chan os.Signal, 1)
	signal.Notify(quit, syscall.SIGINT, syscall.SIGTERM)
	<-quit

	log.Println("🔄 收到停机信号，等待活跃的 AI 流式请求完成收尾 (最多 60 秒)...")
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()

	if err := server.Shutdown(ctx); err != nil {
		log.Fatalf("❌ 强制关闭服务异常: %v", err)
	}
	log.Println("✅ AIzaSy Gateway 已安全退出")
}

func logRequest(req *http.Request) {
	requestURI := req.URL.RequestURI()
	maskedURI := apiKeyRegexp.ReplaceAllString(requestURI, "${1}***")
	log.Printf("[FORWARD] %s %s", req.Method, maskedURI)
}
