//! 上游源（tar.gz）获取与归档名推断：跨打包格式共用。
//!
//! 输入两种：**本地路径**或 **http(s) / file:// URL**。URL 流式下载到临时目录
//! （不全量进内存），本地直读；两种都算 sha256（PKGBUILD 的 `sha256sums=()` 要它）。
//!
//! ## 为什么 sha256 在这里算
//!
//! 正统 AUR `-bin` 包的 PKGBUILD 是 `source=("url")` + `sha256sums=()`：
//! makepkg 自己下载并校验。所以本工具只需**获取一次**把校验和算出来嵌进去，
//! 生成物不再依赖本地那份下载（临时目录随即释放）。
//!
//! ## 只支持 tar.gz
//!
//! v1 只处理 gzip 压缩的 tar（按文件魔数判断，不看扩展名）。zip 明确报错
//! 而不是静默当 tar 处理——GitHub releases 里 zip 很常见，静默失败更糟。
//! tar.gz → tar.gz 重打包、目录源、裸二进制源均为规划中。

use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use ureq::config::Config;

/// gzip 魔数（`\x1f\x8b`）——v1 只接受 gzip 压缩的 tar。
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];
/// zip 魔数（`PK\x03\x04`）——识别后明确报「暂未支持」。
const ZIP_MAGIC: [u8; 4] = [b'P', b'K', 0x03, 0x04];

/// 用户给的源：本地路径或 URL。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceInput {
    /// 本地 tar.gz 路径（`file://` 输入剥壳后的结果）
    Local(PathBuf),
    /// `http(s)://` URL
    Url(String),
}

impl SourceInput {
    /// 启发式分类：
    /// - `http://` / `https://` → [`Self::Url`]
    /// - `file://` → 剥壳后 [`Self::Local`]
    /// - 其他含 `://` 的 → 报错（仅支持 http(s) 与 file）
    /// - 其余 → [`Self::Local`]（原样）
    pub fn parse(s: &str) -> Result<Self> {
        if let Some(rest) = s
            .strip_prefix("http://")
            .or_else(|| s.strip_prefix("https://"))
        {
            if rest.is_empty() {
                bail!("URL 缺少主机名：{s}");
            }
            return Ok(Self::Url(s.to_string()));
        }
        if let Some(path) = s.strip_prefix("file://") {
            if path.is_empty() {
                bail!("file:// 后面缺少路径：{s}");
            }
            return Ok(Self::Local(PathBuf::from(path)));
        }
        // 含 `://` 但既不是 http(s) 也不是 file 的，一律拒绝（不猜 scheme）
        if let Some((scheme, _)) = s.split_once("://") {
            bail!("不支持的 URL scheme `{scheme}`：本工具只支持 http/https 与 file");
        }
        Ok(Self::Local(PathBuf::from(s)))
    }

    /// 错误消息 / PKGBUILD 注释里的展示串。
    pub fn display(&self) -> String {
        match self {
            Self::Local(p) => p.display().to_string(),
            Self::Url(u) => u.clone(),
        }
    }

    /// 归档 basename，如 `ELYTerminal-0.1.8-linux-x86_64.tar.gz`。
    ///
    /// URL 取路径末段（剥 `?query` 与 `#fragment`）；本地取 `file_name`。
    /// 取不到时回退一个固定名字（`infer_version` / `infer_arch` 也只是拿不到
    /// 推断值，不影响主流程）。
    pub fn archive_name(&self) -> String {
        let raw: String = match self {
            Self::Local(p) => p
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            Self::Url(u) => u
                .split(['?', '#'])
                .next()
                .unwrap_or("")
                .rsplit('/')
                .next()
                .unwrap_or("")
                .to_string(),
        };
        let name = raw.trim().trim_end_matches('/');
        if name.is_empty() {
            "source.tar.gz".to_string()
        } else {
            name.to_string()
        }
    }
}

/// 已获取的源：本地 tar.gz 路径 + sha256 + PKGBUILD `source=()` 的值。
///
/// **设计约束**：URL 输入时归档落在 `_temp` 内，`_temp` 在 drop 时清理。
/// 因此生成物（PKGBUILD）里只能写 URL / `file://` 原路径 / `$srcdir` 相对路径，
/// **绝不写临时目录的绝对路径**——否则临时目录一删，生成物就成了死引用。
pub struct ResolvedSource {
    pub input: SourceInput,
    /// tar.gz 的本地路径（URL 输入时在临时目录内）
    pub archive: PathBuf,
    /// 归档的 sha256（小写 hex）
    pub sha256: String,
    /// PKGBUILD `source=()` 的条目：URL 输入 = URL 原文；本地输入 = `file://<绝对路径>`
    pub source_entry: String,
    /// 保活：URL 输入的下载目录；本地输入为 `None`
    _temp: Option<TempDir>,
}

