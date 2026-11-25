//! `http_client::HttpClient` 的 reqwest 实现。
//!
//! 对应 zed 的 `crates/reqwest_client`：`http_client` 只定义接口（trait +
//! [`AsyncBody`]），真正的连接池、代理、超时都在这里。这样上层（provider /
//! 应用）可以只持有 `Arc<dyn http_client::HttpClient>`，测试时换成
//! `http_client::FakeHttpClient`（见 `aa_http_client` 的 `test-support` feature）。
//!
//! 超时/keep-alive 取值对齐 zed 的 `ReqwestClient::builder`（commit `4ed72e0135`）：
//! 10s 建连、30s TCP keep-alive / 连接池空闲回收、HTTP/2 keep-alive 15s/10s。
//! 区别只在 read timeout：zed 的 LLM 入口传 `None`，这里默认开启
//! （见 [`DEFAULT_READ_TIMEOUT`]）。

use std::{
    mem,
    pin::Pin,
    sync::OnceLock,
    task::Poll,
    time::Duration,
};

use anyhow::anyhow;
use bytes::{BufMut, Bytes, BytesMut};
use futures::{AsyncRead, FutureExt as _, TryStreamExt as _};
use http_client::{AsyncBody, Inner, RequestTimeout, Response, Url, http};
use reqwest::header::HeaderValue;

/// 建连超时（DNS + TCP + TLS + 代理握手）。
///
/// 没有它，坏网络或挂掉的代理会让 `.send()` 长时间悬着，用户只看到「没反应」。
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// TCP keep-alive：让 NAT/中间设备看到连接仍然活跃。
pub const TCP_KEEPALIVE: Duration = Duration::from_secs(30);

/// 空闲连接在池里放多久后丢弃。
///
/// 尽早丢掉「静默坏掉」的复用连接（NAT 超时、链路重置）：stale HTTP/2 连接
/// 是长连接端点 `BadRecordMac` 一类 TLS 错误的常见来源。
pub const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// HTTP/2 PING 间隔与等待 PONG 的上限。
pub const HTTP2_KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(15);
pub const HTTP2_KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(10);

/// 默认的读取 idle 超时：连续这么久没收到**任何字节**就放弃。
///
/// 这是「两次读取之间」的上限，**不是整请求总时长**：只要还在收数据（包括
/// SSE 注释形式的 keep-alive），计时就会重置。取 120s 是因为推理阶段可能静默
/// 数十秒，而常见 provider 的 keep-alive 间隔是 15–30s；它只用来抓「连接已经
/// 死了，但 TCP 还没报错」的情况。
///
/// 不需要的调用方用 [`Builder::without_read_timeout`] 关掉。
pub const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(120);

/// 出站路由。
///
/// 「空串 = 读环境变量」「拒绝 socks」这类**用户配置语义**不在这里：本 crate 只
/// 接收已经解好的选择。`Direct` 是机制（忽略环境变量代理），不是策略。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Proxy {
    /// 跟随环境变量（`ALL_PROXY` / `HTTPS_PROXY` / `NO_PROXY`），reqwest 默认。
    Environment,
    /// 强制直连，忽略环境变量代理。
    Direct,
    /// 显式代理 URL（`http://` / `https://`，可带 `user:pass@`）。
    Explicit(String),
}

impl Proxy {
    /// 显式代理。调用方负责先校验/规范化 URL。
    pub fn explicit(url: impl Into<String>) -> Self {
        Self::Explicit(url.into())
    }
}

/// [`ReqwestClient`] 的构造参数。
#[derive(Debug, Clone)]
pub struct Builder {
    proxy: Proxy,
    read_timeout: Option<Duration>,
}

impl Default for Builder {
    fn default() -> Self {
        Self {
            proxy: Proxy::Environment,
            read_timeout: Some(DEFAULT_READ_TIMEOUT),
        }
    }
}

impl Builder {
    /// 默认策略：跟随环境变量代理 + [`DEFAULT_READ_TIMEOUT`]。
    pub fn new() -> Self {
        Self::default()
    }

    pub fn proxy(mut self, proxy: Proxy) -> Self {
        self.proxy = proxy;
        self
    }

    /// 覆盖读取 idle 超时（默认 [`DEFAULT_READ_TIMEOUT`]）。
    pub fn read_timeout(mut self, timeout: Duration) -> Self {
        self.read_timeout = Some(timeout);
        self
    }

    /// 关闭读取 idle 超时。只有调用方自己负责取消（或端点确实需要无限静默）时才用。
    pub fn without_read_timeout(mut self) -> Self {
        self.read_timeout = None;
        self
    }

