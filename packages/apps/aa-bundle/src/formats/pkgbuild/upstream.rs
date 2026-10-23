//! 上游 tar.gz → 正统 AUR `-bin` PKGBUILD。
//!
//! 与 [`crate::formats::pkgbuild::generate_pkgbuild`]（本地产物/源码两种形态）**并列**：
//! 那条路把本地构建产物装进包（生成期钉死路径，本机安装用，不能分发）；本模块走
//! `source=()` + `sha256sums=()`，由 makepkg 自己下载并校验——这才是 AUR `-bin`
//! 包的正确形态，生成物可以分发给别人。
//!
//! ## 两个分支
//!
//! - **vendor 分支**：tarball 自带 `.desktop` → 直接装上游的文件（保持上游的
//!   `Exec=` / `Icon=` 自洽），不生成 .desktop。
//! - **生成分支**：tarball 没有 `.desktop` → 按 bundle 元数据生成（与既有
//!   pkgbuild 一致），随 PKGBUILD 同目录写出、从 `$startdir` 装。
//!
//! 命名差异是**有意设计**：vendor 分支保持上游名，生成分支用规范名（保证生成的
//! `.desktop` 里 `Icon=` 与安装后的图标文件名对得上）。同理，`meta.icon` 只在
//! tarball 没有图标时生效。

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::cargo::read_package_info;
use crate::meta::BundleMeta;
use crate::source::{
    SourceInput, infer_arch, infer_project_url, infer_version, inspect_targz, resolve_source,
};

/// `-bin` PKGBUILD 生成参数。
///
/// 与 [`crate::formats::pkgbuild::PkgbuildOptions`] 对称：`bundle_meta` 仍是身份的
/// 单一来源；`project_dir` 只作 meta 引用的解析基目录（`--source` 模式下它不必是
/// cargo 工程）。
pub struct BinPkgOptions<'a> {
    /// bundle 元数据引用的解析基目录
    pub project_dir: &'a Path,
    /// 身份来源（**必填**）：`<文件>` 或 `<文件>#[k1, k2, ...]`，可重复分层
    pub bundle_meta: Vec<String>,
    /// `--source` 原文：本地路径或 http(s) URL
    pub source: &'a str,
    /// 期望的 sha256（给了就校验，不给则由本工具算出并写入）
    pub expected_sha256: Option<&'a str>,
    /// 覆盖二进制名（同时用于多 ELF 消歧）
    pub bin_name: Option<&'a str>,
    /// 覆盖图标安装名
    pub icon_name: Option<&'a str>,
    /// 覆盖 .desktop 模板路径
    pub desktop_template: Option<&'a Path>,
    // 以下是库 API 覆盖项（CLI 不暴露，格式特有设置一律声明在 meta 的 [bundle.arch]）
    pub depends: Option<&'a [String]>,
    pub arch: Option<&'a [String]>,
    pub url: Option<&'a str>,
    pub license: Option<&'a str>,
    pub prefix: Option<&'a str>,
}