// TempDir 不保证 Debug，手写一份：测试里 `unwrap_err()` 需要 Ok 侧实现 Debug。
impl std::fmt::Debug for ResolvedSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedSource")
            .field("input", &self.input)
            .field("archive", &self.archive)
            .field("sha256", &self.sha256)
            .field("source_entry", &self.source_entry)
            .finish_non_exhaustive()
    }
}

/// 取源并算 sha256。
///
/// - [`SourceInput::Url`]：ureq 流式下载进 TempDir，边下边哈希（归档可能几百 MB，
///   不全量进内存）；非 2xx 与网络错误都带 URL 上下文报错。
/// - [`SourceInput::Local`]：直接哈希原文件（不拷贝）。
/// - `expected` 给了就校验，不匹配报「校验和不匹配」并列出期望/实际两个值。
pub fn resolve_source(input: &SourceInput, expected: Option<&str>) -> Result<ResolvedSource> {
    let (archive, source_entry, temp) = match input {
        SourceInput::Local(path) => {
            let abs = path
                .canonicalize()
                .with_context(|| format!("本地源不存在或无法解析：{}", path.display()))?;
            if !abs.is_file() {
                bail!("本地源不是文件（目录源规划中）：{}", abs.display());
            }
            check_archive_kind(&abs)?;
            (
                abs.clone(),
                format!("file://{}", abs.display()),
                None::<TempDir>,
            )
        }
        SourceInput::Url(url) => {
            let dir = TempDir::new().context("创建下载临时目录失败")?;
            let dest = dir.path().join(sanitized_file_name(&input.archive_name()));
            download(url, &dest)?;
            check_archive_kind(&dest)?;
            (dest, url.clone(), Some(dir))
        }
    };

    let sha256 = sha256_hex(&archive)?;
    if let Some(want) = expected {
        let want = want.trim().to_ascii_lowercase();
        if want != sha256 {
            bail!(
                "校验和不匹配：期望 {want}，实际 {sha256}\
                 （下载被截断，或 URL 指向的文件已被替换）"
            );
        }
    }

    Ok(ResolvedSource {
        input: input.clone(),
        archive,
        sha256,
        source_entry,
        _temp: temp,
    })
}