    pub fn build(self) -> Result<ReqwestClient, BuildError> {
        let mut builder = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .tcp_keepalive(TCP_KEEPALIVE)
            .pool_idle_timeout(POOL_IDLE_TIMEOUT)
            .http2_keep_alive_interval(HTTP2_KEEP_ALIVE_INTERVAL)
            .http2_keep_alive_timeout(HTTP2_KEEP_ALIVE_TIMEOUT)
            .http2_keep_alive_while_idle(true);

        if let Some(read_timeout) = self.read_timeout {
            builder = builder.read_timeout(read_timeout);
        }

        let mut resolved_proxy = None;
        match &self.proxy {
            Proxy::Environment => {}
            Proxy::Direct => {
                builder = builder.no_proxy();
            }
            Proxy::Explicit(url) => {
                let proxy = reqwest::Proxy::all(url.as_str()).map_err(|source| {
                    BuildError::InvalidProxy {
                        url: url.clone(),
                        source,
                    }
                })?;
                // 显式代理仍然尊重 NO_PROXY（NO_PROXY 是「哪些主机直连」，与走哪个代理无关）。
                builder = builder.proxy(proxy.no_proxy(reqwest::NoProxy::from_env()));
                resolved_proxy = url.parse().ok();
            }
        }

        let client = builder
            .build()
            .map_err(|error| BuildError::Client(describe_error(&error)))?;

        Ok(ReqwestClient {
            client,
            proxy: resolved_proxy,
            read_timeout: self.read_timeout,
            handle: current_runtime_handle(),
        })
    }
}

/// 构造 [`ReqwestClient`] 失败。
///
/// 这是**传输构造**错误，不是「代理设置校验」：哪个配置键叫 `proxy`、空串是什么
/// 语义、要不要拒绝 socks，都是应用层的事。
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    /// `reqwest::Proxy::all` 解析 URL 失败。
    #[error("代理 `{url}` 无效：{source}")]
    InvalidProxy {
        url: String,
        #[source]
        source: reqwest::Error,
    },

    /// 客户端构建失败（TLS 后端、DNS 配置等）。
    #[error("构建 HTTP 客户端失败：{0}")]
    Client(String),
}

/// reqwest 客户端句柄（内部是 `reqwest::Client` 的 `Arc`，克隆的是指针）。
pub struct ReqwestClient {
    client: reqwest::Client,
    proxy: Option<Url>,
    read_timeout: Option<Duration>,
    /// `send` 必须跑在 tokio runtime 上（reqwest/hyper 的 reactor）；没有就自建一个。
    handle: tokio::runtime::Handle,
}

impl ReqwestClient {
    /// 默认策略：跟随环境变量代理 + [`DEFAULT_READ_TIMEOUT`]。
    pub fn builder() -> Builder {
        Builder::default()
    }

    /// 构造时选定的读取 idle 超时；`None` = 不设。
    pub fn read_timeout(&self) -> Option<Duration> {
        self.read_timeout
    }
}

impl http_client::HttpClient for ReqwestClient {
    fn user_agent(&self) -> Option<&HeaderValue> {
        None
    }

    fn proxy(&self) -> Option<&Url> {
        self.proxy.as_ref()
    }

    fn send(
        &self,
        req: http::Request<AsyncBody>,
    ) -> futures::future::BoxFuture<
        'static,
        anyhow::Result<Response<AsyncBody>>,
    > {
        let (parts, body) = req.into_parts();

        let mut request_builder = self.client.request(parts.method, parts.uri.to_string());
        request_builder = request_builder.headers(parts.headers);
        // 注：`http_client` 的 `RedirectPolicy` 扩展在 reqwest 0.12 没有「每请求」
        // 入口（只有 `ClientBuilder::redirect`）。这里沿用 reqwest 默认（最多跟随
        // 10 次）；需要 `NoFollow` 的调用方应使用自己的客户端配置。
        if let Some(timeout) = parts.extensions.get::<RequestTimeout>() {
            request_builder = request_builder.timeout(timeout.0);
        }
        let request = request_builder.body(match body.0 {
            Inner::Empty => reqwest::Body::default(),
            Inner::Bytes(cursor) => cursor.into_inner().into(),
            Inner::AsyncReader(stream) => reqwest::Body::wrap_stream(StreamReader::new(stream)),
        });

        let handle = self.handle.clone();
        let body_handle = self.handle.clone();
        let read_timeout = self.read_timeout;
        async move {
            let join_handle = handle.spawn(async { request.send().await });
            let abort_handle = join_handle.abort_handle();
            let _abort_on_drop = AbortOnDrop(abort_handle);

            let mut response = join_handle
                .await
                .map_err(|error| anyhow!("HTTP 请求被取消：{error}"))?
                // 错误在源头就翻成可读文案：调用方只持有 `Arc<dyn HttpClient>`，
                // 拿不到 reqwest 类型，也没法再判断「是不是超时」。
                .map_err(|error| anyhow!("{}", describe_transport(&error, read_timeout)))?;

            let headers = mem::take(response.headers_mut());
            let mut builder = http::Response::builder()
                .status(response.status().as_u16())
                .version(response.version());
            *builder.headers_mut().unwrap() = headers;

            let bytes = response
                .bytes_stream()
                // body 读取阶段的 idle 超时同样在源头带好文案。
                .map_err(move |error| {
                    std::io::Error::other(describe_transport(&error, read_timeout))
                })
                .into_async_read();
            // `read_timeout` 让 body 的每次读取都用到 tokio 的 timer；而 body 是在
            // 调用方的 executor（gpui 的 smol、futures::executor 等）上被 poll 的。
            // 因此读的时候必须把 tokio 上下文重新挂上，否则报 “no reactor running”。
            let body = AsyncBody::from_reader(RuntimeAsyncRead {
                inner: Box::pin(bytes),
                handle: body_handle.clone(),
            });

            builder.body(body).map_err(|error| anyhow!(error))
        }
        .boxed()
    }
}

