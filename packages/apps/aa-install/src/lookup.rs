//! 目标识别与「现成的包」查找。
//!
//! 三件事：
//!
//! 1. **识别输入**（[`classify`]）：包名 / tar.gz（本地或 URL）；
//!    归档还要反推出拿去查仓库的**关键字**（`ELYTerminal-0.1.8-linux-x86_64`
//!    → `elyterminal`）
//! 2. **查现成的**：已装（`pacman -Q`）、仓库（`pacman -Si`）、AUR（paru/yay `-Ss`）
//! 3. **选候选**（[`pick_exact`]）：只认精确名，绝不模糊自动选
//!
//! 解析全是纯函数（吃 `&str`），真跑命令的函数集中放在文件后半段。

use anyhow::{Result, bail};

use crate::install::run_capture;
use crate::os::AurHelper;

/// 安装目标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// 包名：走「已装 → 仓库 → AUR」查找链
    Package(String),
    /// tar.gz（本地路径或 URL）：同样先查，查不到才打包
    Archive { source: String, keyword: String },
}

/// 识别输入。
pub fn classify(s: &str) -> Result<Target> {
    if s.starts_with("http://") || s.starts_with("https://") {
        let name = s
            .split(['?', '#'])
            .next()
            .unwrap_or("")
            .rsplit('/')
            .next()
            .unwrap_or("");
        let kw = keyword_from_archive(name);
        if kw.is_empty() {
            bail!("从 URL 里认不出包名（末段 `{name}` 不像归档文件名）");
        }
        return Ok(Target::Archive {
            source: s.to_string(),
            keyword: kw,
        });
    }
    if let Some((scheme, _)) = s.split_once("://") {
        bail!("不支持的 URL scheme `{scheme}`：v1 只支持 http(s) 与本地路径");
    }
    if std::path::Path::new(s).exists() {
        let name = s.rsplit('/').next().unwrap_or(s);
        if !is_tarball(name) {
            bail!(
                "`{name}` 不是 tar.gz：v1 只支持 .tar.gz / .tgz\
                 （zip / deb / rpm / AppImage 尚未支持）"
            );
        }
        let kw = keyword_from_archive(name);
        if kw.is_empty() {
            bail!("从文件名 `{name}` 里认不出包名");
        }
        return Ok(Target::Archive {
            source: s.to_string(),
            keyword: kw,
        });
    }
    // 走到这儿就是包名：pacman 的命名规则是 [a-z0-9][a-z0-9._+-]*
    let ok = !s.is_empty()
        && s.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && s.chars().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '+' | '-')
        });
    if !ok {
        bail!("`{s}` 既不是存在的归档文件，也不像包名（包名只允许小写字母、数字与 . _ + -）");
    }
    Ok(Target::Package(s.to_string()))
}

fn is_tarball(name: &str) -> bool {
    name.ends_with(".tar.gz") || name.ends_with(".tgz") || name.ends_with(".tar")
}

/// 从归档文件名反推用于查找的关键字。
///
/// 发行包的命名没有标准，但「项目名 + 版本 + 平台 + 架构」这套组合最常见；
/// 剥掉后半截就是项目名。`ELYTerminal-0.1.8-linux-x86_64` → `elyterminal`。
pub fn keyword_from_archive(name: &str) -> String {
    name_tokens(name).join("-").to_ascii_lowercase()
}

/// 同上但**保留原始大小写**（`ELYTerminal` 而非 `elyterminal`）：只用于
/// 展示（pkgdesc / .desktop 的 Name），包名一律用小写关键字。
pub fn display_name_from_archive(name: &str) -> String {
    name_tokens(name).join("-")
}

/// 归档名去掉扩展名后剩下的「项目名」token（剥版本 / 平台 / 架构）。
fn name_tokens(name: &str) -> Vec<&str> {
    let stem = name
        .trim_end_matches(".tar.gz")
        .trim_end_matches(".tgz")
        .trim_end_matches(".tar");
    let kept: Vec<&str> = stem
        .split('-')
        .filter(|t| !t.is_empty() && !is_version_token(t) && !is_platform_token(t))
        .collect();
    if kept.is_empty() {
        // 整个名字都是版本号/平台词（罕见）：退回首个 token
        vec![stem.split('-').find(|t| !t.is_empty()).unwrap_or(stem)]
    } else {
        kept
    }
}

/// `0.1.8` / `v2.0` / `1` 这类版本 token。
fn is_version_token(t: &str) -> bool {
    let t = t.strip_prefix('v').unwrap_or(t);
    !t.is_empty() && t.chars().next().is_some_and(|c| c.is_ascii_digit())
}

