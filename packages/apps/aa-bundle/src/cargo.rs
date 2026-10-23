//! cargo 构建环境查询：target 目录、profile 目录名、产物路径、 crate 清单。
//!
//! 这里只**问** cargo / 读 manifest / 拼路径，不执行构建——构建是项目自己的事
//! （aa-bundle 只打包）。两类查询被各打包格式共用（pkgbuild / tar.gz 都依赖
//! 本模块，格式之间互不依赖）：
//!
//! - **构建环境**：target 目录 / profile 目录名 / 产物路径
//! - **crate 清单**：`read_package_info` 读项目 Cargo.toml 取打包元数据，
//!   `resolve_bin_name` 按 cargo 的自动发现规则定安装哪个二进制

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// cargo 的 `target_directory`：`cargo metadata --no-deps` 问出。
///
/// **workspace 成员的 target 由 workspace 根决定**，不在 crate 目录下——
/// 把产物/输出猜在 `<crate>/target/` 是多 crate workspace 下的经典错误。
/// 这里问 cargo 而不是拼路径，顺带尊重 `CARGO_TARGET_DIR`；metadata 拿不到
/// （cargo 不在 PATH 等）时退回 `<project_dir>/target`，由调用方的存在性
/// 检查兜底。`--no-deps` 只读 manifest，不解析依赖、不联网。
pub fn resolve_target_dir(project_dir: &Path) -> PathBuf {
    std::process::Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(project_dir)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("target_directory")?.as_str().map(PathBuf::from))
        .unwrap_or_else(|| project_dir.join("target"))
}

/// cargo profile 名 → target 下的产物目录名。`dev` 落 `debug`，其余同名
/// （`release` / `release-quick` / 自定义 profile）。
pub fn profile_dir(profile: &str) -> &str {
    if profile == "dev" { "debug" } else { profile }
}

/// cargo profile 名 → `cargo build` 的 flag。`release` 用简写（保持与旧输出
/// 一致），其余走 `--profile <name>`。
pub fn profile_flag(profile: &str) -> String {
    if profile == "release" {
        "--release".to_string()
    } else {
        format!("--profile {profile}")
    }
}

/// 二进制路径：`resolve_target_dir` 问出 workspace 根的 target/，拼
/// `<profile 目录>/<bin>`（也尊重 `CARGO_TARGET_DIR`）。
///
/// 各格式共用：pkgbuild 的源码模式（package() 拼路径）、prebuilt 模式（生成期
/// 定位）与 tar.gz（本来就只打包不构建）都要它。
pub fn resolve_binary_path(project_dir: &Path, bin: &str, profile: &str) -> PathBuf {
    resolve_target_dir(project_dir)
        .join(profile_dir(profile))
        .join(bin)
}

// ---- crate 清单查询（各格式共用，故住在这里而非某个 format 模块）----

/// crate 的 Cargo.toml 里我们关心的元数据。
#[derive(Debug, Clone)]
pub struct PackageInfo {
    pub name: String,
    pub version: String,
    pub description: String,
    pub license: String,
    /// `[[bin]]` 目标名。**只有恰好一个 bin 目标时才有值** —— 多个就必须由项目声明，
    /// 因为 CLI 无从判断该用哪个。
    pub bin_name: Option<String>,
    /// `[[bin]]` 目标数量（0 = 纯 lib crate）
    pub bin_count: usize,
}

/// `license` 可能是字面量，也可能是 `license.workspace = true`（workspace 继承）。
/// `workspace` 字段只是用来让 serde 认出这个形状，值本身不读。
#[allow(dead_code)]
#[derive(Deserialize)]
#[serde(untagged)]
enum StringOrWorkspace {
    Literal(String),
    Inherited { workspace: bool },
}

#[derive(Deserialize)]
struct CrateManifest {
    package: Option<CratePackage>,
    bin: Option<Vec<CrateBin>>,
}

#[derive(Deserialize)]
struct CrateBin {
    name: String,
}

#[derive(Deserialize)]
struct CratePackage {
    name: String,
    version: String,
    description: Option<String>,
    license: Option<StringOrWorkspace>,
}

#[derive(Deserialize)]
struct WorkspaceManifest {
    workspace: Option<WorkspaceTable>,
}

#[derive(Deserialize)]
struct WorkspaceTable {
    package: Option<WorkspacePackage>,
}

#[derive(Deserialize)]
struct WorkspacePackage {
    license: Option<String>,
}

