//! `aa-bundle` 的 CLI 壳 —— 参数解析后全部转调库侧（`aa_bundle::pkgbuild` /
//! `aa_bundle::targz`）。
//!
//! 设计原则（对齐 cargo-bundle）：**打包元数据声明在 Cargo.toml，不在命令行**。
//! 所以 CLI flag 都是「覆盖项」而非「必填项」，且只保留**所有格式通用**的：
//! 格式特有设置（Arch 的 depends、安装前缀…）一律声明在 bundle 元数据的子表里
//! （如 `[bundle.arch]`）——否则 `--format tar.gz` 时用户会看到一堆对当前格式
//! 无效的 flag。库 API 仍接受这些覆盖项（见 `PkgbuildOptions`），留给程序化调用。
//!
//! 格式是枚举而不是子命令：`pkgbuild` / `tar.gz` /（将来的）`deb` / `rpm`
//! 是同一件事——「把这个 crate 打成目标格式的安装物」——的不同取值，共用
//! 一套通用 flag。加新格式 = 库侧加一个模块 + 这里加一个枚举变体。

use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Args, Parser, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "aa-bundle", about = "打包工具：身份解析 / 各平台安装包生成")]
struct Cli {
    /// 目标格式：pkgbuild = Arch PKGBUILD（makepkg 的输入，不是安装包本身）；
    /// tar.gz = 自包含应用包（zed.app 布局，跨发行版解包即用）
    #[arg(long, required = true)]
    format: Format,
    #[command(flatten)]
    common: CommonArgs,
}

#[derive(ValueEnum, Clone, Debug)]
enum Format {
    #[value(name = "pkgbuild")]
    Pkgbuild,
    #[value(name = "tar.gz", alias = "targz")]
    TarGz,
}

/// 所有格式通用的参数。
#[derive(Debug, Args)]
struct CommonArgs {
    /// 源码目录（含 Cargo.toml 与 resources/），默认当前目录。
    /// 给 `--source` 时它只作 `--bundle-meta` 引用的解析基目录
    /// （此时不必是 cargo 工程，二进制来自上游 tarball）
    #[arg(short = 'p', long, default_value = ".")]
    project_dir: PathBuf,
    /// 上游源：tar.gz 的**本地路径**或 **http(s) URL**。生成 AUR `-bin` 形态
    /// PKGBUILD：makepkg 自己下载（`source=()` + `sha256sums=()`），包内不构建；
    /// 本地路径转成 `file://` source。**zip 暂不支持（规划中）**。
    /// 与 `--prebuilt` / `--bin-path` 互斥（payload 来源只能是一种）；
    /// `--profile` 在此模式下无意义（忽略）。
    #[arg(long, value_name = "PATH_OR_URL", conflicts_with_all = ["prebuilt", "bin_path"])]
    source: Option<String>,
    /// 期望的 sha256（十六进制）：给了就在获取后校验，不匹配报错退出；不给则
    /// 由 aa-bundle 计算并写入 PKGBUILD 的 `sha256sums=()`。
    #[arg(long, value_name = "HEX", requires = "source")]
    sha256: Option<String>,
    /// 身份/打包配置来源（**必填，可重复**）：`<文件>` 或 `<文件>#[k1, k2, ...]`。
    /// 字段对齐 cargo-bundle 的 BundleSettings；格式特有设置放同表的子表
    /// （如 `[bundle.arch]`）。
    /// 例：`Cargo.toml#[package, metadata, bundle-stable]`
    /// （这只是 aacode 的写法，不是约定 —— 你的项目放哪都行）
    ///
    /// 重复给出时按顺序**分层**：后者递归覆盖前者（对象递归合并，数组/标量
    /// 整体替换）。典型用法是先给各 channel 的共享表，再给 channel 专属表：
    /// `--bundle-meta "Cargo.toml#[package, metadata, bundle-shared]" \
    ///  --bundle-meta "Cargo.toml#[package, metadata, bundle-stable]"`
    #[arg(long, required = true)]
    bundle_meta: Vec<String>,
    /// 覆盖二进制名（默认取 Cargo.toml 的 [[bin]] 推导链）
    #[arg(long)]
    bin_name: Option<String>,
    /// 覆盖图标基础名（默认取二进制名，仅作图标安装文件名）
    #[arg(long)]
    icon_name: Option<String>,
    /// 覆盖 .desktop 模板路径
    #[arg(long)]
    desktop_template: Option<PathBuf>,
    /// 输出位置：pkgbuild 为目录（默认 <workspace target>/aa-bundle/pkgbuild/<bin>）；
    /// tar.gz 为文件（默认 <workspace target>/aa-bundle/targz/<bin>-linux-<arch>.tar.gz）
    #[arg(long)]
    out: Option<PathBuf>,
    /// 要打包的 cargo 构建 profile（默认 release）。安装**已编好**的产物时用，
    /// 如 `--profile release-quick`：pkgbuild 的 build() 会变成
    /// `cargo build --profile release-quick`，package() 取 target/release-quick/<bin>。
    #[arg(long, default_value = "release")]
    profile: String,
    /// 二进制打包模式（AUR `-bin` 形态）：不现场编译，安装已编好的产物。
    /// pkgbuild：build() 只校验、package() 直接 install 生成期钉死的
    /// target/<profile>/<bin>，makedepends 默认清空。tar.gz 本就是预编译打包，
    /// 此 flag 无额外作用。重新构建后需重新生成，否则装旧产物。
    #[arg(long, default_value_t = false)]
    prebuilt: bool,
    /// 直接指定二进制路径（默认 <target>/<profile>/<bin>）。生成时即校验存在性；
    /// pkgbuild prebuilt 模式下 canonicalize 成绝对路径钉死进 PKGBUILD。
    #[arg(long)]
    bin_path: Option<PathBuf>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.format {
        Format::TarGz => {
            if cli.common.source.is_some() {
                bail!(
                    "--source 暂不支持 --format tar.gz：v1 仅 pkgbuild 支持上游源重打包\
                     （tar.gz 的「解包后重新 staging」为规划中）"
                );
            }
            run_targz(&cli.common)?;
        }
        Format::Pkgbuild => {
            if let Some(source) = &cli.common.source {
                run_pkgbuild_bin(&cli.common, source)?;
            } else {
                run_pkgbuild_local(&cli.common)?;
            }
        }
    }
    Ok(())
}

