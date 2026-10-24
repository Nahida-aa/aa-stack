//! 把上游 tar.gz 变成 Arch 原生包：合成最小 bundle 元数据 → 调 aa-bundle
//! 生成 PKGBUILD → 落到构建目录。
//!
//! ## 为什么要合成 bundle.toml
//!
//! aa-bundle 的身份系统要求至少一份元数据（`meta.rs` 对空引用列表直接报错），
//! 且 `.desktop` 生成分支在没有 `identifier` 时会去推导 git 仓库名——而构建
//! 目录在 `~/.cache` 里，推导必失败。第三方软件本来就没有 bundle.toml，所以
//! 这里按归档名合成一份最小表（identifier / name / short_description），
//! 版本仍由 aa-bundle 的推断链兜底。
//!
//! ## 两个约束
//!
//! - 构建目录里**不能有 Cargo.toml**：aa-bundle 会读它当 crate 清单，误取其
//!   license / name
//! - 每次生成前清掉 `src/`、`pkg/`、`*.pkg.tar.*`：makepkg 的残留会让旧解包
//!   内容混进 `package()`

use std::path::{Path, PathBuf};

use aa_bundle::formats::pkgbuild::{BinPkgOptions, generate_pkgbuild_bin};
use aa_bundle::fs::write_file;
use aa_bundle::source::SourceInput;
use anyhow::{Context as _, Result, anyhow};

use crate::lookup::display_name_from_archive;

/// 打包结果（供 [`crate::plan`] 渲染与执行）。
#[derive(Debug, Clone)]
pub struct BundlePlan {
    pub dir: PathBuf,
    pub pkgname: String,
    pub pkgver: String,
    /// 自动生成的 .desktop（tarball 自带时不需要，为 `None`）
    pub desktop: Option<PathBuf>,
}

/// 构建目录：`$XDG_CACHE_HOME/aa-install/<slug>`。
///
/// 放在缓存而非临时目录：makepkg 的产物（`src/`、`pkg/`、`*.pkg.tar.zst`）要能
/// 事后查看，且重复安装时复用同一目录。
pub fn build_dir_for(slug: &str) -> PathBuf {
    let base = dirs::cache_dir().unwrap_or_else(|| PathBuf::from(".cache"));
    base.join("aa-install").join(slug)
}

/// 合成最小 bundle 元数据。
///
/// `identifier` 只在「tarball 没有 .desktop、需要生成一份」的分支用到；
/// vendor 自带 .desktop 时它是死字段。
pub fn synthesize_bundle_toml(identifier: &str, name: &str, description: &str) -> String {
    format!(
        r#"# 由 aa-install 合成：上游没有 bundle 元数据，而 aa-bundle 要求至少一份。
identifier = "{identifier}"
name = "{name}"
short_description = "{description}"
"#
    )
}

/// 生成 PKGBUILD（含可能的 .desktop）并落到构建目录。
///
/// `source` 是原文（本地路径或 URL）；`keyword` 只用于目录名与 identifier。
pub fn generate(
    source: &str,
    keyword: &str,
    bin_name: Option<&str>,
    prefix: Option<&str>,
) -> Result<BundlePlan> {
    let archive_name = SourceInput::parse(source)?.archive_name();
    let display = display_name_from_archive(&archive_name);
    let dir = build_dir_for(keyword);
    prepare_dir(&dir)?;

    let meta_file = "bundle.toml";
    write_file(
        &dir.join(meta_file),
        &synthesize_bundle_toml(
            &format!("com.local.{keyword}"),
            &display,
            &format!("{display}（由 aa-install 重打包上游预编译包）"),
        ),
    )?;

    let opts = BinPkgOptions {
        project_dir: &dir,
        bundle_meta: vec![meta_file.to_string()],
        source,
        expected_sha256: None,
        bin_name,
        icon_name: None,
        desktop_template: None,
        depends: None,
        arch: None,
        url: None,
        license: None,
        prefix,
    };
    let (pkgbuild, desktop) = generate_pkgbuild_bin(&opts)?;
    write_file(&dir.join("PKGBUILD"), &pkgbuild)?;

    let desktop = match desktop {
        Some((name, content)) => {
            let p = dir.join(&name);
            write_file(&p, &content)?;
            Some(p)
        }
        None => None,
    };

    let pkgname = pkgbuild_field(&pkgbuild, "pkgname")
        .ok_or_else(|| anyhow!("生成的 PKGBUILD 里找不到 pkgname"))?;
    let pkgver = pkgbuild_field(&pkgbuild, "pkgver")
        .ok_or_else(|| anyhow!("生成的 PKGBUILD 里找不到 pkgver"))?;

    Ok(BundlePlan {
        dir,
        pkgname,
        pkgver,
        desktop,
    })
}