/// future 被丢弃时取消已经 spawn 到 tokio 的请求（否则超时后它还在后台跑）。
struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

/// 没有 tokio 上下文时用的兜底运行时。
pub fn runtime() -> &'static tokio::runtime::Runtime {
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            // 请求本身仍是异步的；这里只是保证 reqwest 有 reactor 可用。
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("初始化 HTTP 运行时失败")
    })
}

fn current_runtime_handle() -> tokio::runtime::Handle {
    tokio::runtime::Handle::try_current().unwrap_or_else(|_| runtime().handle().clone())
}

/// 在每次 `poll_read` 期间恢复 tokio 上下文的 `AsyncRead` 适配器。
///
/// 响应体的读取发生在调用方 executor 上（不一定是 tokio）；而带 `read_timeout`
/// 的 body 在内部要用 tokio timer。没有这层，gpui 的 smol executor 上读 body 会
/// 直接 panic（`there is no reactor running`）。
struct RuntimeAsyncRead {
    inner: Pin<Box<dyn futures::AsyncRead + Send + Sync>>,
    handle: tokio::runtime::Handle,
}

impl futures::AsyncRead for RuntimeAsyncRead {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let _guard = this.handle.enter();
        this.inner.as_mut().poll_read(cx, buf)
    }
}

// ---------- AsyncRead → Stream（请求体的流式发送） ----------
/// 把 `AsyncRead` 适配成 `Stream<Item = io::Result<Bytes>>`，交给
/// `reqwest::Body::wrap_stream`。
///
/// 这是 tokio-util `ReaderStream` 的等价实现（避免额外依赖）。
struct StreamReader {
    reader: Option<Pin<Box<dyn futures::AsyncRead + Send + Sync>>>,
    buf: BytesMut,
    capacity: usize,
}

impl StreamReader {
    const DEFAULT_CAPACITY: usize = 4096;

    fn new(reader: Pin<Box<dyn futures::AsyncRead + Send + Sync>>) -> Self {
        Self {
            reader: Some(reader),
            buf: BytesMut::new(),
            capacity: Self::DEFAULT_CAPACITY,
        }
    }
}

impl futures::Stream for StreamReader {
    type Item = std::io::Result<Bytes>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let mut this = self.as_mut();

        let mut reader = match this.reader.take() {
            Some(reader) => reader,
            None => return Poll::Ready(None),
        };

        if this.buf.capacity() == 0 {
            let capacity = this.capacity;
            this.buf.reserve(capacity);
        }

        match poll_read_buf(&mut reader, cx, &mut this.buf) {
            Poll::Pending => {
                // 关键：把 reader 放回去，否则下一轮会被当成流结束。
                self.reader = Some(reader);
                Poll::Pending
            }
            Poll::Ready(Err(error)) => {
                self.reader = None;
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(Ok(0)) => {
                self.reader = None;
                Poll::Ready(None)
            }
            Poll::Ready(Ok(_)) => {
                let chunk = this.buf.split();
                self.reader = Some(reader);
                Poll::Ready(Some(Ok(chunk.freeze())))
            }
        }
    }
}

