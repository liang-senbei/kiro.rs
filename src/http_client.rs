//! HTTP Client 构建模块
//!
//! 提供统一的 HTTP Client 构建功能，支持代理配置

use reqwest::{Client, ClientBuilder, Proxy};
use std::time::Duration;

use crate::model::config::TlsBackend;

/// 代理配置
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct ProxyConfig {
    /// 代理地址，支持 http/https/socks5
    pub url: String,
    /// 代理认证用户名
    pub username: Option<String>,
    /// 代理认证密码
    pub password: Option<String>,
}

impl ProxyConfig {
    /// 从 url 创建代理配置
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            username: None,
            password: None,
        }
    }

    /// 设置认证信息
    pub fn with_auth(mut self, username: impl Into<String>, password: impl Into<String>) -> Self {
        self.username = Some(username.into());
        self.password = Some(password.into());
        self
    }
}

/// 构建 HTTP Client
///
/// # Arguments
/// * `proxy` - 可选的代理配置
/// * `timeout_secs` - 超时时间（秒）
///
/// # Returns
/// 配置好的 reqwest::Client
pub fn build_client(
    proxy: Option<&ProxyConfig>,
    timeout_secs: u64,
    tls_backend: TlsBackend,
) -> anyhow::Result<Client> {
    let builder = Client::builder().timeout(Duration::from_secs(timeout_secs));
    finish_client(builder, proxy, tls_backend)
}

/// 上游模型调用 Client 的建连超时
const STREAMING_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// 上游模型调用 Client 的读空闲超时：等响应头、或响应体相邻两次读到数据之间最多等这么久。
///
/// 取原来的总超时值：以前能在总超时内完成的请求不受影响，一直在出数据的长输出不再被掐断。
const STREAMING_READ_IDLE_TIMEOUT: Duration = Duration::from_secs(720);

/// 构建上游模型调用（generateAssistantResponse / MCP）用的 HTTP Client
///
/// 不设总超时：总超时包含读响应体的时间，长输出（effort=max 时常见数分钟）会在
/// 生成途中被掐断。改为建连超时 + 读空闲超时，只有上游真的停住不动才算超时。
pub fn build_streaming_client(
    proxy: Option<&ProxyConfig>,
    tls_backend: TlsBackend,
) -> anyhow::Result<Client> {
    let builder = streaming_client_builder(STREAMING_CONNECT_TIMEOUT, STREAMING_READ_IDLE_TIMEOUT);
    finish_client(builder, proxy, tls_backend)
}

/// 上游模型调用 Client 的超时配置（测试用它注入短超时）
fn streaming_client_builder(
    connect_timeout: Duration,
    read_idle_timeout: Duration,
) -> ClientBuilder {
    Client::builder()
        .connect_timeout(connect_timeout)
        .read_timeout(read_idle_timeout)
}

/// 统一设置 TLS 后端与代理并构建 Client
fn finish_client(
    mut builder: ClientBuilder,
    proxy: Option<&ProxyConfig>,
    tls_backend: TlsBackend,
) -> anyhow::Result<Client> {
    match tls_backend {
        TlsBackend::Rustls => {
            builder = builder.use_rustls_tls();
        }
        TlsBackend::NativeTls => {
            #[cfg(feature = "native-tls")]
            {
                builder = builder.use_native_tls();
            }
            #[cfg(not(feature = "native-tls"))]
            {
                anyhow::bail!("此构建版本未包含 native-tls 后端，请在配置中改用 rustls");
            }
        }
    }

    if let Some(proxy_config) = proxy {
        let mut proxy = Proxy::all(&proxy_config.url)?;

        // 设置代理认证
        if let (Some(username), Some(password)) = (&proxy_config.username, &proxy_config.password) {
            proxy = proxy.basic_auth(username, password);
        }

        builder = builder.proxy(proxy);
        tracing::debug!(
            "HTTP Client 使用代理: {}",
            redact_proxy_url(&proxy_config.url)
        );
    }

    Ok(builder.build()?)
}