/// 上游 tarball → AUR `-bin` PKGBUILD。
fn run_pkgbuild_bin(common: &CommonArgs, source: &str) -> Result<()> {
    let opts = aa_bundle::formats::pkgbuild::BinPkgOptions {
        project_dir: &common.project_dir,
        bundle_meta: common.bundle_meta.clone(),
        source,
        expected_sha256: common.sha256.as_deref(),
        bin_name: common.bin_name.as_deref(),
        icon_name: common.icon_name.as_deref(),
        desktop_template: common.desktop_template.as_deref(),
        // 格式特有设置走 meta 子表（[bundle.arch]），CLI 不传覆盖
        depends: None,
        arch: None,
        url: None,
        license: None,
        prefix: None,
    };
    let (pkgbuild, desktop) = aa_bundle::formats::pkgbuild::generate_pkgbuild_bin(&opts)?;

    // 输出目录：source 模式没有 cargo 工程，不能走 default_out_dir（它要读
    // Cargo.toml）；AUR 工作流就是「cd 进一个目录 → PKGBUILD 落在那里」。
    let out_dir = match &common.out {
        Some(p) => p.clone(),
        None => common.project_dir.clone(),
    };
    aa_bundle::fs::write_file(&out_dir.join("PKGBUILD"), &pkgbuild)?;
    println!("已生成 {}", out_dir.join("PKGBUILD").display());
    if let Some((name, rendered)) = desktop {
        aa_bundle::fs::write_file(&out_dir.join(&name), &rendered)?;
        println!("已生成 {}", out_dir.join(&name).display());
    }

    println!("接下来：");
    println!("  cd {}", out_dir.display());
    println!("  makepkg -si     # 下载上游 tarball、校验 sha256 并安装");
    Ok(())
}

