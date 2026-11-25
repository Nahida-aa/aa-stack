//! 与平台相关的文件辅助函数（不依赖 gpui）。

/// Set the permissions for the given path so that the file becomes executable.
#[cfg(unix)]
pub async fn make_file_executable(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    async_fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).await
}

#[cfg(not(unix))]
#[allow(clippy::unused_async)]
/// Set the permissions for the given path so that the file becomes executable.
/// This is a noop for non-unix platforms.
pub async fn make_file_executable(_path: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}
