//! 共享 HTTP 传输的**接口层**：自有 [`HttpClient`] trait + [`AsyncBody`] + 假客户端。
//!
//! 为什么不直接复用 zed 的 `http_client`：那个 crate 要能随时用来 patch gpui，
//! 必须保持与上游一致；业务接口若跟着它走，就会被它的改动牵着。这里定义接口，
//! reqwest 实现在 `aa_reqwest_client`，`http_client` 只作为可选桥接
//! （feature `http-client-bridge`）。
//!
//! 使用形状：
//!
//! - 应用构造一次 `Arc<dyn HttpClient>`（`aa_reqwest_client::builder`），所有
//!   消费者共享同一个连接池；
//! - 单元测试换成 [`FakeHttpClient`]（`test-support` feature），不绑端口、不读环境。
//!
//! 本 crate **不认识任何 LLM 类型**，也不认识 reqwest。

mod async_body;

pub use async_body::{AsyncBody, Inner};
pub use http::{
    self, Method, Request, Response, StatusCode, Uri, header::HeaderMap, request::Builder,
};
pub use url::Url;

use futures::future::BoxFuture;
use std::time::Duration;

/// 重定向策略（塞进请求 extension，由后端决定怎么用）。
#[derive(Default, Debug, Clone, PartialEq, Eq, Hash)]
pub enum RedirectPolicy {
    #[default]
    NoFollow,
    FollowLimit(u32),
    FollowAll,
}

/// 整请求总时长（含响应体读取），塞进请求 extension 由后端消费。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestTimeout(pub Duration);

/// 构造 [`Request`]/[`Builder`] 时用到的便捷扩展。
pub trait HttpRequestExt {
    /// 条件修改自身。
    fn when(self, condition: bool, then: impl FnOnce(Self) -> Self) -> Self
    where
        Self: Sized,
    {
        if condition { then(self) } else { self }
    }

    /// `Some` 时解包并修改自身。
    fn when_some<T>(self, option: Option<T>, then: impl FnOnce(Self, T) -> Self) -> Self
    where
        Self: Sized,
    {
        match option {
            Some(value) => then(self, value),
            None => self,
        }
    }

    /// 是否跟随重定向。
    fn follow_redirects(self, follow: RedirectPolicy) -> Self;

    /// 整请求总时长（含响应体）。
    fn timeout(self, timeout: Duration) -> Self;
}

impl HttpRequestExt for Builder {
    fn follow_redirects(self, follow: RedirectPolicy) -> Self {
        self.extension(follow)
    }

    fn timeout(self, timeout: Duration) -> Self {
        debug_assert!(!timeout.is_zero(), "timeout must be positive");
        self.extension(RequestTimeout(timeout))
    }
}

/// 可跨线程共享的 HTTP 客户端。
pub trait HttpClient: 'static + Send + Sync {
    fn send(
        &self,
        req: Request<AsyncBody>,
    ) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>>;
}

/// 出站路由配置。
///
/// 「空串 = 读环境变量」「拒绝 socks」这类**用户配置语义**不在本 crate：这里只
/// 接收已经解好的选择。`Direct` 是机制（忽略环境变量代理），不是策略。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Proxy {
    /// 跟随环境变量（`ALL_PROXY` / `HTTPS_PROXY` / `NO_PROXY`）。
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

/// 读完整个响应体（非流式路径）。
///
/// 流式路径用 [`Response::into_body`] 自己逐块读，不要先缓冲。
pub async fn read_body(response: &mut Response<AsyncBody>) -> std::io::Result<Vec<u8>> {
    use futures::AsyncReadExt as _;

    let mut body = Vec::new();
    response.body_mut().read_to_end(&mut body).await?;
    Ok(body)
}

#[cfg(any(test, feature = "test-support"))]
mod fake {
    use super::*;
    use std::{
        future::Future,
        sync::{Arc, Mutex},
    };

    type Handler = Arc<
        dyn Fn(Request<AsyncBody>) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>>
            + Send
            + Sync
            + 'static,
    >;

    /// 完全离线的客户端：只回调用方给的响应。
    pub struct FakeHttpClient {
        handler: Mutex<Handler>,
    }

