//! GitHub release 的查询、下载与解压：只依赖 `aa_http_client`，不依赖 gpui。
//!
//! 对应上游 zed `crates/github_releases`；需要 gpui `BackgroundExecutor` 的便捷
//! 包装见 `github_releases_gpui`。

mod download;
mod releases;

pub use download::{GithubBinaryMetadata, download_server_binary, download_server_raw_binary};
pub use releases::{
    AssetKind, GitHubLspBinaryVersion, GithubRelease, GithubReleaseAsset, build_asset_url,
    get_release_by_tag_name, latest_github_release,
};
