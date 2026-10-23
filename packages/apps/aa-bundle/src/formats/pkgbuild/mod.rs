//! Arch Linux 包（PKGBUILD）生成。
//!
//! 这是 `aa-bundle` 的第一个打包目标：把「构建 + 拷贝 + 注册 .desktop」这套
//! 手工流程（此前散在各项目的 justfile / script 里）收敛成生成一份 PKGBUILD，
//! 交给 `makepkg`/`pacman` 处理，从而：
//! - 卸载变成 `pacman -R`，不再需要手写 uninstall 脚本
//! - 身份（包名 / .desktop 名 / 图标）统一来自 `--bundle-meta` 指向的表
//!
//! ## 命名：为什么都叫 `pkgbuild`
//!
//! CLI 子命令 `aa-bundle pkgbuild` 读 Cargo.toml 的
//! `[package.metadata.bundle-pkgbuild]` —— 同名，省一层映射。
//! 早期两者都叫 `arch`，与 `--arch`（CPU 架构：x86_64）撞词：同一个词既指发行版
//! 又指架构。改成 `pkgbuild` 后两边都无歧义。
//!
//! 注意这个段是 **aa-bundle 自己的输入**，不是 Arch 的元数据 ——
//! Arch/pacman 只认 PKGBUILD 与 makepkg 生成的 `.PKGINFO`，从不读 Cargo.toml。
//! 它只是把「推不出来、又不该每次敲命令行」的设置集中声明。
//!
//! **生成的 PKGBUILD 是「本地开发安装」用途**：它直接在源码树里构建（`build()`
//! 里 cd 回源码目录），不带 `source=`，因此**不能用于分发**（AUR 那种需要真正的
//! source + checksum）。分发版是后续的事。

use std::path::Path;

use anyhow::{Context, Result};

use crate::meta::BundleMeta;

mod manifest;
mod settings;
#[cfg(test)]
mod tests;

pub use manifest::{PackageInfo, read_package_info};
pub use settings::{ArchSettings, default_out_dir, resolve_bin_name};
// generate_pkgbuild 与 tests 用；对外不公开（子表解析是内部细节）。
use settings::arch_settings_from;

/// PKGBUILD 生成参数。
///
/// 字段分两类：`project_dir` / `bundle_meta` / `bin_name` / `icon_name` /
/// `desktop_template` 是所有格式通用的（CLI 有对应 flag）；其余
/// （`depends` 及以下）是 Arch 特有设置的**库 API 覆盖项**——CLI 不暴露
/// 它们，格式特有设置一律声明在 bundle 元数据的 `[bundle.arch]` 子表里，
/// 免得 `--format tar.gz` 时面对一堆无效 flag。
pub struct PkgbuildOptions<'a> {
    /// 源码目录（含 Cargo.toml）
    pub project_dir: &'a Path,
    /// 身份/打包配置来源引用列表（**必填**，无默认值）：多个按顺序分层，
    /// 后者递归覆盖前者（见 [`crate::meta::load_bundle_meta_layered`]）。
    /// 典型用法：先给共享表，再给 channel 专属表。
    pub bundle_meta: Vec<String>,
    /// 覆盖二进制名
    pub bin_name: Option<&'a str>,
    /// 覆盖 .desktop 模板路径
    pub desktop_template: Option<&'a Path>,
    /// 覆盖图标基础名（不含尺寸目录与扩展）
    pub icon_name: Option<&'a str>,
    /// 覆盖运行依赖（完全替代 `[bundle.arch].depends`）
    pub depends: Option<&'a [String]>,
    /// 覆盖构建依赖
    pub makedepends: Option<&'a [String]>,
    /// 覆盖目标架构
    pub arch: Option<&'a [String]>,
    /// 覆盖 url
    pub url: Option<&'a str>,
    /// 覆盖 license
    pub license: Option<&'a str>,
    /// 覆盖安装前缀
    pub prefix: Option<&'a str>,
    /// 要打包的 cargo 构建 profile（默认 `release`）。用于安装**已编好**的产物：
    /// 如 `release-quick` → PKGBUILD 里 `cargo build --profile release-quick`，
    /// 且取 `target/release-quick/<bin>`。`dev` 映射到 cargo 的 `debug` 目录。
    pub profile: Option<&'a str>,
    /// 二进制打包模式（AUR `-bin` 形态）：`build()` 不在包内编译，只校验生成期
    /// 钉死的二进制路径；`package()` 直接 install 该路径。makedepends 默认清空
    /// （包内不调 cargo）。默认 `false` = 源码打包（`build()` 里 `cargo build`）。
    pub prebuilt: bool,
    /// 直接指定二进制路径（仅 prebuilt 模式有意义）：生成时 canonicalize 成绝对
    /// 路径、校验存在后钉死进 PKGBUILD。默认取 `target/<profile>/<bin>`。
    pub bin_path: Option<&'a Path>,
}

