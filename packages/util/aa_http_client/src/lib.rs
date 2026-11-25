//! 共享 HTTP 传输的**门面**：接口来自 `http_client`（zed 的同名 crate 移植），
//! 真实连接池在 [`aa_reqwest_client`]。
//!
//! 为什么是接口层：provider 数量会增长，但连接池不该跟着增长。Zed 用
//! `Arc<dyn HttpClient>` 跨 provider 共享（`crates/http_client` + `crates/reqwest_client`），
//! 这里沿用同一形状：
//!
//! - 应用构造一次 `Arc<dyn HttpClient>`，所有 provider 共享；
//! - 单元测试换成 [`FakeHttpClient`]（`test-support` feature），不再起本地端口。
//!
//! 本 crate **不认识任何 LLM 类型**：构造错误用 [`BuildError`]，调用方自己映射成
//! 自己的错误（例如 aa-ai 的 `LlmError`、aacode 的 `SettingsError`）。
//!
//! 超时/keep-alive 取值见 [`aa_reqwest_client`]（对齐 zed `ReqwestClient::builder`）。

use std::sync::Arc;
use std::time::Duration;

pub use aa_reqwest_client::{
    BuildError, CONNECT_TIMEOUT, DEFAULT_READ_TIMEOUT, HTTP2_KEEP_ALIVE_INTERVAL,
    HTTP2_KEEP_ALIVE_TIMEOUT, POOL_IDLE_TIMEOUT, Proxy, ReqwestClient, TCP_KEEPALIVE,
    describe_error, describe_transport,
};
pub use http_client::{
    self, AsyncBody, HttpClient, HttpRequestExt, Method, RedirectPolicy, Request, RequestTimeout,
    Response, StatusCode, Uri, Url, http,
};
#[cfg(feature = "test-support")]
pub use http_client::FakeHttpClient;

/// 默认策略：跟随环境变量代理 + [`DEFAULT_READ_TIMEOUT`]。
pub fn builder() -> Builder {
    Builder {
        inner: ReqwestClient::builder(),
    }
}

/// [`builder`] 的构造参数。
#[derive(Debug, Clone)]
pub struct Builder {
    inner: aa_reqwest_client::Builder,
}

impl Builder {
    pub fn proxy(mut self, proxy: Proxy) -> Self {
        self.inner = self.inner.proxy(proxy);
        self
    }

    /// 覆盖读取 idle 超时（默认 [`DEFAULT_READ_TIMEOUT`]）。
    pub fn read_timeout(mut self, timeout: Duration) -> Self {
        self.inner = self.inner.read_timeout(timeout);
        self
    }

    /// 关闭读取 idle 超时。只有调用方自己负责取消（或端点确实需要无限静默）时才用。
    pub fn without_read_timeout(mut self) -> Self {
        self.inner = self.inner.without_read_timeout();
        self
    }

    /// 构造一个可在 provider 之间共享的客户端。
    pub fn build(self) -> Result<Arc<dyn HttpClient>, BuildError> {
        Ok(Arc::new(self.inner.build()?))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_returns_a_shared_dyn_client() {
        let client: Arc<dyn HttpClient> = builder()
            .proxy(Proxy::Direct)
            .read_timeout(Duration::from_millis(150))
            .build()
            .expect("直连应该能构造");
        // trait 对象可以直接跨线程共享（provider 的 `Send + Sync` 要求）。
        let _cloned = client.clone();
        assert_eq!(client.proxy(), None);
    }

    #[test]
    fn invalid_proxy_url_is_a_build_error() {
        let error = match builder().proxy(Proxy::explicit("http://")).build() {
            Ok(_) => panic!("坏代理 URL 应该报错"),
            Err(error) => error,
        };
        assert!(
            matches!(error, BuildError::InvalidProxy { .. }),
            "应该是 InvalidProxy：{error}"
        );
        assert!(error.to_string().contains("代理"), "{error}");
    }

    #[cfg(any(test, feature = "test-support"))]
    #[test]
    fn fake_client_satisfies_the_same_trait_object() {
        use futures::executor::block_on;

        // 测试构建里 dev-dependency 会打开 `http_client/test-support`，所以这里
        // 绕过 feature-gated 的重导出，直接用上游类型。
        let client: Arc<dyn HttpClient> = http_client::FakeHttpClient::with_200_response();
        let request = Request::builder()
            .uri("http://test.example/v1/models")
            .body(AsyncBody::empty())
            .unwrap();
        let response = block_on(client.send(request)).expect("假客户端不该报错");
        assert_eq!(response.status(), StatusCode::OK);
    }
}