/// 读 crate 的 Cargo.toml；`license.workspace = true` 时上溯到 workspace 根取值。
pub fn read_package_info(project_dir: &Path) -> Result<PackageInfo> {
    let manifest_path = project_dir.join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("读取 {} 失败", manifest_path.display()))?;
    let manifest: CrateManifest =
        toml::from_str(&text).with_context(|| format!("解析 {} 失败", manifest_path.display()))?;
    let package = manifest.package.context(
        "Cargo.toml 没有 [package] 段（传的是 workspace 根？请用 -p 指向具体 crate 目录）",
    )?;

    let license = match package.license {
        Some(StringOrWorkspace::Literal(s)) => s,
        Some(StringOrWorkspace::Inherited { .. }) => find_workspace_root(project_dir)
            .and_then(|root| {
                let text = std::fs::read_to_string(root.join("Cargo.toml")).ok()?;
                let ws: WorkspaceManifest = toml::from_str(&text).ok()?;
                ws.workspace?.package?.license
            })
            .context("license 是 workspace 继承，但没能从 workspace 根解析出值（请在 workspace 根的 [workspace.package] 补 license）")?,
        None => bail!("Cargo.toml 的 [package] 缺少 license（请在 [package] 补 license = \"...\"，或让 workspace 根继承）"),
    };

    let bins: Vec<String> = manifest
        .bin
        .unwrap_or_default()
        .into_iter()
        .map(|b| b.name)
        .collect();

    Ok(PackageInfo {
        name: package.name,
        version: package.version,
        description: package.description.unwrap_or_default(),
        license,
        bin_name: (bins.len() == 1).then(|| bins[0].clone()),
        bin_count: bins.len(),
    })
}

/// 向上找含 `[workspace]` 的 Cargo.toml 所在目录。
fn find_workspace_root(start: &Path) -> Option<PathBuf> {
    let mut dir = start.canonicalize().ok()?;
    loop {
        let candidate = dir.join("Cargo.toml");
        if let Ok(text) = std::fs::read_to_string(&candidate) {
            if toml::from_str::<WorkspaceManifest>(&text)
                .ok()
                .and_then(|m| m.workspace)
                .is_some()
            {
                return Some(dir);
            }
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// bin 名的推导链（对齐 cargo 的自动发现规则）：
/// 1. 覆盖值（最高优先）
/// 2. 有 `[[bin]]` 且唯一 → 该目标名（可能与 package.name 不同）
/// 3. 无 `[[bin]]` → **包名**。这是 cargo 的规则：`src/main.rs` 自动成为以
///    包名命名的二进制。不是猜，是 cargo 文档化的行为。
/// 4. 多个 `[[bin]]` → 歧义，报错要求声明
pub fn resolve_bin_name<'a>(
    info: &'a PackageInfo,
    override_name: Option<&'a str>,
    project_dir: &Path,
) -> Result<&'a str> {
    if let Some(b) = override_name {
        return Ok(b);
    }
    match info.bin_count {
        0 => Ok(info.name.as_str()),
        1 => Ok(info
            .bin_name
            .as_deref()
            .expect("bin_count==1 时必有 bin_name")),
        n => bail!(
            "{} 有 {n} 个 [[bin]] 目标，无法判断该装哪个。\
             请用 --bin-name 指定，或在 bundle 元数据里加 `[bundle.arch]` 后\
             用 --bundle-meta 指向含 bin 配置的表。",
            project_dir.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_helpers_map_dev_and_release() {
        // dev → cargo 的 debug 目录；其余 profile 同名
        assert_eq!(profile_dir("dev"), "debug");
        assert_eq!(profile_dir("release"), "release");
        assert_eq!(profile_dir("release-quick"), "release-quick");
        // release 用简写 flag（历史输出兼容），其余 --profile
        assert_eq!(profile_flag("release"), "--release");
        assert_eq!(profile_flag("dev"), "--profile dev");
        assert_eq!(profile_flag("release-quick"), "--profile release-quick");
    }

    #[test]
    fn resolve_binary_path_uses_target_dir_and_profile_dir() {
        // 与既有 subprocess 测试同模式：临时目录里 cargo metadata 问出
        // target_directory（= <dir>/target），拼 <profile 目录>/<bin>。
        let dir =
            std::env::temp_dir().join(format!("aa-bundle-cargo-{}-bin_path", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let p = resolve_binary_path(&dir, "aacode", "release-quick");
        assert_eq!(p, dir.join("target").join("release-quick").join("aacode"));
        // dev 落 debug 目录
        let p = resolve_binary_path(&dir, "aacode", "dev");
        assert_eq!(p, dir.join("target").join("debug").join("aacode"));
    }
}