/// 生成 PKGBUILD 文本，并渲染 .desktop。
///
/// 返回 `(PKGBUILD 文本, Option<(desktop 文件名, desktop 内容)>)`。
pub fn generate_pkgbuild(opts: &PkgbuildOptions) -> Result<(String, Option<(String, String)>)> {
    let info = read_package_info(opts.project_dir)?;
    let identity: BundleMeta =
        crate::meta::load_bundle_meta_layered(&opts.bundle_meta, opts.project_dir)?;
    // 错误消息 / PKGBUILD 注释里展示的「配置来源」：多引用按顺序用 + 连起来
    let meta_ref = opts.bundle_meta.join(" + ");
    let mut settings = arch_settings_from(&identity.arch, opts.prebuilt);
    // 命令行覆盖优先于 Cargo.toml 的 [bundle.arch]
    if let Some(v) = opts.depends {
        settings.depends = v.to_vec();
    }
    if let Some(v) = opts.makedepends {
        settings.makedepends = v.to_vec();
    }
    if let Some(v) = opts.arch {
        settings.arch = v.to_vec();
    }
    if let Some(v) = opts.url {
        settings.url = v.to_string();
    }
    if let Some(v) = opts.prefix {
        settings.prefix = v.to_string();
    }

    // —— bin 名（对齐 cargo 的自动发现规则，见 resolve_bin_name）——
    let bin: &str = resolve_bin_name(&info, opts.bin_name, opts.project_dir)?;

    // 图标安装名：覆盖 > bin 名。无数据源可推导，回退时不静默（见 resolve_icon_name）。
    let icon = crate::desktop::resolve_icon_name(opts.icon_name, bin);
    let license = opts.license.unwrap_or(&info.license);
    let prefix = settings.prefix.trim_end_matches('/');
    // pkgver：bundle 元数据的 version > Cargo.toml 的 version
    let version = identity
        .version
        .clone()
        .unwrap_or_else(|| info.version.clone());
    // pkgdesc 回退链：short_description > Cargo.toml description > name > crate 名
    // （与 cargo-bundle 的 short_description() 同思路）
    let pkgdesc = identity
        .short_description
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            if !info.description.is_empty() {
                info.description.clone()
            } else {
                identity.name.clone().unwrap_or_else(|| info.name.clone())
            }
        });
    // 源码目录绝对化：PKGBUILD 会在自己的目录里执行，必须按绝对路径回到源码树
    let src = opts
        .project_dir
        .canonicalize()
        .with_context(|| format!("无法解析源码目录 {}", opts.project_dir.display()))?;

    // cargo 包名直接从 crate 的 Cargo.toml 取 —— 已经指了目录，不需要再让人传一遍
    let cargo_pkg_flag = format!(" -p {}", info.name);

    // 要打包的 profile：默认 release。`dev` 在 cargo 里落 target/debug，其余同名。
    // release 用简写 `--release`（保持与旧输出一致），其余走 `--profile <name>`。
    // （profile_dir / profile_flag 与 tar.gz 共用，见 lib.rs。）
    let profile = opts.profile.unwrap_or("release");
    let profile_dir = crate::cargo::profile_dir(profile);
    let profile_flag = crate::cargo::profile_flag(profile);

    // prebuilt 模式：生成期就把二进制定位并钉死成绝对路径。canonicalize 一体
    // 解决「相对→绝对」与存在性；`--bin-path` 覆盖优先于 target/<profile>/<bin>。
    // 在进程内校验（而不是留给 makepkg）是为了早失败：缺产物时立刻告诉用户
    // 先 cargo build，而不是等 makepkg 跑到 build() 才报错。
    let pinned_bin = if opts.prebuilt {
        let path = match opts.bin_path {
            Some(p) => p.canonicalize().with_context(|| {
                format!("--bin-path 指向的二进制不存在或无法解析：{}", p.display())
            })?,
            None => {
                let path = crate::cargo::resolve_binary_path(opts.project_dir, bin, profile);
                anyhow::ensure!(
                    path.exists(),
                    "找不到二进制 {}：先 `cargo build {} -p {}`（prebuilt 模式不构建，只安装已编好的产物）",
                    path.display(),
                    profile_flag,
                    info.name
                );
                path.canonicalize().unwrap_or(path)
            }
        };
        Some(path)
    } else {
        None
    };

    // .desktop（文件名 + 内容）：模板 > aa-bundle 自动生成，见 resolve_desktop_entry。
    // `$APP_CLI` 用绝对安装路径——PKGBUILD 装完二进制就在 <prefix>/bin/<bin>。
    // Comment 回退值取 Cargo.toml 的 [package].description。
    let desktop_rel = crate::desktop::resolve_desktop_entry(
        &identity,
        bin,
        &icon,
        &format!("{prefix}/bin/{bin}"),
        opts.project_dir,
        &meta_ref,
        opts.desktop_template,
        &info.description,
    )?;

    // 没给 url 就整行不输出，避免生成 `url=""` 这种空字段
    let url_line = match opts.url {
        Some(u) if !u.is_empty() => format!("url=\"{u}\"\n"),
        _ => String::new(),
    };
    let depends = settings.depends.join(" ");
    let makedepends = settings.makedepends.join(" ");
    let arch = settings.arch.join(" ");
    // 与 cargo-bundle 一致：空则不写该行（而不是写个空的 depends=()）
    let depends_line = if settings.depends.is_empty() {
        String::new()
    } else {
        format!("depends=({depends})\n")
    };
    let makedepends_line = if settings.makedepends.is_empty() {
        String::new()
    } else {
        format!("makedepends=({makedepends})\n")
    };

    let desktop_install = {
        let (name, _) = &desktop_rel;
        format!(
            "  install -Dm644 \"$startdir/{name}\" \"$pkgdir{prefix}/share/applications/{name}\"\n"
        )
    };

    // 图标安装行：路径来自 `[bundle].icon`（cargo-bundle 的字段），安装目录来自
    // `[bundle.arch].icon_dir`。**不再硬编码 resources/app-icon.png** —— 那正是
    // 「CLI 假设项目怎么命名」的老问题。没配 icon 就不装图标。
    let mut icon_install = String::new();
    let icon_dir_full = format!("{prefix}/{}", settings.icon_dir.trim_end_matches('/'));
    for icon_path in identity.icon.iter().flatten() {
        let abs = opts.project_dir.join(icon_path);
        anyhow::ensure!(
            abs.exists(),
            "bundle 元数据 {} 的 icon 指向的文件不存在：{}",
            meta_ref,
            abs.display()
        );
        // 尺寸目录按文件名约定判断：`@2x` 结尾 → 1024x1024，否则 512x512。
        // （cargo-bundle 的 icon 列表也是这个约定；不按文件大小猜——那不可靠。）
        let size_dir = crate::desktop::icon_size_dir(icon_path);
        icon_install.push_str(&format!(
            "  install -Dm644 \"$_srcdir/{icon_path}\" \"$pkgdir{icon_dir_full}/{size_dir}/apps/{icon}.png\"\n"
        ));
    }
    if icon_install.is_empty() {
        println!("提示：bundle 元数据里没有 icon，将不安装应用图标。");
    }

    // 模板按打包模式分叉的三段：头部注释+_srcdir、build() 主体、package() 的
    // 二进制安装段。其余（包元信息、图标、.desktop）两模式共用。
    // 分叉理由：prebuilt 的二进制路径在**生成期**钉死（pinned_bin），makepkg
    // 侧零子进程、不读 makepkg 注入的环境；源码模式维持「现场 cargo build +
    // 问 cargo metadata」的现状。
    let (src_note, build_body, bin_install) = match &pinned_bin {
        Some(pinned) => (
            format!(
                r#"# 二进制打包（prebuilt）：二进制不在包内编译。aa-bundle 生成时已把
# {pinned} 钉死为安装源；重新构建后必须重新运行 aa-bundle --prebuilt
# 生成本文件，否则装的是旧产物。图标 / .desktop 源文件仍在源码树里
# （bundle 元数据的 icon 字段），按 $_srcdir 引用。
_srcdir="{src}"
"#,
                pinned = pinned.display(),
                src = src.display(),
            ),
            format!(
                r#"  # prebuilt 模式：不编译。二进制由包外构建提供
  # （cargo build {profile_flag} -p {cargo_pkg}），此处只校验生成期
  # 钉死的路径仍在——cargo clean / 移动 target 后会失效，重新构建后
  # 需重新运行 aa-bundle --prebuilt。
  # 环境仍按源码模式卸掉：本函数虽不编译，保持环境一致以防将来加步骤。
  unset CFLAGS CXXFLAGS CPPFLAGS LDFLAGS DEBUG_RUSTFLAGS
  if [ ! -x "{pinned}" ]; then
    echo "aa-bundle(prebuilt): 找不到可执行文件 {pinned}" >&2
    echo "  请先在包外构建：cargo build {profile_flag} -p {cargo_pkg}" >&2
    return 1
  fi
"#,
                pinned = pinned.display(),
                profile_flag = profile_flag,
                cargo_pkg = info.name,
            ),
            format!(
                r#"  # prebuilt：直接安装生成期钉死的二进制。路径生成时已定，makepkg
  # 运行环境的 CARGO_TARGET_DIR 一概不影响。
  install -Dm755 "{pinned}" "$pkgdir{prefix}/bin/{bin}"
"#,
                pinned = pinned.display(),
                prefix = prefix,
                bin = bin,
            ),
        ),
        None => (
            format!(
                r#"# 源码树路径。本 PKGBUILD 不带 source=，直接在源码目录里构建以复用 target/ 的
# 增量缓存 —— 代价是不能用于 AUR/分发，只适合本机安装。图标源文件也在源码树里
# （bundle 元数据的 icon 字段），同样按 $_srcdir 引用。
_srcdir="{src}"
"#,
                src = src.display(),
            ),
            format!(
                r#"  cd "$_srcdir"
  # 卸掉 makepkg 从 /etc/makepkg.conf 注入的编译器环境变量。rustc 不读它们，
  # 但 cc-rs 会读，且**追加在命令行末尾**——覆盖 crate 为个别文件设的 -O0。
  # 实例：aws-lc-sys 的 jitterentropy-base.c 禁用优化（源码 #error 强制），
  # 被 CFLAGS 的 -O2 覆盖后必挂。用户 shell 里这些变量本来就不存在，卸掉
  # 正好让包内构建与直接 `cargo build` 一致。
  # DEBUG_RUSTFLAGS 同族：OPTIONS=(debug) 时 makepkg 的 buildenv/rust.sh
  # 会往 RUSTFLAGS 追加 `-C debuginfo=2`（不卸就把调试符号编进产物，
  # rustc 内存与二进制体积双双暴涨）。
  unset CFLAGS CXXFLAGS CPPFLAGS LDFLAGS DEBUG_RUSTFLAGS
  cargo build {profile_flag}{cargo_pkg_flag}
"#,
                profile_flag = profile_flag,
                cargo_pkg_flag = cargo_pkg_flag,
            ),
            // 二进制路径不能硬编码「crate 目录下的 target/release」——cargo 的
            // target_directory 由 **workspace 根**决定（多 crate workspace 下产物不在
            // crate 目录），且 CARGO_TARGET_DIR 可改。直接问 cargo；--no-deps 只读
            // manifest，不解析依赖、不联网。也顺带尊重 makepkg 运行环境的
            // CARGO_TARGET_DIR。
            format!(
                r#"  _target_dir=$(cd "$_srcdir" && cargo metadata --no-deps --format-version 1 \
    | sed -n 's/.*"target_directory": *"\([^"]*\)".*/\1/p')
  if [ -z "$_target_dir" ]; then
    echo "aa-bundle: 无法从 cargo metadata 解析 target_directory" >&2
    return 1
  fi
  install -Dm755 "$_target_dir/{profile_dir}/{bin}" "$pkgdir{prefix}/bin/{bin}"
"#,
                profile_dir = profile_dir,
                bin = bin,
                prefix = prefix,
            ),
        ),
    };

    let pkgbuild = format!(
        r#"# 由 aa-bundle 生成（本地开发安装用途，非分发版）—— 手写修改会在下次生成时被覆盖。
# 配置来源：{bundle_meta}
pkgname={name}
pkgver={version}
pkgrel=1
pkgdesc="{pkgdesc}"
arch=({arch})
{url_line}license=({license})
{depends_line}{makedepends_line}
# 二进制体积大（strip 后仍 300MB+），开 LTO 链接会非常慢；与项目构建配置一致关掉。
options=(!lto)
{src_note}
build() {{
{build_body}}}

package() {{
{bin_install}
  # 图标路径来自 bundle 元数据的 icon 字段，安装目录来自 [bundle.arch].icon_dir
{icon_install}{desktop_install}}}
"#,
        name = info.name,
        version = version,
        pkgdesc = pkgdesc,
        arch = arch,
        license = license,
        depends_line = depends_line,
        makedepends_line = makedepends_line,
        src_note = src_note,
        build_body = build_body,
        bin_install = bin_install,
        icon_install = icon_install,
        bundle_meta = meta_ref,
        desktop_install = desktop_install,
    );

    Ok((pkgbuild, Some(desktop_rel)))
}