/// 生成 `-bin` PKGBUILD。
///
/// 返回形状与既有 `generate_pkgbuild` 一致；`Some((desktop 文件名, 内容))` **仅当
/// 自行生成了 .desktop**（vendor 分支返回 `None`——装上游文件，不需要同目录副本）。
pub fn generate_pkgbuild_bin(
    opts: &BinPkgOptions<'_>,
) -> Result<(String, Option<(String, String)>)> {
    let input = SourceInput::parse(opts.source)?;
    let resolved = resolve_source(&input, opts.expected_sha256)?;
    let layout = inspect_targz(&resolved, opts.bin_name)?;
    let identity: BundleMeta =
        crate::meta::load_bundle_meta_layered(&opts.bundle_meta, opts.project_dir)?;
    let meta_ref = opts.bundle_meta.join(" + ");

    // —— 安装名：vendor 分支保持上游名，生成分支用规范名 ——
    let has_vendor_desktop = !layout.desktops.is_empty();
    let bin: &str = if has_vendor_desktop {
        &layout.bin_name
    } else {
        opts.bin_name.unwrap_or(&layout.bin_name)
    };
    let pkgname = pkgname_with_bin_suffix(bin);
    let prefix = opts
        .prefix
        .or(identity.arch.as_ref().and_then(|a| a.prefix.as_deref()))
        .unwrap_or("/usr")
        .trim_end_matches('/');

    // —— pkgver：meta > 归档名 > 包装目录名 ——
    let pkgver = opts_version(&identity)
        .or_else(|| infer_version(&resolved.input.archive_name()))
        .or_else(|| layout.wrapper.as_deref().and_then(infer_version))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "推不出 pkgver：请在 bundle 元数据补 version \
                 （或让归档名/目录名带版本号，如 `App-0.1.8-linux-x86_64.tar.gz`）"
            )
        })?;
    validate_pkgver(&pkgver)?;

    // —— pkgdesc ——
    // --source 模式下 project_dir 未必是 cargo 工程，读不到就当没有（不报错）
    let cargo_info = read_package_info(opts.project_dir).ok();
    let cargo_desc = cargo_info
        .as_ref()
        .map(|i| i.description.clone())
        .unwrap_or_default();
    let pkgdesc = identity
        .short_description
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| (!cargo_desc.is_empty()).then(|| cargo_desc.clone()))
        .or_else(|| identity.name.clone())
        .unwrap_or_else(|| bin.to_string());
    if pkgdesc.trim().is_empty() {
        bail!("pkgdesc 为空：请在 bundle 元数据补 short_description 或 name");
    }

    // —— license / url / arch / depends ——
    let license = opts
        .license
        .or(identity.license.as_deref())
        .or_else(|| cargo_info.as_ref().map(|i| i.license.as_str()));
    let arch: Vec<String> = match opts.arch {
        Some(v) => v.to_vec(),
        None => identity
            .arch
            .as_ref()
            .and_then(|a| a.arch.clone())
            .or_else(|| {
                let a = infer_arch(&resolved.input.archive_name())?;
                Some(vec![a.to_string()])
            })
            .unwrap_or_else(|| vec!["x86_64".to_string()]),
    };
    let depends: Vec<String> = match opts.depends {
        Some(v) => v.to_vec(),
        None => identity
            .arch
            .as_ref()
            .and_then(|a| a.depends.clone())
            .unwrap_or_default(),
    };
    let url: Option<String> = opts
        .url
        .map(str::to_string)
        .or_else(|| identity.arch.as_ref().and_then(|a| a.url.clone()))
        .or_else(|| match &input {
            // GitHub release asset → 仓库首页；否则退回 URL 原文
            SourceInput::Url(u) => Some(infer_project_url(u).unwrap_or_else(|| u.clone())),
            SourceInput::Local(_) => None,
        });

    // —— package() 各行 ——
    // makepkg 把 tarball 解到 $srcdir，解出来的顶层就是（可能的）包装目录。
    let srcdir_prefix = match &layout.wrapper {
        Some(w) => format!("{w}/"),
        None => String::new(),
    };
    let install_bin = format!(
        "  install -Dm755 \"$srcdir/{srcdir_prefix}{}\" \"$pkgdir{prefix}/bin/{bin}\"\n",
        layout.binary.display()
    );

    let mut install_rest = String::new();
    let mut generated_desktop = None;

    if has_vendor_desktop {
        for d in &layout.desktops {
            let base = file_name_of(d);
            install_rest.push_str(&format!(
                "  install -Dm644 \"$srcdir/{srcdir_prefix}{}\" \
                 \"$pkgdir{prefix}/share/applications/{base}\"\n",
                d.display()
            ));
        }
        for ic in &layout.icons {
            let base = file_name_of(&ic.rel);
            install_rest.push_str(&format!(
                "  install -Dm644 \"$srcdir/{srcdir_prefix}{}\" \
                 \"$pkgdir{prefix}/share/icons/hicolor/{}/apps/{base}\"\n",
                ic.rel.display(),
                ic.size_dir
            ));
        }
    } else {
        // 生成 .desktop：与既有 pkgbuild 同款（Exec 用绝对安装路径）
        let icon = crate::desktop::resolve_icon_name(opts.icon_name, bin);
        let (name, content) = crate::desktop::resolve_desktop_entry(
            &identity,
            bin,
            &icon,
            &format!("{prefix}/bin/{bin}"),
            opts.project_dir,
            &meta_ref,
            opts.desktop_template,
            &cargo_desc,
        )
        .context("生成 .desktop 失败")?;
        install_rest.push_str(&format!(
            "  install -Dm644 \"$startdir/{name}\" \
             \"$pkgdir{prefix}/share/applications/{name}\"\n"
        ));
        // 图标：优先装 tarball 里的（改名成生成 .desktop 引用的名字），否则用 meta 的
        if let Some(ic) = layout.icons.first() {
            install_rest.push_str(&format!(
                "  install -Dm644 \"$srcdir/{srcdir_prefix}{}\" \
                 \"$pkgdir{prefix}/share/icons/hicolor/{}/apps/{icon}.{}\"\n",
                ic.rel.display(),
                ic.size_dir,
                ic.ext
            ));
        } else if let Some(meta_icons) = &identity.icon {
            for rel in meta_icons.iter() {
                let ext = rel.rsplit('.').next().unwrap_or("png");
                let size = crate::desktop::icon_size_dir(rel);
                install_rest.push_str(&format!(
                    "  install -Dm644 \"$startdir/{rel}\" \
                     \"$pkgdir{prefix}/share/icons/hicolor/{size}/apps/{icon}.{ext}\"\n"
                ));
            }
        } else {
            println!("提示：tarball 与 bundle 元数据里都没有图标，将不安装应用图标。");
        }
        generated_desktop = Some((name, content));
    }

    let url_line = match url {
        Some(u) if !u.is_empty() => format!("url=\"{}\"\n", bash_escape(&u)),
        _ => String::new(),
    };
    let license_line = match license {
        Some(l) if !l.is_empty() => format!("license=({})\n", bash_escape(l)),
        _ => String::new(),
    };
    let depends_line = if depends.is_empty() {
        String::new()
    } else {
        format!("depends=({})\n", depends.join(" "))
    };

    let pkgbuild = format!(
        r#"# 由 aa-bundle 生成（AUR -bin 形态：重打包上游预编译 tarball，包内不构建）。
# 手写修改会在下次生成时被覆盖。
# 配置来源：{meta_ref}
# 上游源：{source_display}
# 上游替换该 asset 后 sha256 会失配：重新运行 aa-bundle --source 生成本文件即可。
# 本地 file:// 源由 makepkg 经 curl 拷贝，makepkg 运行前不要移动该文件。
pkgname={pkgname}
pkgver={pkgver}
pkgrel=1
pkgdesc="{pkgdesc}"
arch=({arch})
{url_line}{license_line}{depends_line}source=("{source_entry}")
sha256sums=('{sha256}')

package() {{
{install_bin}{install_rest}}}
"#,
        arch = arch.join(" "),
        pkgdesc = bash_escape(&pkgdesc),
        source_display = input.display(),
        source_entry = bash_escape(&resolved.source_entry),
        sha256 = resolved.sha256,
    );

    Ok((pkgbuild, generated_desktop))
}

