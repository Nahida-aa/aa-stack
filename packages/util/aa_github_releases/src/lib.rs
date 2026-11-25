//! GitHub release 元数据查询：只依赖 `http_client`，不依赖 gpui。
//!
//! 对应上游 zed `crates/github_releases/src/releases.rs`；下载/解压见
//! `github_releases_gpui`。

mod releases;

pub use releases::{
    AssetKind, GitHubLspBinaryVersion, GithubRelease, GithubReleaseAsset, build_asset_url,
    get_release_by_tag_name, latest_github_release,
};
