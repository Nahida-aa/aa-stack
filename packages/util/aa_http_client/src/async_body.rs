use std::{
    io::{Cursor, Read},
    pin::Pin,
    task::Poll,
};

use bytes::Bytes;
use futures::AsyncRead;

/// 请求/响应体的统一表示。
///
/// 抄自 zed `http_client::AsyncBody`，但**归本 crate 所有**：`http_client` 只是
/// 可选的桥接目标，业务不该被迫跟着 zed 的类型走。实现上只依赖 `bytes` 与
/// `futures`，足够小而稳定。
pub struct AsyncBody(pub Inner);

pub enum Inner {
    /// 没有 body（语义上不同于长度为 0 的 body）。
    Empty,

    /// 内存中的 body。
    Bytes(Cursor<Bytes>),

    /// 异步 reader（流式上传/下载）。
    AsyncReader(Pin<Box<dyn AsyncRead + Send + Sync>>),
}

impl AsyncBody {
    pub fn empty() -> Self {
        Self(Inner::Empty)
    }

    pub fn from_reader<R>(read: R) -> Self
    where
        R: AsyncRead + Send + Sync + 'static,
    {
        Self(Inner::AsyncReader(Box::pin(read)))
    }

    pub fn from_bytes(bytes: Bytes) -> Self {
        Self(Inner::Bytes(Cursor::new(bytes)))
    }

    /// 取出内部表示：`aa_reqwest_client` 用它转成 `reqwest::Body`，桥接层用它
    /// 转成 `http_client::AsyncBody`。
    pub fn into_inner(self) -> Inner {
        self.0
    }
}

impl Default for AsyncBody {
    fn default() -> Self {
        Self(Inner::Empty)
    }
}

impl From<()> for AsyncBody {
    fn from(_: ()) -> Self {
        Self(Inner::Empty)
    }
}

impl From<Bytes> for AsyncBody {
    fn from(bytes: Bytes) -> Self {
        Self::from_bytes(bytes)
    }
}

impl From<Vec<u8>> for AsyncBody {
    fn from(body: Vec<u8>) -> Self {
        Self::from_bytes(body.into())
    }
}

impl From<String> for AsyncBody {
    fn from(body: String) -> Self {
        Self::from_bytes(body.into())
    }
}

impl From<&'static [u8]> for AsyncBody {
    #[inline]
    fn from(s: &'static [u8]) -> Self {
        Self::from_bytes(Bytes::from_static(s))
    }
}

impl From<&'static str> for AsyncBody {
    #[inline]
    fn from(s: &'static str) -> Self {
        Self::from_bytes(Bytes::from_static(s.as_bytes()))
    }
}

impl<T: Into<Self>> From<Option<T>> for AsyncBody {
    fn from(body: Option<T>) -> Self {
        match body {
            Some(body) => body.into(),
            None => Self::empty(),
        }
    }
}

impl futures::AsyncRead for AsyncBody {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        // SAFETY: 标准 enum pin 投影。
        let inner = unsafe { &mut self.get_unchecked_mut().0 };
        match inner {
            Inner::Empty => Poll::Ready(Ok(0)),
            // 内存 buffer 上的读取不会阻塞。
            Inner::Bytes(cursor) => Poll::Ready(cursor.read(buf)),
            Inner::AsyncReader(async_reader) => {
                AsyncRead::poll_read(async_reader.as_mut(), cx, buf)
            }
        }
    }
}
