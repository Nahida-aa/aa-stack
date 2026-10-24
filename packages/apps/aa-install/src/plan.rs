//! 安装计划：解析（查什么 / 打什么包）、渲染（`--dry-run`）、执行。
//!
//! 决策链（v1）：
//!
//! ```text
//! 已装？→ 报告版本退出（--force 才继续）
//! 仓库有？→ sudo pacman -S
//! AUR 有？→ paru/yay -S
//! 都没有 → aa-bundle 生成 PKGBUILD → makepkg -si
//! ```
//!
//! 给归档（URL / 本地路径）时同样先走查找——只有查不到（或 `--bundle`）才打包。

use anyhow::{Result, bail};

use crate::bundle::{self, BundlePlan};
use crate::install::{Cmd, argv_of, run_inherit};
use crate::lookup::{Target, aur_any, installed_any, query_keys, repo_any};
use crate::os::{AurHelper, Tools};

/// 解析时的开关。
#[derive(Debug, Clone, Default)]
pub struct Options<'a> {
    /// 已装同版本也继续（pkgrel 恒为 1，重复安装需要它）
    pub force: bool,
    /// 给了归档也坚持打包（不查仓库 / AUR）
    pub bundle_only: bool,
    /// AUR 多个候选时指定包名
    pub pick: Option<&'a str>,
    /// 打包时覆盖二进制名
    pub bin_name: Option<&'a str>,
    /// 打包产物的安装前缀
    pub prefix: Option<&'a str>,
}

/// 现成包的来源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    Repo { repo: String },
    Aur { helper: AurHelper },
}

#[derive(Debug, Clone)]
pub enum Step {
    /// 已经装了，什么都不做
    Already { name: String, ver: String },
    /// 用现成的包
    Existing {
        name: String,
        ver: String,
        origin: Origin,
        cmd: Cmd,
    },
    /// 自己打包后装
    Bundle(BundlePlan),
}

#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub steps: Vec<Step>,
}

/// 按目标解析出计划。
pub fn resolve(target: &Target, tools: &Tools, opts: &Options) -> Result<Plan> {
    match target {
        Target::Package(name) => resolve_package(name, tools, opts),
        Target::Archive { source, keyword } => resolve_archive(source, keyword, tools, opts),
    }
}

fn resolve_package(name: &str, tools: &Tools, opts: &Options) -> Result<Plan> {
    let keys = query_keys(name);
    if let Some(step) = already_installed(&keys, opts)? {
        return Ok(Plan { steps: vec![step] });
    }
    if let Some((n, info)) = repo_any(&keys)? {
        return Ok(existing(
            n,
            info.version,
            Origin::Repo { repo: info.repo },
            opts.force,
        ));
    }
    if let Some(helper) = tools.aur
        && let Some((n, hit)) = aur_pick(helper, &keys, opts)?
    {
        return Ok(existing(n, hit.version, Origin::Aur { helper }, opts.force));
    }
    bail!(
        "仓库与 AUR 里都没有 `{}`：\
         ① 给它 tar.gz 的路径或 URL 走打包安装；\
         ② 或先装 AUR helper（paru / yay）再重试",
        name
    );
}

fn resolve_archive(source: &str, keyword: &str, tools: &Tools, opts: &Options) -> Result<Plan> {
    let keys = query_keys(keyword);
    if !opts.bundle_only {
        if let Some(step) = already_installed(&keys, opts)? {
            return Ok(Plan { steps: vec![step] });
        }
        if let Some((n, info)) = repo_any(&keys)? {
            return Ok(existing(
                n,
                info.version,
                Origin::Repo { repo: info.repo },
                opts.force,
            ));
        }
        if let Some(helper) = tools.aur
            && let Some((n, hit)) = aur_pick(helper, &keys, opts)?
        {
            return Ok(existing(n, hit.version, Origin::Aur { helper }, opts.force));
        }
    }

    let plan = bundle::generate(source, keyword, opts.bin_name, opts.prefix)?;
    // 打包产物本身可能已装（pkgname 要到生成后才知道）。
    // `--bundle` 视同「就按我给的归档装」，与 --force 一样跳过这道检查。
    if !opts.force
        && !opts.bundle_only
        && let Some((name, ver)) = crate::lookup::installed_version(&plan.pkgname)?
    {
        return Ok(Plan {
            steps: vec![Step::Already { name, ver }],
        });
    }
    Ok(Plan {
        steps: vec![Step::Bundle(plan)],
    })
}

fn already_installed(keys: &[String], opts: &Options) -> Result<Option<Step>> {
    if opts.force {
        return Ok(None);
    }
    Ok(installed_any(keys)?.map(|(name, ver)| Step::Already { name, ver }))
}

fn existing(name: String, ver: String, origin: Origin, force: bool) -> Plan {
    // 仓库包走 pacman（自己加 sudo），AUR 包走 helper（helper 内部提权）
    let cmd = match &origin {
        Origin::Repo { .. } => Cmd::PacmanInstall {
            name: name.clone(),
            needed: !force,
        },
        Origin::Aur { helper } => Cmd::AurInstall {
            helper: *helper,
            name: name.clone(),
        },
    };
    Plan {
        steps: vec![Step::Existing {
            name,
            ver,
            origin,
            cmd,
        }],
    }
}