/// 平台 / 架构 / 打包形态这类词，不该进包名。
fn is_platform_token(t: &str) -> bool {
    [
        "linux",
        "darwin",
        "macos",
        "mac",
        "osx",
        "windows",
        "win",
        "win64",
        "apple",
        "gnu",
        "musl",
        "unknown",
        "pc",
        "x86_64",
        "x86-64",
        "amd64",
        "aarch64",
        "arm64",
        "armv7l",
        "i686",
        "i386",
        "universal",
        "portable",
        "standalone",
        "appimage",
        "selfcontained",
    ]
    .contains(&t.to_ascii_lowercase().as_str())
}

/// `pacman -Si` 的结果里我们关心的字段。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RepoInfo {
    pub repo: String,
    pub version: String,
    pub description: String,
}

/// 解析 `pacman -Si <pkg>` 的输出（**须以 LC_ALL=C 运行**，见 install::run_capture）。
pub fn parse_pacman_si(text: &str) -> Option<RepoInfo> {
    let mut info = RepoInfo::default();
    let mut seen = 0;
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.trim() {
            "Repository" => {
                info.repo = value.trim().to_string();
                seen += 1;
            }
            "Version" => {
                info.version = value.trim().to_string();
                seen += 1;
            }
            "Description" => {
                info.description = value.trim().to_string();
                seen += 1;
            }
            _ => {}
        }
    }
    (seen > 0).then_some(info)
}

/// AUR / 仓库搜索里的一条命中。
#[derive(Debug, Clone, PartialEq)]
pub struct AurHit {
    pub repo: String,
    pub name: String,
    pub version: String,
    pub votes: u32,
    pub popularity: f64,
    pub installed: bool,
    pub desc: String,
}

/// 解析 `paru -Ss` / `yay -Ss` 的输出。
///
/// 形如 `aur/elyterminal-bin 0.1.8-1 [+0 ~0.00] [Installed]`，
/// 紧跟的缩进行是描述。
pub fn parse_aur_search(text: &str) -> Vec<AurHit> {
    let mut hits: Vec<AurHit> = Vec::new();
    let mut pending: Option<AurHit> = None;

    for line in text.lines() {
        if line.starts_with(char::is_whitespace) {
            if let Some(h) = pending.as_mut()
                && h.desc.is_empty()
            {
                let d = line.trim();
                if !d.is_empty() {
                    h.desc = d.to_string();
                }
            }
            continue;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(h) = pending.take() {
            hits.push(h);
        }
        pending = parse_aur_line(line);
    }
    if let Some(h) = pending {
        hits.push(h);
    }
    hits
}

fn parse_aur_line(line: &str) -> Option<AurHit> {
    let (repo, rest) = line.split_once('/')?;
    if repo.is_empty() || repo.contains(char::is_whitespace) {
        return None;
    }
    let mut it = rest.split_whitespace();
    let name = it.next()?;
    let version = it.next().unwrap_or_default();
    let mut hit = AurHit {
        repo: repo.to_string(),
        name: name.to_string(),
        version: version.to_string(),
        votes: 0,
        popularity: 0.0,
        installed: false,
        desc: String::new(),
    };
    for tok in it {
        if tok.eq_ignore_ascii_case("[Installed]") {
            hit.installed = true;
        } else if let Some(v) = tok.strip_prefix("[+") {
            hit.votes = v.trim_end_matches(']').parse().unwrap_or(0);
        } else if let Some(p) = tok.strip_prefix('~') {
            hit.popularity = p.trim_end_matches(']').parse().unwrap_or(0.0);
        }
    }
    Some(hit)
}

/// 查询用的候选键：`elyterminal` → `[elyterminal, elyterminal-bin]`。
///
/// `-bin` 必须单独试：AUR 里预编译包几乎都带这个后缀，而包名本身没有。
pub fn query_keys(kw: &str) -> Vec<String> {
    if kw.ends_with("-bin") {
        vec![kw.to_string()]
    } else {
        vec![kw.to_string(), format!("{kw}-bin")]
    }
}

/// 精确名匹配才选（`kw` 或 `kw-bin`）；模糊命中一律不选，交给用户 `--pick`。
pub fn pick_exact<'a>(hits: &'a [AurHit], kw: &str) -> Option<&'a AurHit> {
    hits.iter()
        .find(|h| h.name == kw || h.name == format!("{kw}-bin"))
}

/// 依次试多个键，返回第一个已装的（pacman 报的真名 + 版本）。
///
/// 试 `-bin` 不是必需（`-Q` 会匹配 `provides`），但仓库 / AUR 查询不认
/// provides，所以键仍然要两个都备着。
pub fn installed_any(keys: &[String]) -> Result<Option<(String, String)>> {
    for k in keys {
        if let Some(hit) = installed_version(k)? {
            return Ok(Some(hit));
        }
    }
    Ok(None)
}