/// 从 `AsyncRead` 读进 `BytesMut`，返回本次读到的字节数。
fn poll_read_buf(
    io: &mut Pin<Box<dyn futures::AsyncRead + Send + Sync>>,
    cx: &mut std::task::Context<'_>,
    buf: &mut BytesMut,
) -> Poll<std::io::Result<usize>> {
    if !buf.has_remaining_mut() {
        return Poll::Ready(Ok(0));
    }

    let n = {
        let dst = buf.chunk_mut();
        // SAFETY: `chunk_mut()` 返回 `UninitSlice`，它是 `[MaybeUninit<u8>]` 的
        // transparent wrapper；这里只需要一块可写的未初始化内存。
        let dst = unsafe { &mut *(dst as *mut _ as *mut [std::mem::MaybeUninit<u8>]) };
        let mut read_buf = tokio::io::ReadBuf::uninit(dst);
        let unfilled_portion = read_buf.initialize_unfilled();
        // SAFETY: Pin 投影（`io` 本身已经是被 pin 的 box）。
        let io_pin = unsafe { Pin::new_unchecked(io) };
        // `futures::AsyncRead` 把读取长度作为返回值，不推进 `ReadBuf`。
        std::task::ready!(io_pin.poll_read(cx, unfilled_portion)?)
    };

    // SAFETY: `initialize_unfilled()` 已经把可写区初始化；`advance_mut` 会在
    // `n` 超过容量时 panic，而不是越界写。
    unsafe {
        buf.advance_mut(n);
    }

    Poll::Ready(Ok(n))
}

// ---------- 错误文案 ----------

/// 把错误展开成「原因链」。
///
/// reqwest 的 `Display` 只有最外层（`error sending request for url (...)`），真正的
/// 原因——连接被拒、代理握手失败、证书错误——藏在 `source()` 里。配置了代理之后，
/// 不带原因链的报错基本没法定位（分不清是端点挂了还是代理挂了）。
pub fn describe_error(error: &(dyn std::error::Error + 'static)) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}

/// reqwest 错误 → 面向用户的文案。
///
/// 超时单独标出来：`read_timeout` 触发时 reqwest 的 `Display` 往往只是
/// `error decoding response body`，不点明「多久没收到数据」就分不清是端点慢、
/// 代理挂了还是连接已经被中间设备断开。
pub fn describe_transport(error: &reqwest::Error, read_timeout: Option<Duration>) -> String {
    let detail = describe_error(error);
    // `is_connect()` 也可能同时 `is_timeout()`（建连超时），先判连接阶段。
    if error.is_connect() {
        let kind = if error.is_timeout() {
            "连接超时"
        } else {
            "连接失败"
        };
        return format!("{kind}（DNS/TCP/TLS/代理握手）: {detail}");
    }
    if error.is_timeout() {
        return match read_timeout {
            Some(timeout) => format!(
                "读取超时：连续 {} 没有收到任何数据（连接可能已被代理或中间设备断开）: {detail}",
                format_duration(timeout)
            ),
            None => format!("请求超时: {detail}"),
        };
    }
    detail
}

/// 超时文案里的时长：`120 秒` / `200 毫秒`（避免亚秒级显示成「0 秒」）。
fn format_duration(duration: Duration) -> String {
    if duration.as_secs() > 0 {
        format!("{} 秒", duration.as_secs())
    } else {
        format!("{} 毫秒", duration.as_millis())
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    use futures::{AsyncReadExt as _, executor::block_on};
    use http_client::{HttpClient as _, Method};

    use super::*;

    /// 起一个只回固定字节的本地端点。
    fn serve_once(response: &'static [u8]) -> (std::net::SocketAddr, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 4096];
            // 读到请求头结束就回应；测试请求体很小。
            loop {
                let read = socket.read(&mut buf).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            socket.write_all(response).unwrap();
            socket.flush().unwrap();
        });
        (addr, handle)
    }

    fn get(client: &ReqwestClient, url: String) -> Response<AsyncBody> {
        let request = http::Request::builder()
            .method(Method::GET)
            .uri(url)
            .body(AsyncBody::empty())
            .unwrap();
        block_on(client.send(request)).expect("请求应该成功")
    }

    #[test]
    fn response_status_and_body_are_passed_through() {
        let (addr, server) = serve_once(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 14\r\n\r\n{\"hello\":\"hi\"}",
        );

        let client = ReqwestClient::builder()
            .proxy(Proxy::Direct)
            .build()
            .unwrap();
        let response = get(&client, format!("http://{addr}/v1/models"));

        assert_eq!(response.status().as_u16(), 200);
        let mut body = String::new();
        block_on(response.into_body().read_to_string(&mut body)).unwrap();
        server.join().unwrap();
        assert_eq!(body, "{\"hello\":\"hi\"}");
    }

    #[test]
    fn non_success_status_is_not_an_error() {
        let (addr, server) = serve_once(
            b"HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
        );

        let client = ReqwestClient::builder()
            .proxy(Proxy::Direct)
            .build()
            .unwrap();
        let response = get(&client, format!("http://{addr}/missing"));

        // 404 是「HTTP 层成功」，由调用方决定怎么处理。
        assert_eq!(response.status().as_u16(), 404);
        server.join().unwrap();
    }
}