/// 文件 sha256（小写 hex）。分块读，不整文件进内存。
pub fn sha256_hex(path: &Path) -> Result<String> {
    let file =
        std::fs::File::open(path).with_context(|| format!("读取 {} 失败", path.display()))?;
    let mut hasher = Sha256::new();
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader
            .read(&mut buf)
            .with_context(|| format!("读取 {} 失败", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// 归档类型检查：只接受 gzip 压缩的 tar；zip 与裸文件明确报错。
///
/// 按**魔数**判断而不是扩展名——扩展名可骗人，魔数不会。
fn check_archive_kind(path: &Path) -> Result<()> {
    let mut head = [0u8; 4];
    let n = {
        let mut f =
            std::fs::File::open(path).with_context(|| format!("读取 {} 失败", path.display()))?;
        use std::io::Read as _;
        f.read(&mut head)
            .with_context(|| format!("读取 {} 失败", path.display()))?
    };
    anyhow::ensure!(
        n >= 2 && head[..2] == GZIP_MAGIC,
        "只支持 gzip 压缩的 tar（.tar.gz / .tgz）：{} 不是 gzip 文件\
         （zip / 目录源 / 裸二进制为规划中）",
        path.display()
    );
    anyhow::ensure!(
        !head.starts_with(&ZIP_MAGIC),
        "检测到 zip 归档：zip 源暂未支持（v1 仅 tar.gz；规划中）：{}",
        path.display()
    );
    Ok(())
}

/// 下载 URL 到 `dest`，非 2xx 与网络错误都报出 URL 与状态码。
fn download(url: &str, dest: &Path) -> Result<()> {
    let config = Config::builder()
        .user_agent(concat!("aa-bundle/", env!("CARGO_PKG_VERSION")))
        // 三个超时各管一段：
        // - resolve / connect：主机同时有 A/AAAA 时，不通的那条（常见是 IPv6）
        //   会挂住，各自给超时才能快速退到能通的那条。
        // - global：兜底。曾经设 5 分钟，结果 20MB 的 release asset 在慢网下
        //   必然超时；放宽到 30 分钟。注意 ureq 3 没有 per-read 超时，同步 I/O
        //   也无法在单线程里中断阻塞读，所以**传输中途彻底卡死只能靠它兜**。
        .timeout_global(Some(Duration::from_secs(30 * 60)))
        .timeout_resolve(Some(Duration::from_secs(15)))
        .timeout_connect(Some(Duration::from_secs(15)))
        // 自己判状态码，好把 URL 与 status 一起报出来
        .http_status_as_error(false)
        .build();
    let agent = ureq::Agent::new_with_config(config);

    let resp = agent
        .get(url)
        .call()
        .with_context(|| format!("下载 {} 失败", url))?;
    let status = resp.status();
    if !status.is_success() {
        bail!(
            "下载 {} 失败：HTTP {} {}",
            url,
            status.as_str(),
            status.canonical_reason().unwrap_or("")
        );
    }

    // 流式落地，不全量进内存（归档可能几百 MB）。sha256 统一由调用方的
    // `sha256_hex` 算——本地与 URL 两条路共用同一份哈希代码。
    let mut reader = resp.into_body().into_reader();
    let mut file =
        std::fs::File::create(dest).with_context(|| format!("创建 {} 失败", dest.display()))?;
    std::io::copy(&mut reader, &mut file)
        .with_context(|| format!("写下载内容到 {} 失败", dest.display()))?;
    Ok(())
}

/// 归档名 → 落盘文件名（去掉可能捣乱的字符，只留 basename）。
fn sanitized_file_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "source.tar.gz".to_string()
    } else {
        cleaned
    }
}

/// 从归档名/目录名推版本：`ELYTerminal-0.1.8-linux-x86_64` → `0.1.8`。
///
/// 剥掉 `.tar.gz` / `.tgz` / `.zip`，按 `-` 与 `_` 切 token，取第一个形如
/// `\d+(\.\d+)*` 的（容忍前导 `v`）。取不到返回 `None`——调用方再找别的数据源。
pub fn infer_version(name: &str) -> Option<String> {
    let stem = name
        .trim_end_matches(".tar.gz")
        .trim_end_matches(".tgz")
        .trim_end_matches(".zip");
    for token in stem.split(['-', '_']) {
        let t = token.trim_start_matches(['v', 'V']);
        if t.is_empty() {
            continue;
        }
        let parts: Vec<&str> = t.split('.').collect();
        if parts.len() >= 2
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        {
            return Some(t.to_string());
        }
        // 单段纯数字（如 `-2`）不算版本号，继续找下一个 token
    }
    None
}

/// 从归档名推 CPU 架构：`x86_64`/`amd64` → `x86_64`；`aarch64`/`arm64` → `aarch64`。
///
/// 只按 `-` 与 `.` 切分——**不能按 `_` 切**，`x86_64` 本身带下划线。
pub fn infer_arch(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    for token in lower.split(['-', '.']) {
        match token {
            "x86_64" | "amd64" | "x64" => return Some("x86_64"),
            "aarch64" | "arm64" => return Some("aarch64"),
            _ => {}
        }
    }
    None
}

/// GitHub release asset URL → 项目 URL（剥 `/releases/download/...` 及以后）。
/// 非 GitHub 形态返回 `None`（调用方回退用 URL 原文）。
pub fn infer_project_url(url: &str) -> Option<String> {
    let (host, path) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?
        .split_once('/')?;
    if !host.ends_with("github.com") {
        return None;
    }
    let mut segments: Vec<&str> = path.split('/').collect();
    if let Some(pos) = segments.iter().position(|s| *s == "releases") {
        segments.truncate(pos);
    }
    if segments.len() >= 2 {
        Some(format!("https://{}/{}", host, segments[..2].join("/")))
    } else {
        None
    }
}

/// ELF 魔数（可执行文件头 4 字节）——比 exec 位可靠（tar 可能丢权限位，
/// 而 `.so` 也是 ELF，所以还要按扩展名排除共享库）。
const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];

/// 一个 ELF 候选（解包目录内）。
#[derive(Debug, Clone)]
struct ElfCandidate {
    /// 相对 `TarballLayout::root`
    rel: PathBuf,
    /// 有可执行位（仅作优先级，非硬条件：tar 可能丢权限）
    executable: bool,
}

/// tar.gz 内的一个图标。
#[derive(Debug, Clone)]
pub struct TarballIcon {
    /// 相对 `TarballLayout::root`
    pub rel: PathBuf,
    /// hicolor 尺寸子目录：`scalable` 或 `512x512` 等
    pub size_dir: String,
    /// `svg` / `png`
    pub ext: String,
}

/// tar.gz 的角色分类结果。
///
/// **注意**：`root` 指向临时目录内的解包结果，由 `_temp` 保活。因此生成物里
/// 只能写**相对 root 的路径**（配 `$srcdir` 使用），不能写 root 本身。
#[derive(Debug)]
pub struct TarballLayout {
    /// 单一顶层包装目录名（`ELYTerminal-0.1.8-linux-x86_64`）；tar bomb 为 `None`
    pub wrapper: Option<String>,
    /// 布局根：wrapper 内层，或（tar bomb 时）解包根
    pub root: PathBuf,
    /// ELF 可执行文件，相对 `root`
    pub binary: PathBuf,
    /// 二进制 basename
    pub bin_name: String,
    /// 包内 `.desktop`（相对 root，可能多个）
    pub desktops: Vec<PathBuf>,
    /// 包内图标（`.svg` / `.png`）
    pub icons: Vec<TarballIcon>,
    /// 无法归类的文件（打印提示，v1 忽略；`/opt` 兜底为规划中）
    pub extras: Vec<PathBuf>,
    /// 保活：解包目录
    _temp: TempDir,
}