fn opts_version(identity: &BundleMeta) -> Option<String> {
    identity.version.clone()
}

fn file_name_of(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `pkgname`：规范化名 + `-bin` 后缀（AUR 里预编译包的惯例后缀）。
///
/// 不用 `identity::pkg_segment`——那个把 `-` 也换成 `_`（它是给 identifier 段用的），
/// 而 AUR 包名习惯保留连字符（`elyterminal-bin` 而非 `elyterminal_bin-bin`）。
fn pkgname_with_bin_suffix(bin: &str) -> String {
    let seg: String = bin
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    let seg = seg.trim_matches('_');
    if seg.is_empty() {
        "app-bin".to_string()
    } else if seg.ends_with("-bin") {
        seg.to_string()
    } else {
        format!("{seg}-bin")
    }
}

/// makepkg 的 pkgver 只允许 `[A-Za-z0-9._+]`——`-` 很常见（`1.0-rc`），主动报错
/// 并给改法，而不是生成一份 makepkg 会拒绝的 PKGBUILD。
fn validate_pkgver(v: &str) -> Result<()> {
    if v.is_empty() {
        bail!("pkgver 为空");
    }
    if v.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+'))
    {
        return Ok(());
    }
    bail!(
        "pkgver `{v}` 含非法字符：makepkg 只允许字母数字与 . _ +\
         （请把 `-` 换成 `.`，如 `1.0-rc` → `1.0.rc`）"
    )
}

/// bash 双引号上下文的转义：`\` `"` `$`（URL / 描述里都可能出现）。
fn bash_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn fixture_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("aa-bundle-upstream-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn make_targz(parent: &Path, member: &str, tag: &str) -> PathBuf {
        let out = parent
            .parent()
            .unwrap()
            .join(format!("up-{tag}-{}.tar.gz", std::process::id()));
        let st = std::process::Command::new("tar")
            .arg("-czf")
            .arg(&out)
            .arg("-C")
            .arg(parent)
            .arg(member)
            .status()
            .expect("需要系统 tar");
        assert!(st.success());
        out
    }

    fn write_elf(path: &Path) {
        std::fs::write(path, [0x7f, b'E', b'L', b'F', 0x02, 0x01, 0x01, 0x00, b'x']).unwrap();
    }

    /// 建一个带 .desktop + .svg 的 tarball（vendor 分支 fixture）
    fn vendor_tarball(base: &Path) -> PathBuf {
        let wrapper = base.join("App-0.1.8-linux-x86_64");
        std::fs::create_dir_all(&wrapper).unwrap();
        write_elf(&wrapper.join("app"));
        std::fs::write(wrapper.join("app.desktop"), "[Desktop Entry]").unwrap();
        std::fs::write(wrapper.join("app.svg"), "<svg/>").unwrap();
        make_targz(base, "App-0.1.8-linux-x86_64", "vendor")
    }

    /// 只带二进制（生成分支 fixture）
    fn bare_tarball(base: &Path) -> PathBuf {
        let wrapper = base.join("App-0.1.8-linux-x86_64");
        std::fs::create_dir_all(&wrapper).unwrap();
        write_elf(&wrapper.join("app"));
        make_targz(base, "App-0.1.8-linux-x86_64", "bare")
    }

    fn write_meta(dir: &Path, body: &str) {
        std::fs::write(dir.join("bundle.toml"), body).unwrap();
    }

    fn opts<'a>(dir: &'a Path, source: &'a str) -> BinPkgOptions<'a> {
        BinPkgOptions {
            project_dir: dir,
            bundle_meta: vec!["bundle.toml".to_string()],
            source,
            expected_sha256: None,
            bin_name: None,
            icon_name: None,
            desktop_template: None,
            depends: None,
            arch: None,
            url: None,
            license: None,
            prefix: None,
        }
    }

    #[test]
    fn bin_pkgbuild_vendor_desktop_branch_installs_from_srcdir() {
        let base = fixture_dir("vendor-branch");
        let gz = vendor_tarball(&base);
        write_meta(
            &base,
            "identifier = \"com.example.App\"\nname = \"App\"\nshort_description = \"A test app\"\n",
        );
        let src = gz.display().to_string();
        let o = opts(&base, &src);
        let (pkg, desktop) = generate_pkgbuild_bin(&o).unwrap();

        assert!(pkg.contains("pkgname=app-bin"));
        assert!(pkg.contains("pkgver=0.1.8"), "应从归档名推版本：\n{pkg}");
        assert!(pkg.contains("arch=(x86_64)"), "应从归档名推架构：\n{pkg}");
        assert!(
            pkg.contains("source=(\"file:///"),
            "本地源应转 file://：\n{pkg}"
        );
        assert!(pkg.contains("sha256sums=('"), "应带校验和：\n{pkg}");
        assert!(!pkg.contains("build()"), "-bin 不应有 build()：\n{pkg}");
        assert!(
            !pkg.contains("makedepends"),
            "-bin 不应有 makedepends：\n{pkg}"
        );
        // vendor 分支：从 $srcdir 装上游文件，不生成 .desktop
        assert!(
            pkg.contains("\"$srcdir/App-0.1.8-linux-x86_64/app\""),
            "\n{pkg}"
        );
        assert!(pkg.contains("share/applications/app.desktop"), "\n{pkg}");
        assert!(
            pkg.contains("share/icons/hicolor/scalable/apps/app.svg"),
            "\n{pkg}"
        );
        assert!(!pkg.contains("$startdir"));
        assert!(desktop.is_none(), "vendor 分支不应生成 .desktop");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn bin_pkgbuild_generated_desktop_branch_installs_from_startdir() {
        let base = fixture_dir("gen-branch");
        let gz = bare_tarball(&base);
        write_meta(
            &base,
            "identifier = \"com.example.App\"\nname = \"App\"\nshort_description = \"A test app\"\nlicense = \"MIT\"\n",
        );
        let src = gz.display().to_string();
        let o = opts(&base, &src);
        let (pkg, desktop) = generate_pkgbuild_bin(&o).unwrap();

        assert!(
            pkg.contains("$startdir/"),
            "生成分支应从 $startdir 装 desktop：\n{pkg}"
        );
        assert!(pkg.contains("share/applications/"), "\n{pkg}");
        assert!(pkg.contains("license=(MIT)"), "\n{pkg}");
        let (name, content) = desktop.expect("应生成 .desktop");
        assert!(name.ends_with(".desktop"));
        // 自动生成的 Exec 恒为裸 bin 名（靠 PATH，见 desktop.rs 的说明）
        assert!(content.contains("Exec=app"), "\n{content}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn bin_pkgbuild_meta_overrides_take_precedence() {
        let base = fixture_dir("meta-over");
        let gz = vendor_tarball(&base);
        write_meta(
            &base,
            "name = \"App\"\nversion = \"9.9.9\"\nshort_description = \"desc\"\nlicense = \"Apache-2.0\"\n\
             [arch]\nurl = \"https://example.org\"\ndepends = [\"gtk3\", \"glibc\"]\narch = [\"aarch64\"]\nprefix = \"/usr/local\"\n",
        );
        let src = gz.display().to_string();
        let o = opts(&base, &src);
        let (pkg, _) = generate_pkgbuild_bin(&o).unwrap();

        assert!(pkg.contains("pkgver=9.9.9"), "meta version 优先：\n{pkg}");
        assert!(pkg.contains("url=\"https://example.org\""));
        assert!(pkg.contains("license=(Apache-2.0)"));
        assert!(pkg.contains("depends=(gtk3 glibc)"));
        assert!(pkg.contains("arch=(aarch64)"));
        assert!(
            pkg.contains("pkgdir/usr/local/bin/app"),
            "prefix 生效：\n{pkg}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn bin_pkgbuild_invalid_pkgver_errors() {
        let base = fixture_dir("badver");
        let gz = vendor_tarball(&base);
        write_meta(
            &base,
            "name = \"App\"\nversion = \"1.0-rc\"\nshort_description = \"d\"\n",
        );
        let src = gz.display().to_string();
        let o = opts(&base, &src);
        let err = generate_pkgbuild_bin(&o).unwrap_err();
        assert!(err.to_string().contains("非法字符"), "应报非法字符：{err}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn bin_pkgbuild_escapes_quotes_and_dollar() {
        let base = fixture_dir("escape");
        let gz = vendor_tarball(&base);
        write_meta(
            &base,
            "name = \"App\"\nshort_description = \"a \\\"quoted\\\" $thing\"\n",
        );
        let src = gz.display().to_string();
        let o = opts(&base, &src);
        let (pkg, _) = generate_pkgbuild_bin(&o).unwrap();
        assert!(
            pkg.contains("a \\\"quoted\\\" \\$thing"),
            "应转义引号与 $：\n{pkg}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn bin_pkgbuild_expected_sha_mismatch_errors() {
        let base = fixture_dir("sha");
        let gz = vendor_tarball(&base);
        write_meta(&base, "name = \"App\"\nshort_description = \"d\"\n");
        let src = gz.display().to_string();
        let mut o = opts(&base, &src);
        o.expected_sha256 =
            Some("0000000000000000000000000000000000000000000000000000000000000000");
        let err = generate_pkgbuild_bin(&o).unwrap_err();
        assert!(err.to_string().contains("校验和不匹配"), "{err}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn pkgname_gets_bin_suffix_once() {
        assert_eq!(pkgname_with_bin_suffix("App"), "app-bin");
        assert_eq!(pkgname_with_bin_suffix("app-bin"), "app-bin");
        assert_eq!(pkgname_with_bin_suffix("my app"), "my_app-bin");
    }

    #[test]
    fn pkgver_validation_rules() {
        assert!(validate_pkgver("1.0").is_ok());
        assert!(validate_pkgver("1.0_rc1").is_ok());
        assert!(validate_pkgver("1.0+2").is_ok());
        assert!(validate_pkgver("1.0-rc").is_err());
        assert!(validate_pkgver("").is_err());
    }
}
