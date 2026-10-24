//! 外部命令：构造（**纯函数**）与执行（薄壳）。
//!
//! 拆分这两者的唯一目的是为了可测：安装类命令有副作用且依赖本机状态，
//! 单测只能打 [`argv_of`] 的返回值；执行器本身只有十几行，靠人工跑通。
//!
//! 两条硬规则（踩过才知道）：
//!
//! 1. **安装类命令必须继承 stdio**。`pacman -S` / `paru -S` / `makepkg -si`
//!    都有确认提示，还会读 sudo 密码——用 `.output()` 捕获会把这些吞掉，
//!    用户看到的是「命令卡住了」。
//! 2. **AUR helper 绝不加 sudo**。helper 内部自己在需要时提权；给它套
//!    `sudo` 会以 root 跑构建，而 makepkg 在 EUID==0 时直接拒绝。

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context as _, Result, bail};

use crate::os::AurHelper;

/// 一个待执行的操作（构造与执行分离）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    /// 发行版仓库里的包
    PacmanInstall { name: String, needed: bool },
    /// AUR 里的包（走 helper，不加 sudo）
    AurInstall { helper: AurHelper, name: String },
    /// 在构建目录里跑 makepkg
    Makepkg { flags: Vec<String> },
}

/// 把操作展开成 argv（sudo 前缀在此决定）。
pub fn argv_of(c: &Cmd, sudo: bool) -> Vec<String> {
    match c {
        Cmd::PacmanInstall { name, needed } => {
            let mut argv = Vec::new();
            if sudo {
                argv.push("sudo".to_string());
            }
            argv.push("pacman".to_string());
            argv.push("-S".to_string());
            if *needed {
                argv.push("--needed".to_string());
            }
            argv.push(name.clone());
            argv
        }
        // 注释见模块文档：这里恒不加 sudo
        Cmd::AurInstall { helper, name } => {
            vec![helper.name().to_string(), "-S".to_string(), name.clone()]
        }
        Cmd::Makepkg { flags } => {
            let mut argv = vec!["makepkg".to_string()];
            argv.extend(flags.iter().cloned());
            argv
        }
    }
}

/// 执行安装类命令（继承 stdio）。`cwd` 用于 makepkg（要在构建目录里跑）。
pub fn run_inherit(c: &Cmd, cwd: Option<&Path>, sudo: bool) -> Result<()> {
    let argv = argv_of(c, sudo);
    let (prog, args) = argv.split_first().expect("argv_of 至少返回程序名");
    let mut cmd = Command::new(prog);
    cmd.args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let status = cmd
        .status()
        .with_context(|| format!("启动 `{}` 失败", argv.join(" ")))?;
    if !status.success() {
        bail!("`{}` 失败（退出码 {:?}）", argv.join(" "), status.code());
    }
    Ok(())
}

/// 执行**只读查询**并捕获输出（`-Si` / `-Q` / `-Ss` 这类）。
///
/// 强制 `LC_ALL=C`：pacman 的输出是按 locale 本地化的（本机中文环境下字段
/// 名是「软件库 / 版本」），解析必须先把语言钉死。
pub fn run_capture(prog: &str, args: &[&str]) -> Result<(i32, String, String)> {
    let out = Command::new(prog)
        .args(args)
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .output()
        .with_context(|| format!("启动 `{prog}` 失败"))?;
    Ok((
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pacman_install_adds_sudo_when_not_root() {
        let c = Cmd::PacmanInstall {
            name: "bash".into(),
            needed: true,
        };
        assert_eq!(
            argv_of(&c, true),
            vec!["sudo", "pacman", "-S", "--needed", "bash"]
        );
        assert_eq!(argv_of(&c, false), vec!["pacman", "-S", "--needed", "bash"]);
    }

    #[test]
    fn pacman_install_without_needed() {
        let c = Cmd::PacmanInstall {
            name: "bash".into(),
            needed: false,
        };
        assert_eq!(argv_of(&c, true), vec!["sudo", "pacman", "-S", "bash"]);
    }

    #[test]
    fn aur_install_never_gets_sudo() {
        let c = Cmd::AurInstall {
            helper: AurHelper::Paru,
            name: "elyterminal-bin".into(),
        };
        // 即使 sudo=true 也不加——helper 内部自己提权
        assert_eq!(argv_of(&c, true), vec!["paru", "-S", "elyterminal-bin"]);
        assert_eq!(argv_of(&c, false), vec!["paru", "-S", "elyterminal-bin"]);
    }

    #[test]
    fn makepkg_carries_flags() {
        let c = Cmd::Makepkg {
            flags: vec!["-si".into(), "--clean".into()],
        };
        assert_eq!(argv_of(&c, false), vec!["makepkg", "-si", "--clean"]);
    }
}