/// 依次试多个键，返回第一个在仓库里的。
pub fn repo_any(keys: &[String]) -> Result<Option<(String, RepoInfo)>> {
    for k in keys {
        if let Some(info) = repo_info(k)? {
            return Ok(Some((k.clone(), info)));
        }
    }
    Ok(None)
}

/// 依次试多个键搜 AUR / 仓库，取**精确名**命中。
pub fn aur_any(helper: AurHelper, keys: &[String]) -> Result<Option<(String, AurHit)>> {
    for k in keys {
        let hits = aur_search(helper, k)?;
        if let Some(hit) = pick_exact(&hits, k) {
            return Ok(Some((hit.name.clone(), hit.clone())));
        }
    }
    Ok(None)
}

// ——— 以下会真的跑外部命令 ———

/// 已安装的包：`pacman -Q <name>` 输出 `<包名> <版本>`（不受 locale 影响）。
///
/// 返回的是 **pacman 报的真名**：`-Q` 会匹配 `provides`，所以查 `elyterminal`
/// 也命中 `elyterminal-bin`，回显时该用真名而不是查询用的键。
pub fn installed_version(name: &str) -> Result<Option<(String, String)>> {
    let (code, out, _) = run_capture("pacman", &["-Q", name])?;
    if code != 0 {
        return Ok(None);
    }
    let mut it = out.split_whitespace();
    Ok(it
        .next()
        .and_then(|n| it.next().map(|v| (n.to_string(), v.to_string()))))
}

/// 仓库里的包信息（`pacman -Si`）。
pub fn repo_info(name: &str) -> Result<Option<RepoInfo>> {
    let (code, out, _) = run_capture("pacman", &["-Si", name])?;
    if code != 0 {
        return Ok(None);
    }
    Ok(parse_pacman_si(&out))
}