/// 本地产物/源码 → PKGBUILD（既有路径）。
fn run_pkgbuild_local(common: &CommonArgs) -> Result<()> {
    let opts = aa_bundle::formats::pkgbuild::PkgbuildOptions {
        project_dir: &common.project_dir,
        bundle_meta: common.bundle_meta.clone(),
        bin_name: common.bin_name.as_deref(),
        desktop_template: common.desktop_template.as_deref(),
        icon_name: common.icon_name.as_deref(),
        // 格式特有设置走 meta 子表（[bundle.arch]），CLI 不传覆盖
        depends: None,
        makedepends: None,
        arch: None,
        url: None,
        license: None,
        prefix: None,
        profile: Some(common.profile.as_str()),
        prebuilt: common.prebuilt,
        bin_path: common.bin_path.as_deref(),
    };
    let (pkgbuild, desktop) = aa_bundle::formats::pkgbuild::generate_pkgbuild(&opts)?;

    let out_dir = match &common.out {
        Some(p) => p.clone(),
        None => aa_bundle::formats::pkgbuild::default_out_dir(
            &common.project_dir,
            common.bin_name.as_deref(),
        )?,
    };
    aa_bundle::fs::write_file(&out_dir.join("PKGBUILD"), &pkgbuild)?;
    println!("已生成 {}", out_dir.join("PKGBUILD").display());
    if let Some((name, rendered)) = desktop {
        aa_bundle::fs::write_file(&out_dir.join(&name), &rendered)?;
        println!("已生成 {}", out_dir.join(&name).display());
    }

    println!("接下来：");
    println!("  cd {}", out_dir.display());
    println!("  makepkg -si        # 构建并安装（卸载用 pacman -R <pkgname>）");
    Ok(())
}

/// tar.gz 自包含应用包（既有路径）。
fn run_targz(common: &CommonArgs) -> Result<()> {
    if common.prebuilt {
        println!("提示：tar.gz 本就是预编译打包（只安装已编好的二进制），--prebuilt 无额外作用。");
    }
    let opts = aa_bundle::formats::targz::TargzOptions {
        project_dir: &common.project_dir,
        bundle_meta: common.bundle_meta.clone(),
        bin_name: common.bin_name.as_deref(),
        icon_name: common.icon_name.as_deref(),
        desktop_template: common.desktop_template.as_deref(),
        out: common.out.as_deref(),
        bin_path: common.bin_path.as_deref(),
        profile: Some(common.profile.as_str()),
    };
    aa_bundle::formats::targz::generate(&opts)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_conflicts_with_prebuilt_and_bin_path() {
        let err = Cli::try_parse_from([
            "aa-bundle",
            "--format",
            "pkgbuild",
            "--bundle-meta",
            "bundle.toml",
            "--source",
            "https://example.com/a.tar.gz",
            "--prebuilt",
        ])
        .unwrap_err();
        assert!(
            err.to_string().contains("prebuilt"),
            "--source 应与 --prebuilt 互斥：{err}"
        );

        let err = Cli::try_parse_from([
            "aa-bundle",
            "--format",
            "pkgbuild",
            "--bundle-meta",
            "bundle.toml",
            "--source",
            "a.tar.gz",
            "--bin-path",
            "/tmp/x",
        ])
        .unwrap_err();
        assert!(err.to_string().contains("bin-path"), "{err}");
    }

    #[test]
    fn sha256_requires_source() {
        let err = Cli::try_parse_from([
            "aa-bundle",
            "--format",
            "pkgbuild",
            "--bundle-meta",
            "bundle.toml",
            "--sha256",
            "abc",
        ])
        .unwrap_err();
        assert!(
            err.to_string().contains("source"),
            "--sha256 需要 --source：{err}"
        );
    }

    #[test]
    fn source_with_meta_parses() {
        let cli = Cli::try_parse_from([
            "aa-bundle",
            "--format",
            "pkgbuild",
            "--bundle-meta",
            "bundle.toml",
            "--source",
            "https://example.com/a.tar.gz",
            "--sha256",
            "abc",
        ])
        .unwrap();
        assert_eq!(
            cli.common.source.as_deref(),
            Some("https://example.com/a.tar.gz")
        );
        assert_eq!(cli.common.sha256.as_deref(), Some("abc"));
    }
}
