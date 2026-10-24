//! 运行环境探测：发行版、可用工具、权限。
//!
//! 只回答两个问题：**这台机器能不能走 Arch 原生包路线**（门禁集中在
//! [`ensure_arch_env`]），以及**用哪些命令**（pacman / AUR helper / makepkg）。
//!
//! 解析与探测分开：解析是纯函数（吃 `&str`），方便单测嵌 fixture；只有
//! `ensure_arch_env` 会真的读 `/etc/os-release` 与外部命令。

use std::path::PathBuf;

use anyhow::{Context as _, Result, bail};

/// `/etc/os-release` 里我们关心的两个字段。
///
/// `ID_LIKE` 是关键：Garuda / Manjaro 派生版的 `ID` 不是 `arch`，但
/// `ID_LIKE=arch` 说明它们用同一套工具链（pacman / makepkg / AUR）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OsRelease {
    pub id: String,
    pub id_like: Vec<String>,
}

/// 解析 os-release 文本。
///
/// 值可能带引号（`ID="garuda"`）或裸写（`ID=garuda`），`#` 开头的行是注释。
/// 字段缺失时给空值——判定由 [`is_arch_family`] 负责。
pub fn parse_os_release(text: &str) -> OsRelease {
    let mut id = String::new();
    let mut id_like: Vec<String> = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, raw)) = line.split_once('=') else {
            continue;
        };
        let value = unquote(raw.trim());
        match key.trim() {
            "ID" => id = value.to_string(),
            "ID_LIKE" => id_like = value.split_whitespace().map(str::to_string).collect(),
            _ => {}
        }
    }
    OsRelease { id, id_like }
}

/// 去掉 shell 风格的成对引号（单引号或双引号）。
fn unquote(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        if (first == b'"' || first == b'\'') && bytes[bytes.len() - 1] == first {
            return &s[1..s.len() - 1];
        }
    }
    s
}

/// 是否属于 Arch 家族（能用 pacman / makepkg / AUR）。
pub fn is_arch_family(o: &OsRelease) -> bool {
    if o.id_like.iter().any(|v| v == "arch") {
        return true;
    }
    // 兜底：个别派生版不写 ID_LIKE（或写得不全），按 ID 认
    matches!(o.id.as_str(), "arch" | "garuda" | "manjaro" | "artix")
}

/// 是否以 root 身份运行（makepkg 在 EUID==0 时直接拒绝构建）。
pub fn running_as_root() -> bool {
    // SAFETY: geteuid 无参数、无副作用、永不出错
    let uid = unsafe { libc::geteuid() };
    uid == 0
}

/// AUR helper：v1 只认这两个，按此顺序优先。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AurHelper {
    Paru,
    Yay,
}

impl AurHelper {
    pub fn name(self) -> &'static str {
        match self {
            Self::Paru => "paru",
            Self::Yay => "yay",
        }
    }
}

/// 探测到的外部命令。缺哪个由 [`ensure_arch_env`] 决定要不要报错。
#[derive(Debug, Default)]
pub struct Tools {
    pub makepkg: Option<PathBuf>,
    pub pacman: Option<PathBuf>,
    pub aur: Option<AurHelper>,
}

/// 按 PATH 探测外部命令的存在性（不校验版本）。
pub fn detect_tools() -> Tools {
    Tools {
        makepkg: which::which("makepkg").ok(),
        pacman: which::which("pacman").ok(),
        // paru 优先：yay 是备选，两者都在时选前者
        aur: [AurHelper::Paru, AurHelper::Yay]
            .into_iter()
            .find(|h| which::which(h.name()).is_ok()),
    }
}

/// 门禁：确认本机走得了「打包 → makepkg → pacman」这条路。
///
/// 非 Arch 家族直接报错——v1 还没有 deb / rpm 生成器，与其静默走不通的路，
/// 不如早说清楚。
pub fn ensure_arch_env() -> Result<Tools> {
    let text = std::fs::read_to_string("/etc/os-release")
        .context("读取 /etc/os-release 失败（本工具 v1 只支持 Linux）")?;
    let os = parse_os_release(&text);
    if !is_arch_family(&os) {
        bail!(
            "v1 只支持 Arch 系（pacman / makepkg），本机是 `{}`\
             （ID_LIKE={}）：deb / rpm 生成器还没实现",
            os.id,
            os.id_like.join(" ")
        );
    }

    let tools = detect_tools();
    if tools.pacman.is_none() {
        bail!("找不到 pacman：v1 只支持 Arch 系");
    }
    if tools.makepkg.is_none() {
        bail!("找不到 makepkg：请先安装 base-devel（sudo pacman -S base-devel）");
    }
    if running_as_root() {
        bail!("请不要以 root 运行：makepkg 拒绝在 root 下构建，请用普通用户执行");
    }
    // makepkg 需要 base-devel 里的工具链，缺了会在构建中途才炸
    let has_base_devel = std::process::Command::new("pacman")
        .args(["-Q", "base-devel"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !has_base_devel {
        bail!("未安装 base-devel：请先执行 `sudo pacman -S base-devel`");
    }
    Ok(tools)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARCH: &str = r#"NAME="Arch Linux"
ID=arch
ID_LIKE=arch
VERSION_ID=rolling
"#;

    const GARUDA: &str = r#"NAME="Garuda Linux"
ID=garuda
ID_LIKE=arch
"#;

    const UBUNTU: &str = r#"NAME="Ubuntu"
ID=ubuntu
ID_LIKE=debian
"#;

    #[test]
    fn parse_arch() {
        let o = parse_os_release(ARCH);
        assert_eq!(o.id, "arch");
        assert_eq!(o.id_like, vec!["arch"]);
    }

    #[test]
    fn parse_garuda_splits_id_like() {
        let o = parse_os_release(GARUDA);
        assert_eq!(o.id, "garuda");
        assert_eq!(o.id_like, vec!["arch"]);
        assert!(is_arch_family(&o));
    }

    #[test]
    fn parse_quoted_values() {
        let o = parse_os_release("ID=\"garuda\"\nID_LIKE=\"arch archlinux\"\n");
        assert_eq!(o.id, "garuda");
        assert_eq!(o.id_like, vec!["arch", "archlinux"]);
        assert!(is_arch_family(&o));
    }

    #[test]
    fn parse_skips_comments_and_blank_lines() {
        let o = parse_os_release("# comment\n\nID=arch\nVERSION_ID=rolling\n");
        assert_eq!(o.id, "arch");
        assert!(o.id_like.is_empty());
    }

    #[test]
    fn missing_fields_are_empty() {
        let o = parse_os_release("VERSION_ID=rolling\n");
        assert_eq!(o, OsRelease::default());
        assert!(!is_arch_family(&o));
    }

    #[test]
    fn non_arch_is_rejected() {
        let o = parse_os_release(UBUNTU);
        assert!(!is_arch_family(&o));
    }

    #[test]
    fn arch_family_by_id_fallback() {
        for id in ["arch", "garuda", "manjaro", "artix"] {
            let o = OsRelease {
                id: id.to_string(),
                id_like: vec![],
            };
            assert!(is_arch_family(&o), "{id} 应算 Arch 家族");
        }
    }

    #[test]
    fn helper_names() {
        assert_eq!(AurHelper::Paru.name(), "paru");
        assert_eq!(AurHelper::Yay.name(), "yay");
    }
}
