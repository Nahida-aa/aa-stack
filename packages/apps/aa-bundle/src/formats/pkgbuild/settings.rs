//! Arch 打包设置：`[bundle.arch]` 子表解析、bin 名推导、默认输出目录。

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use super::manifest::{PackageInfo, read_package_info};

/// Arch 打包参数的最终取值：`[bundle.arch]` 子表 > 命令行覆盖 > 默认值。
///
/// 与 cargo-bundle 的 deb_depends 一致：系统包依赖**不猜、不填默认值** ——
/// 只有项目自己知道（cargo 只认 -l，不认发行版包名），没声明就整行不写
/// depends=（见 cargo-bundle deb_bundle.rs:128-131）。
/// makedepends 默认 (cargo rust) 不是猜：我们的 PKGBUILD 自己调 cargo build。
#[derive(Debug, Clone)]
pub struct ArchSettings {
    pub depends: Vec<String>,
    pub makedepends: Vec<String>,
    pub arch: Vec<String>,
    pub url: String,
    pub prefix: String,
    /// 图标安装目录（相对 prefix），如 share/icons/hicolor/512x512/apps
    pub icon_dir: String,
}

impl Default for ArchSettings {
    fn default() -> Self {
        Self {
            depends: Vec::new(),
            makedepends: ["cargo", "rust"].iter().map(|s| s.to_string()).collect(),
            arch: vec!["x86_64".to_string()],
            url: String::new(),
            prefix: "/usr".to_string(),
            // 只到 hicolor 为止——尺寸目录由 icon 文件名（@2x）推导后拼上
            icon_dir: "share/icons/hicolor".to_string(),
        }
    }
}

/// 从 `[bundle.arch]` 子表取 Arch 设置（子表不存在 → 默认值）。
///
/// `prebuilt` 为真且子表未显式声明 `makedepends` 时，把默认的 `(cargo rust)`
/// 清空——二进制打包的 PKGBUILD 既不在包内 `cargo build`，也不调 cargo metadata
/// （路径生成期已钉死），构建依赖没有意义。显式声明仍然尊重（优先级见调用方）。
pub(super) fn arch_settings_from(
    arch: &Option<crate::meta::ArchTable>,
    prebuilt: bool,
) -> ArchSettings {
    let mut s = ArchSettings::default();
    if let Some(t) = arch {
        if let Some(v) = &t.depends {
            s.depends = v.clone();
        }
        if let Some(v) = &t.makedepends {
            s.makedepends = v.clone();
        }
        if let Some(v) = &t.arch {
            s.arch = v.clone();
        }
        if let Some(v) = &t.url {
            s.url = v.clone();
        }
        if let Some(v) = &t.prefix {
            s.prefix = v.clone();
        }
        if let Some(v) = &t.icon_dir {
            s.icon_dir = v.clone();
        }
    }
    if prebuilt && arch.as_ref().and_then(|t| t.makedepends.as_ref()).is_none() {
        s.makedepends = Vec::new();
    }
    s
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

/// pkgbuild 格式的默认输出目录：`<workspace target>/aa-bundle/pkgbuild/<bin>/`。
///
/// 不放在 crate 目录的 `target/` 下——workspace 成员的 target 由 workspace
/// 根决定（见 [`crate::cargo::resolve_target_dir`]），crate 目录下的 target/ cargo
/// 根本不用。末尾按 bin 名分目录：同一 workspace 打多个 crate 不会互相覆盖。
pub fn default_out_dir(project_dir: &Path, bin_name: Option<&str>) -> Result<PathBuf> {
    let info = read_package_info(project_dir)?;
    let bin = resolve_bin_name(&info, bin_name, project_dir)?;
    Ok(crate::cargo::resolve_target_dir(project_dir).join(format!("aa-bundle/pkgbuild/{bin}")))
}
