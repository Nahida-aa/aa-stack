pub struct Deferred<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> Deferred<F> {
    /// 丢弃时不执行延迟函数。
    pub fn abort(mut self) {
        self.0.take();
    }
}

impl<F: FnOnce()> Drop for Deferred<F> {
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            f()
        }
    }
}

/// 返回的守卫被 drop 时执行 `f`，除非调用了 [`Deferred::abort`]。
///
/// 必须绑定到具名变量（如 `let _guard = defer(..)`）；写成 `let _ = ..`
/// 会立刻 drop，从而立刻执行闭包。
#[must_use]
pub fn defer<F: FnOnce()>(f: F) -> Deferred<F> {
    Deferred(Some(f))
}
