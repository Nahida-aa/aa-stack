//! 按操作系统安装软件：先查发行版仓库 / AUR，查不到现成的就用 aa-bundle
//! 把上游 tar.gz 打成包再装。
//!
//! v1 只做 **Arch 原生包**一条路（`makepkg` + pacman，卸载交给 pacman）。
//!
//! ## 决策链
//!
//! ```text
//! 输入：包名 / 本地 tar.gz / http(s) URL
//!   → 已装？    → 报告版本，退出（--force 重装）
//!   → 仓库有？  → sudo pacman -S
//!   → AUR 有？  → paru/yay -S（helper 自己提权，不加 sudo）
//!   → 都没有    → aa-bundle 生成 PKGBUILD（-bin）→ makepkg -si
//! ```
//!
//! 模块分工：
//!
//! | 模块 | 职责 |
//! |---|---|
//! | [`os`] | 发行版门禁、工具与权限探测 |
//! | [`lookup`] | 输入识别与「现成的包」查找（解析为纯函数） |
//! | [`bundle`] | 合成最小 bundle 元数据 → 调 aa-bundle 生成 PKGBUILD |
//! | [`install`] | 命令构造（纯）与执行（继承 stdio） |
//! | [`plan`] | 计划解析 / 渲染 / 执行 |

mod bundle;
mod install;
mod lookup;
mod os;
mod plan;

use anyhow::Result;
use clap::Parser;

use plan::Step;

#[derive(Parser)]
#[command(
    name = "aa-install",
    about = "按操作系统安装软件：先查仓库/AUR，没有现成的就打包后安装",
    long_about = "\
按操作系统安装软件：先查仓库/AUR，没有现成的就打包后安装。

目标可以是包名、本地 tar.gz 路径或 http(s) URL。v1 只支持 Arch 系
（pacman / makepkg）；zip、deb、rpm 与 AppImage 尚未支持，
依赖补全（ldd → depends）也还没做。

--dry-run 仍会把 PKGBUILD 生成到缓存目录（便于查看），只跳过安装类命令。"
)]
struct Cli {
    /// 安装目标：包名 / 本地 tar.gz 路径 / tar.gz 的 http(s) URL
    target: String,

    /// 只打印将要执行的计划，不改动系统
    #[arg(long)]
    dry_run: bool,

    /// 给了归档（URL 或本地路径）时，即使仓库/AUR 里有同名包也坚持打包
    #[arg(long)]
    bundle: bool,

    /// 已装同版本时也重装（pkgrel 恒为 1，重复安装需要它）
    #[arg(long)]
    force: bool,

    /// AUR 有多个候选时指定包名（不指定则只认精确名匹配）
    #[arg(short = 'p', long)]
    pick: Option<String>,

    /// 打包时覆盖二进制名（也用于 tarball 内多个 ELF 时消歧）
    #[arg(long)]
    bin_name: Option<String>,

    /// 打包产物的安装前缀，默认 /usr
    #[arg(long)]
    prefix: Option<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let tools = os::ensure_arch_env()?;
    let target = lookup::classify(&cli.target)?;
    let opts = plan::Options {
        force: cli.force,
        bundle_only: cli.bundle,
        pick: cli.pick.as_deref(),
        bin_name: cli.bin_name.as_deref(),
        prefix: cli.prefix.as_deref(),
    };
    let plan = plan::resolve(&target, &tools, &opts)?;

    if cli.dry_run {
        print!("{}", plan::render(&plan));
        return Ok(());
    }

    if matches!(plan.steps.first(), Some(Step::Bundle(_))) && cli.target.starts_with("http") {
        println!(
            "提示：makepkg 会自己再下载一次该归档（生成 PKGBUILD 时已下载一次用于识别），慢网请耐心。"
        );
    }
    plan::execute(&plan, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_target() {
        let c = Cli::try_parse_from(["aa-install", "bash"]).unwrap();
        assert_eq!(c.target, "bash");
        assert!(!c.dry_run && !c.bundle && !c.force);
    }

    #[test]
    fn parses_flags() {
        let c = Cli::try_parse_from([
            "aa-install",
            "https://example.com/App-1.0-linux-x86_64.tar.gz",
            "--dry-run",
            "--bundle",
            "--force",
            "--pick",
            "app-bin",
            "--bin-name",
            "app",
            "--prefix",
            "/usr/local",
        ])
        .unwrap();
        assert!(c.dry_run && c.bundle && c.force);
        assert_eq!(c.pick.as_deref(), Some("app-bin"));
        assert_eq!(c.bin_name.as_deref(), Some("app"));
        assert_eq!(c.prefix.as_deref(), Some("/usr/local"));
    }

    #[test]
    fn target_is_required() {
        assert!(Cli::try_parse_from(["aa-install"]).is_err());
    }
}
