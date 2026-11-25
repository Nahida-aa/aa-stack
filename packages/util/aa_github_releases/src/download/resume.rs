//! 断点续传：未完成的字节留在 `.part` 文件里，下次用 HTTP Range 续传。
//!
//! GitHub 的 release 资产不可变，因此用持久化的 `ETag`/`Last-Modified`
//! 作为 `If-Range` 校验器即可安全地跨进程续传；带 digest 时最后再校验一次。

use std::path::{Path, PathBuf};

use aa_http_client::{
    AsyncBody, HeaderMap, HttpClient, HttpRequestExt as _, RedirectPolicy, Request, StatusCode,
};
use anyhow::{Context, Result};
use futures::{AsyncReadExt as _, AsyncWriteExt as _, io::BufReader};
use sha2::{Digest, Sha256};

use super::{DownloadedAsset, replace_destination, sha256_matches, stream_file_archive};
use crate::AssetKind;

const PART_META_SUFFIX: &str = ".meta";
const HASH_BUFFER_SIZE: usize = 64 * 1024;

pub(super) async fn download_server_asset_resumable(
    http_client: &dyn HttpClient,
    url: &str,
    digest: Option<&str>,
    destination_path: &Path,
    downloaded_asset: DownloadedAsset,
) -> Result<()> {
    let destination_parent = destination_path
        .parent()
        .with_context(|| format!("destination path has no parent: {destination_path:?}"))?;
    let part_path = part_file_path(destination_path);
    let meta_path = part_meta_path(&part_path);

    fetch_to_part_file(http_client, url, digest, &part_path, &meta_path).await?;

    let staging_directory = tempfile::Builder::new()
        .prefix(".tmp-github-download-")
        .tempdir_in(destination_parent)
        .with_context(|| format!("creating staging directory in {destination_parent:?}"))?;
    let staged_path = staging_directory.path().join("download");

    match downloaded_asset {
        DownloadedAsset::Archive(asset_kind) => {
            if asset_kind != AssetKind::Gz {
                async_fs::create_dir(&staged_path)
                    .await
                    .with_context(|| format!("creating directory {staged_path:?}"))?;
            }
            let part_file = async_fs::File::open(&part_path)
                .await
                .with_context(|| format!("opening {part_path:?}"))?;
            stream_file_archive(part_file, url, &staged_path, asset_kind)
                .await
                .with_context(|| format!("extracting {url} into {staged_path:?}"))?;
        }
        DownloadedAsset::RawBinary(binary_file_name) => {
            async_fs::create_dir(&staged_path)
                .await
                .with_context(|| format!("creating directory {staged_path:?}"))?;
            let binary_path = staged_path.join(binary_file_name);
            async_fs::rename(&part_path, &binary_path)
                .await
                .with_context(|| format!("moving {part_path:?} to {binary_path:?}"))?;
            aa_util::fs::make_file_executable(&binary_path)
                .await
                .with_context(|| format!("marking {binary_path:?} as executable"))?;
        }
    }

    replace_destination(&staged_path, destination_path).await?;
    remove_if_exists(&part_path).await?;
    remove_if_exists(&meta_path).await?;
    Ok(())
}

async fn fetch_to_part_file(
    http_client: &dyn HttpClient,
    url: &str,
    digest: Option<&str>,
    part_path: &Path,
    meta_path: &Path,
) -> Result<()> {
    let mut existing_len = file_len(part_path).await;
    loop {
        fetch_once(http_client, url, part_path, meta_path, existing_len).await?;

        let Some(expected) = digest else {
            return Ok(());
        };
        let actual = file_sha256(part_path).await?;
        if sha256_matches(&actual, expected) {
            return Ok(());
        }
        if existing_len == 0 {
            anyhow::bail!("{url} asset got SHA-256 mismatch. Expected: {expected}, Got: {actual}");
        }
        // 续传可能拼上了另一个版本的字节：丢掉 `.part` 从头再来一次。
        log::warn!("{url} resumed download failed the digest check, restarting from scratch");
        remove_if_exists(part_path).await?;
        remove_if_exists(meta_path).await?;
        existing_len = 0;
    }
}