/// 日志用：隐藏代理 URL 中内嵌的 `user:pass@` 凭据
///
/// `http://user:pass@host:port` → `http://***@host:port`；无凭据时原样返回。
pub fn redact_proxy_url(url: &str) -> String {
    let (scheme, rest) = match url.split_once("://") {
        Some((scheme, rest)) => (Some(scheme), rest),
        None => (None, url),
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    match rest[..authority_end].rfind('@') {
        Some(at) => {
            let host = &rest[at + 1..];
            match scheme {
                Some(scheme) => format!("{scheme}://***@{host}"),
                None => format!("***@{host}"),
            }
        }
        None => url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_proxy_config_new() {
        let config = ProxyConfig::new("http://127.0.0.1:7890");
        assert_eq!(config.url, "http://127.0.0.1:7890");
        assert!(config.username.is_none());
        assert!(config.password.is_none());
    }

    #[test]
    fn test_proxy_config_with_auth() {
        let config = ProxyConfig::new("socks5://127.0.0.1:1080").with_auth("user", "pass");
        assert_eq!(config.url, "socks5://127.0.0.1:1080");
        assert_eq!(config.username, Some("user".to_string()));
        assert_eq!(config.password, Some("pass".to_string()));
    }

    #[test]
    fn test_build_client_without_proxy() {
        let client = build_client(None, 30, TlsBackend::Rustls);
        assert!(client.is_ok());
    }

    #[test]
    fn test_build_client_with_proxy() {
        let config = ProxyConfig::new("http://127.0.0.1:7890");
        let client = build_client(Some(&config), 30, TlsBackend::Rustls);
        assert!(client.is_ok());
    }

    #[test]
    fn test_build_streaming_client() {
        assert!(build_streaming_client(None, TlsBackend::Rustls).is_ok());
        let config = ProxyConfig::new("socks5://127.0.0.1:1080").with_auth("user", "pass");
        assert!(build_streaming_client(Some(&config), TlsBackend::Rustls).is_ok());
    }

    /// 起一个只服务一次的本地上游：等 `header_delay` 后发响应头，再每隔 `chunk_interval`
    /// 发一个 1 字节的 chunk，共 `chunks` 个；`stall_after` 为 true 时发完不收尾、挂住连接。
    async fn spawn_upstream(
        header_delay: Duration,
        chunk_interval: Duration,
        chunks: usize,
        stall_after: bool,
    ) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let _ = socket.set_nodelay(true);
            // 读完请求头
            let mut request = Vec::new();
            let mut buf = [0u8; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                match socket.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => request.extend_from_slice(&buf[..n]),
                }
            }
            tokio::time::sleep(header_delay).await;
            let head = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
            if socket.write_all(head.as_bytes()).await.is_err() {
                return;
            }
            for _ in 0..chunks {
                tokio::time::sleep(chunk_interval).await;
                if socket.write_all(b"1\r\nx\r\n").await.is_err() {
                    return;
                }
            }
            if stall_after {
                tokio::time::sleep(Duration::from_secs(30)).await;
            } else {
                let _ = socket.write_all(b"0\r\n\r\n").await;
            }
        });
        format!("http://{addr}/")
    }

    /// 与 build_streaming_client 同一套超时配置，读空闲超时换成 `read_idle`；
    /// 不走系统代理，免得代理环境变量把 127.0.0.1 的请求带走
    fn short_idle_client(read_idle: Duration) -> Client {
        streaming_client_builder(Duration::from_secs(5), read_idle)
            .no_proxy()
            .build()
            .unwrap()
    }

    /// 测试用的读空闲超时
    const READ_IDLE: Duration = Duration::from_millis(600);

    #[tokio::test]
    async fn test_streaming_client_keeps_active_stream_past_read_idle() {
        // 一直在出数据的流：总耗时远超读空闲超时也不能被掐断（旧的总超时会在这里掐断）
        let url = spawn_upstream(Duration::ZERO, Duration::from_millis(60), 25, false).await;
        let client = short_idle_client(READ_IDLE);
        let start = std::time::Instant::now();
        let resp = client.get(url).send().await.unwrap();
        let body = resp.bytes().await.unwrap();
        let elapsed = start.elapsed();
        assert_eq!(body.len(), 25);
        assert!(elapsed > READ_IDLE * 2, "elapsed={elapsed:?}");
    }

    #[tokio::test]
    async fn test_streaming_client_times_out_stalled_body() {
        // 发了一个 chunk 后上游停住不动：读空闲超时到点报超时，而不是一直挂着
        let url = spawn_upstream(Duration::ZERO, Duration::ZERO, 1, true).await;
        let client = short_idle_client(READ_IDLE);
        let resp = client.get(url).send().await.unwrap();
        let err = resp.bytes().await.unwrap_err();
        assert!(err.is_timeout(), "{err:?}");
    }

    #[tokio::test]
    async fn test_streaming_client_times_out_waiting_for_headers() {
        // 读空闲超时同样管等响应头：上游迟迟不回响应头，send() 本身就报超时
        let url = spawn_upstream(Duration::from_secs(30), Duration::ZERO, 0, false).await;
        let client = short_idle_client(READ_IDLE);
        let err = client.get(url).send().await.unwrap_err();
        assert!(err.is_timeout(), "{err:?}");
    }

    #[test]
    fn test_redact_proxy_url() {
        assert_eq!(
            redact_proxy_url("http://user:p%40ss@127.0.0.1:7890"),
            "http://***@127.0.0.1:7890"
        );
        assert_eq!(
            redact_proxy_url("socks5h://user@proxy.example.com:1080/path?q=@x"),
            "socks5h://***@proxy.example.com:1080/path?q=@x"
        );
        assert_eq!(
            redact_proxy_url("user:pass@127.0.0.1:7890"),
            "***@127.0.0.1:7890"
        );
        // 无凭据：原样返回（路径 / query 中的 @ 不视为凭据）
        assert_eq!(
            redact_proxy_url("http://127.0.0.1:7890"),
            "http://127.0.0.1:7890"
        );
        assert_eq!(
            redact_proxy_url("http://127.0.0.1:7890/a@b"),
            "http://127.0.0.1:7890/a@b"
        );
    }
}
