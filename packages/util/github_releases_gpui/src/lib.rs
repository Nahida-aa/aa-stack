//! GitHub release 下载/解压的 gpui 便捷包装（依赖 `BackgroundExecutor`）。
//!
//! 真正的异步实现在 `aa_github_releases`（不依赖 gpui）；这里只做两件事：把 zed
//! 的 `http_client::HttpClient` 适配成 `aa_http_client::HttpClient`，然后把任务
//! 丢进 gpui 的后台执行器。

pub use aa_github_releases::{
    AssetKind, GitHubLspBinaryVersion, GithubBinaryMetadata, GithubRelease, GithubReleaseAsset,
    build_asset_url, get_release_by_tag_name, latest_github_release,
};

use std::{path::Path, sync::Arc};

use aa_http_client::ZedHttpClient;
use anyhow::Result;
use gpui::{BackgroundExecutor, Task};
use http_client::HttpClient;

pub fn download_server_binary(
    executor: &BackgroundExecutor,
    http_client: Arc<dyn HttpClient>,
    url: &str,
    digest: Option<&str>,
    destination_path: &Path,
    asset_kind: AssetKind,
) -> Task<Result<()>> {
    let url = url.to_owned();
    let digest = digest.map(ToOwned::to_owned);
    let destination_path = destination_path.to_owned();
    let http = shared_http_client(http_client);
    executor.spawn(async move {
        aa_github_releases::download_server_binary(
            http,
            &url,
            digest.as_deref(),
            &destination_path,
            asset_kind,
        )
        .await
    })
}

pub fn download_server_raw_binary(
    executor: &BackgroundExecutor,
    http_client: Arc<dyn HttpClient>,
    url: &str,
    digest: Option<&str>,
    destination_path: &Path,
    binary_file_name: &str,
) -> Task<Result<()>> {
    let url = url.to_owned();
    let digest = digest.map(ToOwned::to_owned);
    let destination_path = destination_path.to_owned();
    let binary_file_name = binary_file_name.to_owned();
    let http = shared_http_client(http_client);
    executor.spawn(async move {
        aa_github_releases::download_server_raw_binary(
            http,
            &url,
            digest.as_deref(),
            &destination_path,
            &binary_file_name,
        )
        .await
    })
}