/// 解包 tar.gz 并按角色分类。
///
/// 用 `tar -xzf` shell out（与 targz.rs 打包一致，零新依赖）：GNU tar 默认
/// 剥掉成员名的前导 `/`、拒绝含 `..` 的成员，路径穿越已由它拦住；这里再加
/// 「解进全新空目录 + 遍历不跟随符号链接 + 路径 must starts_with(root)」。
///
/// `bin_override`（`--bin-name`）：多个 ELF 时按 basename 精确匹配消歧。
pub fn inspect_targz(
    resolved: &ResolvedSource,
    bin_override: Option<&str>,
) -> Result<TarballLayout> {
    let dir = TempDir::new().context("创建解包临时目录失败")?;
    let output = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(&resolved.archive)
        .arg("-C")
        .arg(dir.path())
        .output()
        .with_context(|| {
            format!(
                "运行 tar -xzf 失败（需要系统 tar）：{}",
                resolved.archive.display()
            )
        })?;
    anyhow::ensure!(
        output.status.success(),
        "解包 {} 失败：{}",
        resolved.archive.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );

    let (wrapper, root) = detect_wrapper(dir.path())?;

    // ELF：深度 ≤2（覆盖 wrapper 内一层与 bin/），全深度兜底见 pick_elf。
    let mut candidates = elf_candidates(&root, 2);
    if candidates.is_empty() {
        let deep = elf_candidates(&root, usize::MAX);
        if deep.len() == 1 {
            println!(
                "提示：二进制在更深的子目录（{}），已自动采纳；\
                 若不是你要的入口，请用 --bin-name 指定。",
                deep[0].rel.display()
            );
            candidates = deep;
        } else if deep.len() > 1 {
            let names: Vec<String> = deep.iter().map(|c| c.rel.display().to_string()).collect();
            bail!(
                "解包内容里在深度 ≤2 找不到 ELF 可执行文件，深层又有多个候选：{}。\
                 请用 --bin-name 指定要安装哪个。",
                names.join("、")
            );
        }
    }
    let binary = pick_elf(&candidates, bin_override, &root)?;
    let bin_name = binary
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    let mut desktops = Vec::new();
    let mut icons = Vec::new();
    let mut extras = Vec::new();
    for entry in walkdir::WalkDir::new(&root).follow_links(false) {
        let entry = entry.context("遍历解包目录失败")?;
        if !entry.file_type().is_file() {
            continue;
        }
        let abs = entry.path();
        let Some(rel) = safe_rel(&root, abs) else {
            continue;
        };
        if rel == binary {
            continue;
        }
        let name = rel
            .file_name()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if name.ends_with(".desktop") {
            desktops.push(rel);
        } else if name.ends_with(".svg") {
            icons.push(TarballIcon {
                rel,
                size_dir: "scalable".to_string(),
                ext: "svg".to_string(),
            });
        } else if name.ends_with(".png") {
            icons.push(TarballIcon {
                size_dir: png_size_dir(&name),
                rel,
                ext: "png".to_string(),
            });
        } else {
            extras.push(rel);
        }
    }

    if !extras.is_empty() {
        println!(
            "提示：以下 {} 个文件无法归类，v1 忽略不装（/opt 兜底为规划中）：{}",
            extras.len(),
            extras
                .iter()
                .take(8)
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join("、")
        );
    }

    Ok(TarballLayout {
        wrapper,
        root,
        binary,
        bin_name,
        desktops,
        icons,
        extras,
        _temp: dir,
    })
}

/// 包装目录识别：解包根下恰好 1 项且为目录 → 该项即 wrapper。
fn detect_wrapper(extract_root: &Path) -> Result<(Option<String>, PathBuf)> {
    let mut entries: Vec<_> = std::fs::read_dir(extract_root)
        .with_context(|| format!("读取解包目录失败：{}", extract_root.display()))?
        .filter_map(|e| e.ok())
        .collect();
    if entries.len() == 1 {
        let entry = entries.pop().unwrap();
        if entry.path().is_dir() && !entry.path().is_symlink() {
            let name = entry.file_name().to_string_lossy().into_owned();
            return Ok((Some(name.clone()), entry.path()));
        }
    }
    Ok((None, extract_root.to_path_buf()))
}