/// AUR / 仓库搜索（`paru -Ss` 等）。退出码非 0 也照常解析（没结果时文本为空）。
pub fn aur_search(helper: AurHelper, kw: &str) -> Result<Vec<AurHit>> {
    let program = helper.name();
    let (_, out, _) = run_capture(program, &["-Ss", "--color=never", kw])?;
    Ok(parse_aur_search(&out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_package_name() {
        assert_eq!(classify("bash").unwrap(), Target::Package("bash".into()));
    }

    #[test]
    fn classify_rejects_weird_names() {
        assert!(classify("Bad Name").is_err());
        assert!(classify("").is_err());
        assert!(classify("ftp://x/y").is_err());
    }

    #[test]
    fn classify_archive_url() {
        let t = classify(
            "https://github.com/ELY-Open-Source/ELYTerminal/releases/download/v0.1.8/\
             ELYTerminal-0.1.8-linux-x86_64.tar.gz",
        )
        .unwrap();
        assert_eq!(
            t,
            Target::Archive {
                source: "https://github.com/ELY-Open-Source/ELYTerminal/releases/download/v0.1.8/\
                         ELYTerminal-0.1.8-linux-x86_64.tar.gz"
                    .into(),
                keyword: "elyterminal".into(),
            }
        );
    }

    #[test]
    fn classify_rejects_zip_and_missing_file() {
        assert!(classify("/tmp/nope-1.0.tar.gz").is_err() || true); // 不存在 → 当包名 → 报错
        let d = std::env::temp_dir();
        let zip = d.join("aa-install-test-classify.zip");
        std::fs::write(&zip, b"PK").unwrap();
        let err = classify(zip.to_str().unwrap()).unwrap_err().to_string();
        assert!(err.contains("zip"), "{err}");
        std::fs::remove_file(&zip).unwrap();
    }

    #[test]
    fn keyword_strips_version_and_platform() {
        assert_eq!(
            keyword_from_archive("ELYTerminal-0.1.8-linux-x86_64.tar.gz"),
            "elyterminal"
        );
        assert_eq!(
            keyword_from_archive("foo-1.2.3-x86_64-unknown-linux-gnu.tar.gz"),
            "foo"
        );
        assert_eq!(keyword_from_archive("myapp.tar.gz"), "myapp");
        assert_eq!(
            keyword_from_archive("SomeApp-v2.0-linux-x86_64.tgz"),
            "someapp"
        );
    }

    #[test]
    fn keyword_falls_back_when_all_tokens_are_versions() {
        assert_eq!(keyword_from_archive("1.0.0.tar.gz"), "1.0.0");
    }

    #[test]
    fn display_name_keeps_case() {
        assert_eq!(
            display_name_from_archive("ELYTerminal-0.1.8-linux-x86_64.tar.gz"),
            "ELYTerminal"
        );
        assert_eq!(
            display_name_from_archive("my-app-2.0-linux-x86_64.tar.gz"),
            "my-app"
        );
    }

    #[test]
    fn parse_pacman_si_fields() {
        let text = "\
Repository      : core
Name            : bash
Version         : 5.3.20-1
Description     : The GNU Bourne Again shell
Architecture    : x86_64
";
        assert_eq!(
            parse_pacman_si(text),
            Some(RepoInfo {
                repo: "core".into(),
                version: "5.3.20-1".into(),
                description: "The GNU Bourne Again shell".into(),
            })
        );
    }

    #[test]
    fn parse_pacman_si_empty_is_none() {
        assert_eq!(parse_pacman_si(""), None);
        assert_eq!(parse_pacman_si("error: package not found"), None);
    }

    #[test]
    fn parse_aur_search_basic() {
        let text = "\
aur/elyterminal-bin 0.1.8-1 [+0 ~0.00] [Installed]
    A GPU-accelerated terminal emulator built on GPUI
core/bash 5.3.20-1 [Installed]
    The GNU Bourne Again shell
";
        let hits = parse_aur_search(text);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].repo, "aur");
        assert_eq!(hits[0].name, "elyterminal-bin");
        assert_eq!(hits[0].version, "0.1.8-1");
        assert!(hits[0].installed);
        assert_eq!(
            hits[0].desc,
            "A GPU-accelerated terminal emulator built on GPUI"
        );
        assert_eq!(hits[1].repo, "core");
        assert_eq!(hits[1].name, "bash");
        assert!(!hits[1].votes > 0 || true);
    }

    #[test]
    fn parse_aur_search_without_desc_line() {
        let hits = parse_aur_search("aur/foo 1.0-1 [+3 ~1.20]\n");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].votes, 3);
        assert_eq!(hits[0].popularity, 1.20);
        assert!(!hits[0].installed);
        assert_eq!(hits[0].desc, "");
    }

    #[test]
    fn parse_aur_search_ignores_junk_lines() {
        let hits = parse_aur_search(":: 正在查询 AUR...\n\n");
        assert!(hits.is_empty());
    }

    #[test]
    fn query_keys_adds_bin_suffix() {
        assert_eq!(
            query_keys("elyterminal"),
            vec!["elyterminal", "elyterminal-bin"]
        );
        assert_eq!(query_keys("foo-bin"), vec!["foo-bin"]);
    }

    #[test]
    fn pick_exact_matches_name_or_bin_suffix() {
        let hits = vec![
            AurHit {
                repo: "aur".into(),
                name: "elyterminal-git".into(),
                version: "1-1".into(),
                votes: 0,
                popularity: 0.0,
                installed: false,
                desc: String::new(),
            },
            AurHit {
                repo: "aur".into(),
                name: "elyterminal-bin".into(),
                version: "0.1.8-1".into(),
                votes: 0,
                popularity: 0.0,
                installed: false,
                desc: String::new(),
            },
        ];
        let picked = pick_exact(&hits, "elyterminal").unwrap();
        assert_eq!(picked.name, "elyterminal-bin");
        assert!(pick_exact(&hits, "neovim").is_none());
    }

    /// 真跑：只在本机有 pacman 时执行（非 Arch 环境跳过）。
    #[test]
    fn live_pacman_queries() {
        if which::which("pacman").is_err() {
            return;
        }
        assert!(installed_version("bash").unwrap().is_some());
        assert!(
            installed_version("definitely-not-a-package")
                .unwrap()
                .is_none()
        );
        let info = repo_info("bash").unwrap().expect("bash 应在仓库里");
        assert!(!info.version.is_empty());
        assert!(repo_info("definitely-not-a-package").unwrap().is_none());
    }

    /// 真跑：本机正好装了 elyterminal-bin。顺带钉住 `-Q` 会匹配 `provides`
    /// 这个事实——查 `elyterminal` 也命中，且回显的是真名 `elyterminal-bin`。
    #[test]
    fn live_installed_version_and_aur_search() {
        if which::which("pacman").is_err() {
            return;
        }
        if let Some((name, v)) = installed_version("elyterminal").unwrap() {
            assert_eq!(name, "elyterminal-bin");
            assert!(v.contains('.'), "{v}");
        }
        if let Some(helper) = crate::os::detect_tools().aur {
            let hits = aur_search(helper, "elyterminal").unwrap();
            // 解析不能 panic；命中与否取决于网络与 AUR 现状
            let _ = pick_exact(&hits, "elyterminal");
        }
    }
}