fn shared_http_client(client: Arc<dyn HttpClient>) -> Arc<dyn aa_http_client::HttpClient> {
    Arc::new(ZedHttpClient::new(client))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future::BoxFuture;
    use http_client::{AsyncBody, Response, Url, http::HeaderValue};
    use sha2::{Digest, Sha256};

    struct StaticResponseClient {
        status: u16,
        body: Vec<u8>,
    }

    impl HttpClient for StaticResponseClient {
        fn send(
            &self,
            _req: http_client::Request<AsyncBody>,
        ) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
            let status = self.status;
            let body = self.body.clone();
            Box::pin(async move {
                Ok(Response::builder()
                    .status(status)
                    .body(AsyncBody::from(body))
                    .unwrap())
            })
        }

        fn user_agent(&self) -> Option<&HeaderValue> {
            None
        }

        fn proxy(&self) -> Option<&Url> {
            None
        }
    }

    #[gpui::test]
    async fn downloads_raw_binary_with_uppercase_digest_into_destination_dir(
        executor: BackgroundExecutor,
    ) {
        executor.allow_parking();
        let temp_dir = tempfile::tempdir().unwrap();
        let destination_path = temp_dir.path().join("v_1");
        let contents = b"#!/bin/sh\necho hello\n".to_vec();
        let expected_sha_256 = format!("{:X}", Sha256::digest(&contents));
        let client = StaticResponseClient {
            status: 200,
            body: contents,
        };

        download_server_raw_binary(
            &executor,
            Arc::new(client),
            "https://example.com/agent-binary",
            Some(&expected_sha_256),
            &destination_path,
            "agent-binary",
        )
        .await
        .unwrap();

        let binary_path = destination_path.join("agent-binary");
        assert_eq!(
            std::fs::read(&binary_path).unwrap(),
            b"#!/bin/sh\necho hello\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&binary_path)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "binary should be executable");
        }
    }

    #[gpui::test]
    async fn raw_binary_without_digest_rejects_failed_response(executor: BackgroundExecutor) {
        executor.allow_parking();
        let temp_dir = tempfile::tempdir().unwrap();
        let destination_path = temp_dir.path().join("v_1");
        let client = StaticResponseClient {
            status: 403,
            body: b"rate limit exceeded".to_vec(),
        };

        let error = download_server_raw_binary(
            &executor,
            Arc::new(client),
            "https://example.com/agent-binary",
            None,
            &destination_path,
            "agent-binary",
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("403"));
        assert!(!destination_path.exists());
        let leftover_entries = std::fs::read_dir(temp_dir.path()).unwrap().count();
        assert_eq!(
            leftover_entries, 0,
            "no staging directory should be created"
        );
    }

    #[gpui::test]
    async fn raw_binary_digest_mismatch_cleans_up_staging(executor: BackgroundExecutor) {
        executor.allow_parking();
        let temp_dir = tempfile::tempdir().unwrap();
        let destination_path = temp_dir.path().join("v_1");
        let client = StaticResponseClient {
            status: 200,
            body: b"some binary".to_vec(),
        };

        let error = download_server_raw_binary(
            &executor,
            Arc::new(client),
            "https://example.com/agent-binary",
            Some("0000000000000000000000000000000000000000000000000000000000000000"),
            &destination_path,
            "agent-binary",
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("SHA-256 mismatch"));
        assert!(!destination_path.exists());
        let leftover_entries = std::fs::read_dir(temp_dir.path()).unwrap().count();
        assert_eq!(leftover_entries, 0, "staging directory should be removed");
    }

    #[gpui::test]
    async fn downloads_archive_with_uppercase_digest_and_extracts_contents(
        executor: BackgroundExecutor,
    ) {
        executor.allow_parking();
        let archive = vec![
            0x50, 0x4b, 0x03, 0x04, 0x14, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x21, 0x00,
            0x86, 0xa6, 0x10, 0x36, 0x05, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x05, 0x00,
            0x00, 0x00, 0x61, 0x67, 0x65, 0x6e, 0x74, 0x68, 0x65, 0x6c, 0x6c, 0x6f, 0x50, 0x4b,
            0x01, 0x02, 0x14, 0x03, 0x14, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x21, 0x00,
            0x86, 0xa6, 0x10, 0x36, 0x05, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x05, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0x01, 0x00, 0x00,
            0x00, 0x00, 0x61, 0x67, 0x65, 0x6e, 0x74, 0x50, 0x4b, 0x05, 0x06, 0x00, 0x00, 0x00,
            0x00, 0x01, 0x00, 0x01, 0x00, 0x33, 0x00, 0x00, 0x00, 0x28, 0x00, 0x00, 0x00, 0x00,
            0x00,
        ];
        let expected_sha_256 = format!("{:X}", Sha256::digest(&archive));
        let client = StaticResponseClient {
            status: 200,
            body: archive,
        };
        let temp_dir = tempfile::tempdir().unwrap();
        let destination_path = temp_dir.path().join("v_1");

        download_server_binary(
            &executor,
            Arc::new(client),
            "https://example.com/agent.zip",
            Some(&expected_sha_256),
            &destination_path,
            AssetKind::Zip,
        )
        .await
        .unwrap();

        assert_eq!(
            std::fs::read(destination_path.join("agent")).unwrap(),
            b"hello"
        );
    }

    #[cfg(unix)]
    #[gpui::test]
    async fn replaces_symlinked_destination_without_touching_its_target(
        executor: BackgroundExecutor,
    ) {
        executor.allow_parking();
        let temp_dir = tempfile::tempdir().unwrap();
        let symlink_target = temp_dir.path().join("target");
        std::fs::create_dir(&symlink_target).unwrap();
        std::fs::write(symlink_target.join("existing"), b"keep").unwrap();
        let destination_path = temp_dir.path().join("v_1");
        std::os::unix::fs::symlink(&symlink_target, &destination_path).unwrap();
        let client = StaticResponseClient {
            status: 200,
            body: b"binary".to_vec(),
        };

        download_server_raw_binary(
            &executor,
            Arc::new(client),
            "https://example.com/agent-binary",
            None,
            &destination_path,
            "agent-binary",
        )
        .await
        .unwrap();

        assert!(
            !std::fs::symlink_metadata(&destination_path)
                .unwrap()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read(destination_path.join("agent-binary")).unwrap(),
            b"binary"
        );
        assert_eq!(
            std::fs::read(symlink_target.join("existing")).unwrap(),
            b"keep"
        );
    }

    #[gpui::test]
    async fn archive_digest_mismatch_prevents_extraction_and_cleans_up_staging(
        executor: BackgroundExecutor,
    ) {
        executor.allow_parking();
        let temp_dir = tempfile::tempdir().unwrap();
        let destination_path = temp_dir.path().join("v_1");
        let client = StaticResponseClient {
            status: 200,
            body: b"not an archive".to_vec(),
        };

        let error = download_server_binary(
            &executor,
            Arc::new(client),
            "https://example.com/agent.zip",
            Some("0000000000000000000000000000000000000000000000000000000000000000"),
            &destination_path,
            AssetKind::Zip,
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("SHA-256 mismatch"));
        assert!(!destination_path.exists());
        let leftover_entries = std::fs::read_dir(temp_dir.path()).unwrap().count();
        assert_eq!(leftover_entries, 0, "staging directory should be removed");
    }
}