/// 收集 `root` 下深度 ≤`max_depth` 的 ELF 文件（跳过 `*.so*` 共享库与符号链接）。
fn elf_candidates(root: &Path, max_depth: usize) -> Vec<ElfCandidate> {
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .max_depth(max_depth)
        .follow_links(false)
    {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() {
            continue;
        }
        let abs = entry.path();
        let Some(rel) = safe_rel(root, abs) else {
            continue;
        };
        let name = rel
            .file_name()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        // .so 也是 ELF，但不是可执行入口
        if name.contains(".so") {
            continue;
        }
        if !is_elf(abs) {
            continue;
        }
        out.push(ElfCandidate {
            rel,
            executable: is_executable(abs),
        });
    }
    out
}

/// 从候选中挑二进制：覆盖名精确匹配 > 唯一 > 有可执行位 > 报错并列。
fn pick_elf(
    candidates: &[ElfCandidate],
    bin_override: Option<&str>,
    root: &Path,
) -> Result<PathBuf> {
    if candidates.is_empty() {
        bail!(
            "解包内容里找不到 ELF 可执行文件（{} 内）：不是可执行程序包？\
             （v1 只支持「包内含 ELF 二进制」的 tar.gz）",
            root.display()
        );
    }
    if let Some(want) = bin_override {
        if let Some(hit) = candidates
            .iter()
            .find(|c| c.rel.file_name().map(|s| s == want).unwrap_or(false))
        {
            return Ok(hit.rel.clone());
        }
        let names: Vec<String> = candidates
            .iter()
            .map(|c| c.rel.display().to_string())
            .collect();
        bail!(
            "--bin-name 指定的 `{want}` 不在解包候选里（候选：{}）",
            names.join("、")
        );
    }
    if candidates.len() == 1 {
        return Ok(candidates[0].rel.clone());
    }
    let execs: Vec<&ElfCandidate> = candidates.iter().filter(|c| c.executable).collect();
    if execs.len() == 1 {
        return Ok(execs[0].rel.clone());
    }
    let names: Vec<String> = candidates
        .iter()
        .map(|c| {
            format!(
                "{}{}",
                c.rel.display(),
                if c.executable { " (可执行)" } else { "" }
            )
        })
        .collect();
    bail!(
        "发现 {} 个可执行文件，无法判断该装哪个：{}。请用 --bin-name 指定。",
        candidates.len(),
        names.join("、")
    );
}

/// 文件头 4 字节是否为 ELF 魔数。
fn is_elf(path: &Path) -> bool {
    let mut head = [0u8; 4];
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    use std::io::Read as _;
    match f.read(&mut head) {
        Ok(4) => head == ELF_MAGIC,
        _ => false,
    }
}

/// 是否有可执行位（仅 unix；非 unix 一律 false，只作优先级用）。
fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        path.metadata()
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

/// `abs` 相对 `root` 的安全相对路径：canonicalize 后必须仍在 root 内。
fn safe_rel(root: &Path, abs: &Path) -> Option<PathBuf> {
    if abs.is_symlink() {
        return None;
    }
    let abs = abs.canonicalize().ok()?;
    let root = root.canonicalize().ok()?;
    abs.strip_prefix(&root).ok().map(|p| p.to_path_buf())
}