async fn fetch_once(
    http_client: &dyn HttpClient,
    url: &str,
    part_path: &Path,
    meta_path: &Path,
    existing_len: u64,
) -> Result<()> {
    let mut builder = Request::get(url)
        .follow_redirects(RedirectPolicy::FollowAll)
        // Range 的偏移是未压缩字节，避免中间层 gzip 改变长度。
        .header("accept-encoding", "identity");
    if existing_len > 0 {
        builder = builder.header("range", format!("bytes={existing_len}-"));
        if let Some(validator) = read_if_range_validator(meta_path).await {
            builder = builder.header("if-range", validator);
        }
    }
    let request = builder
        .body(AsyncBody::empty())
        .with_context(|| format!("building request for {url}"))?;
    let mut response = http_client
        .send(request)
        .await
        .with_context(|| format!("downloading release from {url}"))?;
    let status = response.status();

    if status == StatusCode::RANGE_NOT_SATISFIABLE {
        // `.part` 已经是完整文件（上次下完但没来得及解压/替换）。
        return Ok(());
    }
    anyhow::ensure!(
        status.is_success(),
        "downloading {url} failed with status {status}"
    );

    // 只有 206 才在已有字节后面追加；200 说明服务端忽略了 Range 或校验器不匹配，
    // 此时从头写，避免把两个版本的字节拼在一起。
    let append = status == StatusCode::PARTIAL_CONTENT && existing_len > 0;
    if !append {
        save_if_range_validator(meta_path, response.headers()).await?;
    }

    let mut file = if append {
        async_fs::OpenOptions::new()
            .append(true)
            .open(part_path)
            .await
            .with_context(|| format!("opening {part_path:?} for appending"))?
    } else {
        async_fs::File::create(part_path)
            .await
            .with_context(|| format!("creating {part_path:?}"))?
    };
    futures::io::copy(&mut BufReader::new(response.body_mut()), &mut file)
        .await
        .with_context(|| format!("saving downloaded contents from {url}"))?;
    file.flush()
        .await
        .with_context(|| format!("flushing {part_path:?}"))?;
    Ok(())
}

async fn read_if_range_validator(meta_path: &Path) -> Option<String> {
    let validator = async_fs::read_to_string(meta_path).await.ok()?;
    let validator = validator.trim();
    (!validator.is_empty()).then(|| validator.to_owned())
}

async fn save_if_range_validator(meta_path: &Path, headers: &HeaderMap) -> Result<()> {
    let validator = headers
        .get(aa_http_client::http::header::ETAG)
        .and_then(|value| value.to_str().ok())
        // 弱校验器不能用于 If-Range。
        .filter(|value| !value.starts_with("W/"))
        .map(str::to_owned)
        .or_else(|| {
            headers
                .get(aa_http_client::http::header::LAST_MODIFIED)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        });
    match validator {
        Some(validator) => async_fs::write(meta_path, validator.as_bytes())
            .await
            .with_context(|| format!("writing {meta_path:?}")),
        None => remove_if_exists(meta_path).await,
    }
}