    impl FakeHttpClient {
        pub fn create<Fut, F>(handler: F) -> Arc<Self>
        where
            Fut: Future<Output = anyhow::Result<Response<AsyncBody>>> + Send + 'static,
            F: Fn(Request<AsyncBody>) -> Fut + Send + Sync + 'static,
        {
            Arc::new(Self {
                handler: Mutex::new(Arc::new(move |req| Box::pin(handler(req)))),
            })
        }

        /// 固定 200 + 空响应体。
        pub fn with_200_response() -> Arc<Self> {
            Self::create(|_| async {
                Ok(Response::builder()
                    .status(200)
                    .body(AsyncBody::empty())
                    .unwrap())
            })
        }
    }

    impl HttpClient for FakeHttpClient {
        fn send(
            &self,
            req: Request<AsyncBody>,
        ) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
            let handler = self.handler.lock().unwrap().clone();
            handler(req)
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
pub use fake::FakeHttpClient;

#[cfg(any(test, feature = "http-client-bridge"))]
mod bridge {
    use super::*;
    use futures::AsyncRead;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::{Context, Poll};

    /// 把 zed 的 `http_client::HttpClient` 适配成本 crate 的 [`HttpClient`]。
    ///
    /// 场景：调用方手里已经是 gpui/zed 的连接池，但下游接口（例如
    /// `aa_github_releases`）要的是本 crate 的 trait。
    pub struct ZedHttpClient(Arc<dyn http_client::HttpClient>);

    impl ZedHttpClient {
        pub fn new(client: Arc<dyn http_client::HttpClient>) -> Self {
            Self(client)
        }
    }

    impl HttpClient for ZedHttpClient {
        fn send(
            &self,
            req: Request<AsyncBody>,
        ) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
            let (parts, body) = req.into_parts();
            let body = match body.into_inner() {
                Inner::Empty => http_client::AsyncBody::empty(),
                Inner::Bytes(cursor) => http_client::AsyncBody::from_bytes(cursor.into_inner()),
                Inner::AsyncReader(reader) => {
                    http_client::AsyncBody::from_reader(ZedReader(reader))
                }
            };
            let request = Request::from_parts(parts, body);
            let client = self.0.clone();
            Box::pin(async move {
                let response = http_client::HttpClient::send(&*client, request).await?;
                let (parts, body) = response.into_parts();
                Ok(Response::from_parts(parts, AsyncBody::from_reader(body)))
            })
        }
    }

    /// `Pin<Box<dyn AsyncRead + Send + Sync>>` 包一层，满足 `from_reader` 对
    /// `Unpin` 的隐性假设。
    struct ZedReader(Pin<Box<dyn AsyncRead + Send + Sync>>);

    impl AsyncRead for ZedReader {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut [u8],
        ) -> Poll<std::io::Result<usize>> {
            self.get_mut().0.as_mut().poll_read(cx, buf)
        }
    }
}

#[cfg(any(test, feature = "http-client-bridge"))]
pub use bridge::ZedHttpClient;

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use std::sync::Arc;

    #[test]
    fn fake_client_satisfies_the_same_trait_object() {
        let client: Arc<dyn HttpClient> = FakeHttpClient::with_200_response();
        let request = Request::builder()
            .uri("http://test.example/v1/models")
            .body(AsyncBody::empty())
            .unwrap();
        let response = block_on(client.send(request)).expect("假客户端不该报错");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn timeout_and_redirect_land_in_request_extensions() {
        let request = Request::builder()
            .uri("http://test.example")
            .follow_redirects(RedirectPolicy::FollowAll)
            .timeout(Duration::from_secs(3))
            .body(AsyncBody::empty())
            .unwrap();

        assert_eq!(
            request.extensions().get::<RedirectPolicy>(),
            Some(&RedirectPolicy::FollowAll)
        );
        assert_eq!(
            request.extensions().get::<RequestTimeout>(),
            Some(&RequestTimeout(Duration::from_secs(3)))
        );
    }

    #[cfg(feature = "http-client-bridge")]
    #[test]
    fn zed_client_is_adapted() {
        let zed: Arc<dyn http_client::HttpClient> =
            http_client::FakeHttpClient::with_200_response();
        let client = ZedHttpClient::new(zed);
        let request = Request::builder()
            .uri("http://test.example/x")
            .body(AsyncBody::empty())
            .unwrap();
        let response = block_on(client.send(request)).expect("桥接客户端不该报错");
        assert_eq!(response.status(), StatusCode::OK);
    }
}