/// 清掉 makepkg 的残留，只留我们自己写的文件。
fn prepare_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("创建构建目录 {} 失败", dir.display()))?;
    for name in ["src", "pkg"] {
        let p = dir.join(name);
        if p.exists() {
            std::fs::remove_dir_all(&p).with_context(|| format!("清理 {} 失败", p.display()))?;
        }
    }
    for entry in std::fs::read_dir(dir).with_context(|| format!("读 {} 失败", dir.display()))? {
        let path = entry?.path();
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if name.contains(".pkg.tar.") {
            std::fs::remove_file(&path)
                .with_context(|| format!("删除旧包 {} 失败", path.display()))?;
        }
    }
    Ok(())
}

/// 从生成的 PKGBUILD 里回读一个顶层变量（`pkgname=foo`）。
fn pkgbuild_field(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let rest = line.strip_prefix(key)?;
        let rest = rest.strip_prefix('=')?;
        Some(rest.trim().to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthesize_toml_shape() {
        let t = synthesize_bundle_toml("com.local.foo", "Foo", "Foo（重打包）");
        assert!(t.contains("identifier = \"com.local.foo\""));
        assert!(t.contains("name = \"Foo\""));
    }

    #[test]
    fn build_dir_under_cache() {
        let d = build_dir_for("elyterminal");
        assert!(d.ends_with("aa-install/elyterminal"), "{}", d.display());
    }

    #[test]
    fn pkgbuild_field_reads_top_level_vars() {
        let text = "pkgname=elyterminal-bin\npkgver=0.1.8\npkgrel=1\n";
        assert_eq!(
            pkgbuild_field(text, "pkgname").as_deref(),
            Some("elyterminal-bin")
        );
        assert_eq!(pkgbuild_field(text, "pkgver").as_deref(), Some("0.1.8"));
        assert_eq!(pkgbuild_field(text, "pkgdesc"), None);
    }

    /// 真跑：本地 fixture tar.gz（不联网），断言生成物的关键行。
    #[test]
    fn generate_from_local_archive() {
        let d = std::env::temp_dir().join(format!("aa-install-fixture-{}", std::process::id()));
        let staging = d.join("st/ELYTerminal-0.1.8-linux-x86_64");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("elyterminal"), b"\x7fELF0201").unwrap();
        std::fs::write(
            staging.join("com.zacharyzhang.elyterminal.desktop"),
            "[Desktop Entry]\nName=ELYTerminal\nExec=elyterminal\n",
        )
        .unwrap();
        std::fs::write(staging.join("com.zacharyzhang.elyterminal.svg"), "<svg/>").unwrap();
        let status = std::process::Command::new("tar")
            .args([
                "-czf",
                d.join("ELYTerminal-0.1.8-linux-x86_64.tar.gz")
                    .to_str()
                    .unwrap(),
                "-C",
                d.join("st").to_str().unwrap(),
                "ELYTerminal-0.1.8-linux-x86_64",
            ])
            .status()
            .unwrap();
        assert!(status.success(), "建 fixture 失败");

        let gz = d.join("ELYTerminal-0.1.8-linux-x86_64.tar.gz");
        let plan = generate(gz.to_str().unwrap(), "elyterminal", None, None).unwrap();
        assert_eq!(plan.pkgname, "elyterminal-bin");
        assert_eq!(plan.pkgver, "0.1.8");
        let text = std::fs::read_to_string(plan.dir.join("PKGBUILD")).unwrap();
        assert!(text.contains("sha256sums=('"), "{text}");
        assert!(text.contains("file://"), "本地源应写 file://");
        assert!(plan.desktop.is_none(), "tarball 自带 .desktop 时不该生成");
        std::fs::remove_dir_all(&d).unwrap();
    }
}