/// AUR 选择：`--pick` 优先，否则只认精确名。
fn aur_pick(
    helper: AurHelper,
    keys: &[String],
    opts: &Options,
) -> Result<Option<(String, crate::lookup::AurHit)>> {
    if let Some(pick) = opts.pick {
        for k in keys {
            let hits = crate::lookup::aur_search(helper, k)?;
            if let Some(hit) = hits.iter().find(|h| h.name == pick) {
                return Ok(Some((hit.name.clone(), hit.clone())));
            }
        }
        bail!(
            "AUR 搜索结果里没有 `{pick}`（用 `{} -Ss <关键字>` 看看有哪些）",
            helper.name()
        );
    }
    aur_any(helper, keys)
}

/// 渲染成给人看的文本（`--dry-run` 直接打印它）。
pub fn render(p: &Plan) -> String {
    let mut out = String::new();
    for step in &p.steps {
        match step {
            Step::Already { name, ver } => {
                out.push_str(&format!(
                    "已安装 {name} {ver} —— 无需操作（要重装加 --force）\n"
                ));
            }
            Step::Existing {
                name,
                ver,
                origin,
                cmd,
            } => {
                let from = match origin {
                    Origin::Repo { repo } => format!("仓库 {repo}"),
                    Origin::Aur { helper } => format!("AUR（经 {}）", helper.name()),
                };
                out.push_str(&format!("用现成的包：{from} 的 {name} {ver}\n"));
                out.push_str(&format!("  $ {}\n", argv_of(cmd, true).join(" ")));
            }
            Step::Bundle(b) => {
                out.push_str(&format!(
                    "仓库与 AUR 都没有现成的 —— 打包后安装：{} {}\n",
                    b.pkgname, b.pkgver
                ));
                out.push_str(&format!(
                    "  PKGBUILD：{}\n",
                    b.dir.join("PKGBUILD").display()
                ));
                if let Some(d) = &b.desktop {
                    out.push_str(&format!("  .desktop：{}\n", d.display()));
                }
                out.push_str(&format!(
                    "  $ (cd {} && {})\n",
                    b.dir.display(),
                    argv_of(
                        &Cmd::Makepkg {
                            flags: vec!["-si".to_string()]
                        },
                        false
                    )
                    .join(" ")
                ));
            }
        }
    }
    out
}

/// 执行计划。
pub fn execute(p: &Plan, sudo: bool) -> Result<()> {
    for step in &p.steps {
        match step {
            Step::Already { name, ver } => {
                println!("已安装 {name} {ver} —— 无需操作（要重装加 --force）");
            }
            Step::Existing { cmd, .. } => run_inherit(cmd, None, sudo)?,
            Step::Bundle(b) => {
                run_inherit(
                    &Cmd::Makepkg {
                        flags: vec!["-si".to_string()],
                    },
                    Some(&b.dir),
                    false,
                )?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_already_installed() {
        let p = Plan {
            steps: vec![Step::Already {
                name: "elyterminal-bin".into(),
                ver: "0.1.8-1".into(),
            }],
        };
        let s = render(&p);
        assert!(s.contains("已安装 elyterminal-bin 0.1.8-1"));
        assert!(s.contains("--force"));
    }

    #[test]
    fn render_repo_existing_shows_sudo() {
        let p = existing(
            "bash".into(),
            "5.3.20-1".into(),
            Origin::Repo {
                repo: "core".into(),
            },
            false,
        );
        let s = render(&p);
        assert!(s.contains("仓库 core"), "{s}");
        assert!(s.contains("sudo pacman -S --needed bash"), "{s}");
    }

    #[test]
    fn render_aur_existing_has_no_sudo() {
        let p = existing(
            "elyterminal-bin".into(),
            "0.1.8-1".into(),
            Origin::Aur {
                helper: AurHelper::Paru,
            },
            false,
        );
        let s = render(&p);
        assert!(s.contains("AUR（经 paru）"), "{s}");
        assert!(s.contains("$ paru -S elyterminal-bin"), "{s}");
        assert!(!s.contains("sudo"), "{s}");
    }

    #[test]
    fn render_bundle_step_shows_makepkg() {
        let p = Plan {
            steps: vec![Step::Bundle(BundlePlan {
                dir: std::path::PathBuf::from("/home/aa/.cache/aa-install/x"),
                pkgname: "x-bin".into(),
                pkgver: "1.0".into(),
                desktop: Some(std::path::PathBuf::from("/tmp/x.desktop")),
            })],
        };
        let s = render(&p);
        assert!(s.contains("x-bin 1.0"), "{s}");
        assert!(s.contains("makepkg -si"), "{s}");
        assert!(s.contains("/tmp/x.desktop"), "{s}");
    }

    #[test]
    fn force_drops_needed_flag() {
        let p = existing(
            "bash".into(),
            "1".into(),
            Origin::Repo {
                repo: "core".into(),
            },
            true,
        );
        let s = render(&p);
        assert!(s.contains("sudo pacman -S bash"), "{s}");
        assert!(!s.contains("--needed"), "{s}");
    }
}