/// PNG 图标的 hicolor 尺寸目录（按文件名约定，不解码图片）：
/// `@2x` → 1024x1024；`64x64` → 64x64；`icon_256` → 256x256；其余 → 512x512
/// （与本包 `desktop::icon_size_dir` 的默认一致）。
fn png_size_dir(name: &str) -> String {
    let stem = name.trim_end_matches(".png");
    if stem.ends_with("@2x") {
        return "1024x1024".to_string();
    }
    // 形如 `64x64` / `icon-128x128`
    for token in stem.split(['-', '_', '.']) {
        if let Some((w, h)) = token.split_once('x')
            && !w.is_empty()
            && !h.is_empty()
            && w.chars().all(|c| c.is_ascii_digit())
            && h.chars().all(|c| c.is_ascii_digit())
        {
            return format!("{w}x{h}");
        }
    }
    // 形如 `icon_256`
    let tail: String = stem
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    if !tail.is_empty() {
        return format!("{tail}x{tail}");
    }
    "512x512".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_input_classifies_local_url_and_file_scheme() {
        assert_eq!(
            SourceInput::parse("/abs/x.tar.gz").unwrap(),
            SourceInput::Local(PathBuf::from("/abs/x.tar.gz"))
        );
        assert_eq!(
            SourceInput::parse("./rel/x.tar.gz").unwrap(),
            SourceInput::Local(PathBuf::from("./rel/x.tar.gz"))
        );
        assert_eq!(
            SourceInput::parse("file:///abs/x.tar.gz").unwrap(),
            SourceInput::Local(PathBuf::from("/abs/x.tar.gz"))
        );
        assert_eq!(
            SourceInput::parse("https://example.com/a.tar.gz").unwrap(),
            SourceInput::Url("https://example.com/a.tar.gz".to_string())
        );
        assert_eq!(
            SourceInput::parse("http://example.com/a.tar.gz").unwrap(),
            SourceInput::Url("http://example.com/a.tar.gz".to_string())
        );
        // Windows 路径不含 :// ，不会被误判成 URL
        assert_eq!(
            SourceInput::parse(r"C:\tmp\x.tar.gz").unwrap(),
            SourceInput::Local(PathBuf::from(r"C:\tmp\x.tar.gz"))
        );
    }

    #[test]
    fn source_input_rejects_unknown_scheme() {
        let err = SourceInput::parse("ftp://example.com/a.tar.gz").unwrap_err();
        assert!(
            err.to_string().contains("只支持"),
            "应报不支持的 scheme：{err}"
        );
        assert!(SourceInput::parse("https://").is_err());
        assert!(SourceInput::parse("file://").is_err());
    }

    #[test]
    fn archive_name_strips_query_and_directory() {
        let url = SourceInput::Url(
            "https://github.com/o/r/releases/download/v0.1.8/App-0.1.8-linux-x86_64.tar.gz?raw=1"
                .to_string(),
        );
        assert_eq!(url.archive_name(), "App-0.1.8-linux-x86_64.tar.gz");
        let local = SourceInput::Local(PathBuf::from("/tmp/other.tar.gz"));
        assert_eq!(local.archive_name(), "other.tar.gz");
        // 取不到时给个可用回退，不影响主流程
        assert_eq!(
            SourceInput::Url("https://example.com/".to_string()).archive_name(),
            "source.tar.gz"
        );
    }

    #[test]
    fn local_source_computes_sha256_and_file_entry() {
        let dir =
            std::env::temp_dir().join(format!("aa-bundle-source-{}-local", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let gz = dir.join("sample.tar.gz");
        // gzip 头 + 若干字节（内容不重要，只要魔数对）
        std::fs::write(&gz, [0x1f, 0x8b, 0x08, 0x00, b'a', b'b', b'c']).unwrap();

        let input = SourceInput::Local(gz.clone());
        let resolved = resolve_source(&input, None).unwrap();
        assert_eq!(resolved.sha256.len(), 64, "sha256 应为 64 个 hex 字符");
        assert!(
            resolved.source_entry.starts_with("file:///"),
            "本地源应转成 file:// 条目：{}",
            resolved.source_entry
        );
        assert!(resolved.source_entry.ends_with("sample.tar.gz"));
        // 与独立计算一致（canonicalize 后的绝对路径参与哈希，再算一次对照）
        assert_eq!(resolved.sha256, sha256_hex(&gz).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expected_sha256_mismatch_errors() {
        let dir =
            std::env::temp_dir().join(format!("aa-bundle-source-{}-mismatch", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let gz = dir.join("s.tar.gz");
        std::fs::write(&gz, [0x1f, 0x8b, 0x08, 0x00, b'x']).unwrap();
        let real = sha256_hex(&gz).unwrap();

        let err = resolve_source(&SourceInput::Local(gz.clone()), Some("deadbeef")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("校验和不匹配"), "应报校验和不匹配：{msg}");
        assert!(
            msg.contains("deadbeef") && msg.contains(&real),
            "应列出期望与实际：{msg}"
        );

        // 匹配时不报错（大小写不敏感）
        resolve_source(&SourceInput::Local(gz.clone()), Some(&real.to_uppercase())).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_gzip_and_zip_are_rejected() {
        let dir =
            std::env::temp_dir().join(format!("aa-bundle-source-{}-kind", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let plain = dir.join("plain.txt");
        std::fs::write(&plain, b"hello").unwrap();
        let err = resolve_source(&SourceInput::Local(plain), None).unwrap_err();
        assert!(err.to_string().contains("只支持"), "文本文件应被拒：{err}");

        let zip = dir.join("a.zip");
        std::fs::write(&zip, [b'P', b'K', 0x03, 0x04, 0x00]).unwrap();
        let err = resolve_source(&SourceInput::Local(zip), None).unwrap_err();
        assert!(
            err.to_string().contains("zip"),
            "zip 应被明确识别并报暂未支持：{err}"
        );

        let dir_src = dir.join("adir");
        std::fs::create_dir_all(&dir_src).unwrap();
        let err = resolve_source(&SourceInput::Local(dir_src), None).unwrap_err();
        assert!(err.to_string().contains("不是文件"), "目录源应被拒：{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn infer_version_and_arch_from_names() {
        assert_eq!(
            infer_version("ELYTerminal-0.1.8-linux-x86_64.tar.gz"),
            Some("0.1.8".to_string())
        );
        assert_eq!(
            infer_version("app_v2.3.4-aarch64.tgz"),
            Some("2.3.4".to_string())
        );
        assert_eq!(
            infer_version("Foo-v1.0-linux-x86_64.tar.gz"),
            Some("1.0".to_string())
        );
        assert_eq!(infer_version("no-version-here.tar.gz"), None);
        assert_eq!(
            infer_version("app-2-linux-x86_64.tar.gz"),
            None,
            "单段数字不算版本号"
        );

        assert_eq!(infer_arch("App-0.1.8-linux-x86_64.tar.gz"), Some("x86_64"));
        assert_eq!(infer_arch("App-0.1.8-amd64.tgz"), Some("x86_64"));
        assert_eq!(infer_arch("App-0.1.8-arm64.tgz"), Some("aarch64"));
        assert_eq!(infer_arch("App-0.1.8-aarch64.tgz"), Some("aarch64"));
        assert_eq!(infer_arch("App-0.1.8.tar.gz"), None);
    }

    // ---- 布局识别：fixture 用系统 tar 构建（与 targz.rs 测试同款做法）----

    fn fixture_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("aa-bundle-inspect-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `tar -czf <out> -C <parent> <member>`：member 为 "." 即打成 tar bomb。
    fn make_targz(parent: &Path, member: &str, tag: &str) -> PathBuf {
        let out = parent
            .parent()
            .unwrap()
            .join(format!("{tag}-{}.tar.gz", std::process::id()));
        let status = std::process::Command::new("tar")
            .arg("-czf")
            .arg(&out)
            .arg("-C")
            .arg(parent)
            .arg(member)
            .status()
            .expect("需要系统 tar");
        assert!(status.success(), "构建 fixture tarball 失败");
        out
    }

    fn write_elf(path: &Path) {
        std::fs::write(path, [0x7f, b'E', b'L', b'F', 0x02, 0x01, 0x01, 0x00, b'x']).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn layout_wrapper_dir_binary_desktop_icon() {
        let base = fixture_dir("wrapper");
        let wrapper = base.join("App-0.1.8-linux-x86_64");
        std::fs::create_dir_all(&wrapper).unwrap();
        write_elf(&wrapper.join("app"));
        std::fs::write(wrapper.join("app.desktop"), "[Desktop Entry]").unwrap();
        std::fs::write(wrapper.join("app.svg"), "<svg/>").unwrap();

        let gz = make_targz(&base, "App-0.1.8-linux-x86_64", "wrapper");
        let resolved = resolve_source(&SourceInput::Local(gz), None).unwrap();
        let layout = inspect_targz(&resolved, None).unwrap();

        assert_eq!(layout.wrapper.as_deref(), Some("App-0.1.8-linux-x86_64"));
        assert_eq!(layout.binary, PathBuf::from("app"));
        assert_eq!(layout.bin_name, "app");
        assert_eq!(layout.desktops, vec![PathBuf::from("app.desktop")]);
        assert_eq!(layout.icons.len(), 1);
        assert_eq!(layout.icons[0].size_dir, "scalable");
        assert_eq!(layout.icons[0].ext, "svg");
        assert!(layout.extras.is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn layout_tar_bomb_flat() {
        let base = fixture_dir("bomb");
        let staging = base.join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        write_elf(&staging.join("tool"));
        std::fs::write(staging.join("tool.desktop"), "[Desktop Entry]").unwrap();

        let gz = make_targz(&staging, ".", "bomb");
        let resolved = resolve_source(&SourceInput::Local(gz), None).unwrap();
        let layout = inspect_targz(&resolved, None).unwrap();

        assert_eq!(layout.wrapper, None, "无包装目录时 wrapper 应为 None");
        assert_eq!(layout.binary, PathBuf::from("tool"));
        assert_eq!(layout.desktops.len(), 1);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn layout_bin_subdir_depth_two() {
        let base = fixture_dir("subdir");
        let wrapper = base.join("App-1.0-linux-x86_64");
        std::fs::create_dir_all(wrapper.join("bin")).unwrap();
        write_elf(&wrapper.join("bin").join("app"));

        let gz = make_targz(&base, "App-1.0-linux-x86_64", "subdir");
        let resolved = resolve_source(&SourceInput::Local(gz), None).unwrap();
        let layout = inspect_targz(&resolved, None).unwrap();

        assert_eq!(
            layout.binary,
            PathBuf::from("bin/app"),
            "深度 2 的 bin/ 应被采纳"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn layout_deep_usr_bin_falls_back_with_single_candidate() {
        let base = fixture_dir("deep");
        let wrapper = base.join("App-1.0-linux-x86_64");
        std::fs::create_dir_all(wrapper.join("usr").join("bin")).unwrap();
        write_elf(&wrapper.join("usr").join("bin").join("app"));

        let gz = make_targz(&base, "App-1.0-linux-x86_64", "deep");
        let resolved = resolve_source(&SourceInput::Local(gz), None).unwrap();
        let layout = inspect_targz(&resolved, None).unwrap();

        assert_eq!(
            layout.binary,
            PathBuf::from("usr/bin/app"),
            "唯一深层候选应兜底采纳"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn layout_so_files_are_not_binary_candidates() {
        let base = fixture_dir("so");
        let wrapper = base.join("App-1.0");
        std::fs::create_dir_all(&wrapper).unwrap();
        write_elf(&wrapper.join("libfoo.so"));
        write_elf(&wrapper.join("app"));

        let gz = make_targz(&base, "App-1.0", "so");
        let resolved = resolve_source(&SourceInput::Local(gz), None).unwrap();
        let layout = inspect_targz(&resolved, None).unwrap();

        assert_eq!(
            layout.binary,
            PathBuf::from("app"),
            ".so 不应成为二进制候选"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn layout_multiple_elf_errors_and_bin_name_disambiguates() {
        let base = fixture_dir("multi");
        let wrapper = base.join("App-1.0");
        std::fs::create_dir_all(&wrapper).unwrap();
        write_elf(&wrapper.join("app"));
        write_elf(&wrapper.join("helper"));

        let gz = make_targz(&base, "App-1.0", "multi");
        let resolved = resolve_source(&SourceInput::Local(gz.clone()), None).unwrap();

        let err = inspect_targz(&resolved, None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("无法判断该装哪个"), "多 ELF 应报错：{msg}");
        assert!(
            msg.contains("app") && msg.contains("helper"),
            "应列出候选：{msg}"
        );

        // --bin-name 消歧
        let layout = inspect_targz(&resolved, Some("helper")).unwrap();
        assert_eq!(layout.binary, PathBuf::from("helper"));
        // 指定的名字不在候选里要报错，而不是静默选一个
        assert!(inspect_targz(&resolved, Some("nope")).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn layout_zero_elf_errors() {
        let base = fixture_dir("noelf");
        let wrapper = base.join("App-1.0");
        std::fs::create_dir_all(&wrapper).unwrap();
        std::fs::write(wrapper.join("README.md"), "hi").unwrap();

        let gz = make_targz(&base, "App-1.0", "noelf");
        let resolved = resolve_source(&SourceInput::Local(gz), None).unwrap();
        let err = inspect_targz(&resolved, None).unwrap_err();
        assert!(
            err.to_string().contains("找不到 ELF"),
            "无 ELF 应报错：{err}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn layout_extras_are_collected_and_icons_sized() {
        let base = fixture_dir("extras");
        let wrapper = base.join("App-1.0");
        std::fs::create_dir_all(&wrapper).unwrap();
        write_elf(&wrapper.join("app"));
        std::fs::write(wrapper.join("README.md"), "hi").unwrap();
        std::fs::write(wrapper.join("LICENSE"), "mit").unwrap();
        std::fs::write(wrapper.join("icon.png"), "png").unwrap();

        let gz = make_targz(&base, "App-1.0", "extras");
        let resolved = resolve_source(&SourceInput::Local(gz), None).unwrap();
        let layout = inspect_targz(&resolved, None).unwrap();

        assert!(layout.extras.iter().any(|p| p.ends_with("README.md")));
        assert!(layout.extras.iter().any(|p| p.ends_with("LICENSE")));
        assert_eq!(layout.icons.len(), 1);
        assert_eq!(layout.icons[0].size_dir, "512x512", "裸名 png 走 512x512");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn png_size_dir_from_filename() {
        assert_eq!(png_size_dir("icon.png"), "512x512");
        assert_eq!(png_size_dir("icon@2x.png"), "1024x1024");
        assert_eq!(png_size_dir("64x64.png"), "64x64");
        assert_eq!(png_size_dir("app-128x128.png"), "128x128");
        assert_eq!(png_size_dir("icon_256.png"), "256x256");
    }

    #[test]
    fn safe_rel_rejects_paths_outside_root_and_symlinks() {
        let base = fixture_dir("safe");
        std::fs::create_dir_all(&base).unwrap();
        // 根外路径
        assert!(safe_rel(&base, &std::env::temp_dir()).is_none());
        // 根内正常文件
        let inside = base.join("f");
        std::fs::write(&inside, b"x").unwrap();
        assert_eq!(safe_rel(&base, &inside), Some(PathBuf::from("f")));
        // 符号链接一律不接受
        #[cfg(unix)]
        {
            let link = base.join("link");
            std::os::unix::fs::symlink("/etc/passwd", &link).unwrap();
            assert!(safe_rel(&base, &link).is_none());
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn infer_project_url_from_github_release() {
        assert_eq!(
            infer_project_url(
                "https://github.com/ELY-Open-Source/ELYTerminal/releases/download/v0.1.8/x.tar.gz"
            ),
            Some("https://github.com/ELY-Open-Source/ELYTerminal".to_string())
        );
        assert_eq!(
            infer_project_url("https://github.com/o/r"),
            Some("https://github.com/o/r".to_string())
        );
        assert_eq!(
            infer_project_url("https://example.com/o/r/releases/download/v1/x.tar.gz"),
            None
        );
    }
}