async fn file_sha256(path: &Path) -> Result<String> {
    let mut file = async_fs::File::open(path)
        .await
        .with_context(|| format!("opening {path:?}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; HASH_BUFFER_SIZE];
    loop {
        let read = file
            .read(&mut buffer)
            .await
            .with_context(|| format!("reading {path:?}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

async fn file_len(path: &Path) -> u64 {
    async_fs::metadata(path)
        .await
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}

fn part_file_path(destination_path: &Path) -> PathBuf {
    let mut file_name = std::ffi::OsString::from(".");
    file_name.push(destination_path.file_name().unwrap_or_default());
    file_name.push(".part");
    match destination_path.parent() {
        Some(parent) => parent.join(file_name),
        None => PathBuf::from(file_name),
    }
}

fn part_meta_path(part_path: &Path) -> PathBuf {
    let mut file_name = part_path.file_name().unwrap_or_default().to_os_string();
    file_name.push(PART_META_SUFFIX);
    part_path.with_file_name(file_name)
}

async fn remove_if_exists(path: &Path) -> Result<()> {
    match async_fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("removing {path:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::tests::sample_zip_archive;
    use crate::{
        AssetKind, DownloadOptions, download_server_binary_with, download_server_raw_binary_with,
    };
    use aa_http_client::Response;
    use futures::{executor::block_on, future::BoxFuture};
    use std::sync::Arc;

    type RecordedRequest = (Option<String>, Option<String>);

    /// 按请求里的 `Range` 返回 206/416 的测试客户端，用来验证续传逻辑。
    struct RangeResponseClient {
        body: Vec<u8>,
        etag: &'static str,
        honor_range: bool,
        requests: Arc<std::sync::Mutex<Vec<RecordedRequest>>>,
    }

    impl HttpClient for RangeResponseClient {
        fn send(
            &self,
            req: Request<AsyncBody>,
        ) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
            let headers = req.headers();
            let range = header_value(headers, "range");
            let if_range = header_value(headers, "if-range");
            self.requests
                .lock()
                .unwrap()
                .push((range.clone(), if_range));
            let body = self.body.clone();
            let etag = self.etag;
            let honor_range = self.honor_range;
            Box::pin(async move {
                let start = range.as_deref().and_then(parse_range_start);
                if honor_range && let Some(start) = start {
                    if start < body.len() as u64 {
                        let start = start as usize;
                        let total = body.len();
                        return Ok(Response::builder()
                            .status(206)
                            .header("etag", etag)
                            .header(
                                "content-range",
                                format!("bytes {start}-{}/{total}", total - 1),
                            )
                            .body(AsyncBody::from(body[start..].to_vec()))
                            .unwrap());
                    }
                    return Ok(Response::builder()
                        .status(416)
                        .body(AsyncBody::empty())
                        .unwrap());
                }
                Ok(Response::builder()
                    .status(200)
                    .header("etag", etag)
                    .body(AsyncBody::from(body))
                    .unwrap())
            })
        }
    }

    fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    }

    fn parse_range_start(range: &str) -> Option<u64> {
        range
            .strip_prefix("bytes=")?
            .strip_suffix('-')?
            .parse()
            .ok()
    }

    #[test]
    fn resumes_raw_binary_from_part_file_with_range_and_if_range() {
        let contents = b"0123456789abcdef".to_vec();
        let expected_sha_256 = format!("{:x}", Sha256::digest(&contents));
        let temp_dir = tempfile::tempdir().unwrap();
        let destination_path = temp_dir.path().join("v_1");
        let part_path = part_file_path(&destination_path);
        std::fs::write(&part_path, &contents[..7]).unwrap();
        std::fs::write(part_meta_path(&part_path), b"\"etag-1\"").unwrap();
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let client = RangeResponseClient {
            body: contents.clone(),
            etag: "\"etag-1\"",
            honor_range: true,
            requests: requests.clone(),
        };

        block_on(download_server_raw_binary_with(
            Arc::new(client),
            "https://example.com/agent-binary",
            Some(&expected_sha_256),
            &destination_path,
            "agent-binary",
            DownloadOptions { resume: true },
        ))
        .unwrap();

        assert_eq!(
            std::fs::read(destination_path.join("agent-binary")).unwrap(),
            contents
        );
        assert!(!part_path.exists());
        assert!(!part_meta_path(&part_path).exists());
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].0.as_deref(), Some("bytes=7-"));
        assert_eq!(requests[0].1.as_deref(), Some("\"etag-1\""));
    }

    #[test]
    fn resumed_digest_mismatch_restarts_from_scratch() {
        // 服务端对 Range 请求返回被污染的字节；第二次全量请求返回正确内容。
        struct WrongResumeClient {
            correct: Vec<u8>,
            wrong_tail: Vec<u8>,
            calls: Arc<std::sync::Mutex<u32>>,
        }

        impl HttpClient for WrongResumeClient {
            fn send(
                &self,
                req: Request<AsyncBody>,
            ) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
                *self.calls.lock().unwrap() += 1;
                let has_range = req.headers().contains_key("range");
                let body = if has_range {
                    self.wrong_tail.clone()
                } else {
                    self.correct.clone()
                };
                Box::pin(async move {
                    let status = if has_range { 206 } else { 200 };
                    Ok(Response::builder()
                        .status(status)
                        .body(AsyncBody::from(body))
                        .unwrap())
                })
            }
        }

        let correct = b"the correct binary payload".to_vec();
        let expected_sha_256 = format!("{:x}", Sha256::digest(&correct));
        let temp_dir = tempfile::tempdir().unwrap();
        let destination_path = temp_dir.path().join("v_1");
        let part_path = part_file_path(&destination_path);
        std::fs::write(&part_path, b"the wrong").unwrap();
        let calls = Arc::new(std::sync::Mutex::new(0));
        let client = WrongResumeClient {
            correct: correct.clone(),
            wrong_tail: b" tail".to_vec(),
            calls: calls.clone(),
        };

        block_on(download_server_raw_binary_with(
            Arc::new(client),
            "https://example.com/agent-binary",
            Some(&expected_sha_256),
            &destination_path,
            "agent-binary",
            DownloadOptions { resume: true },
        ))
        .unwrap();

        assert_eq!(*calls.lock().unwrap(), 2);
        assert_eq!(
            std::fs::read(destination_path.join("agent-binary")).unwrap(),
            correct
        );
        assert!(!part_path.exists());
    }

    #[test]
    fn completed_part_file_is_extracted_without_refetching() {
        let archive = sample_zip_archive();
        let expected_sha_256 = format!("{:x}", Sha256::digest(&archive));
        let temp_dir = tempfile::tempdir().unwrap();
        let destination_path = temp_dir.path().join("v_1");
        let part_path = part_file_path(&destination_path);
        std::fs::write(&part_path, &archive).unwrap();
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let client = RangeResponseClient {
            body: archive,
            etag: "\"zip-etag\"",
            honor_range: true,
            requests: requests.clone(),
        };

        block_on(download_server_binary_with(
            Arc::new(client),
            "https://example.com/agent.zip",
            Some(&expected_sha_256),
            &destination_path,
            AssetKind::Zip,
            DownloadOptions { resume: true },
        ))
        .unwrap();

        assert_eq!(
            std::fs::read(destination_path.join("agent")).unwrap(),
            b"hello"
        );
        assert!(!part_path.exists());
        assert_eq!(requests.lock().unwrap().len(), 1, "416 之后不应再发请求");
    }
}
