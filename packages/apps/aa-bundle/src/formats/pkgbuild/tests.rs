//! pkgbuild 的单元测试：PKGBUILD 文本生成、身份回退链、profile 与
//! prebuilt 模式、bin 名推导。helper（`test_dir` / `opts` / `opts_with`）
//! 建临时 crate 目录后跑 `generate_pkgbuild`，对产出文本做断言。

use std::path::PathBuf;

use super::*;

/// 建一个本测试独有的临时目录（pid + tag 防相撞）。
fn test_dir(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("aa-bundle-pkgbuild-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建临时目录失败");
    dir
}

#[test]
fn pkgbuild_uses_identity_from_bundle_meta() {
    let dir = test_dir("pkgbuild");
    std::fs::write(
        dir.join("Cargo.toml"),
        r#"
[package]
name = "aacode"
version = "1.23.2"
license = "Apache-2.0"

[[bin]]
name = "aacode"

[package.metadata.bundle-stable]
identifier = "dev.aa.AaCode"
name = "AaCode"
"#,
    )
    .unwrap();

    let opts = PkgbuildOptions {
        project_dir: &dir,
        bundle_meta: vec!["Cargo.toml#[package, metadata, bundle-stable]".to_string()],
        bin_name: None,
        desktop_template: None,
        icon_name: None,
        depends: None,
        makedepends: None,
        arch: None,
        url: None,
        license: None,
        prefix: None,
        profile: None,
        prebuilt: false,
        bin_path: None,
    };
    let (pkgbuild, desktop) = generate_pkgbuild(&opts).unwrap();

    // 身份来自 bundle 表：pkgname 取 crate 名、desktop 名取 identifier
    assert!(pkgbuild.contains("pkgname=aacode"));
    assert!(pkgbuild.contains("pkgver=1.23.2"));
    assert!(pkgbuild.contains("license=(Apache-2.0)"));
    // cargo 包名从 Cargo.toml 推导，不需要再传
    assert!(pkgbuild.contains("cargo build --release -p aacode"));
    // 二进制路径必须向 cargo 问 target_directory（workspace 根的 target/），
    // 不能硬编码 crate 目录——多 crate workspace 下那样必挂
    assert!(
        pkgbuild.contains("cargo metadata --no-deps --format-version 1"),
        "package() 应向 cargo 询问 target_directory：{pkgbuild}"
    );
    assert!(
        !pkgbuild.contains("$_srcdir/target/release/"),
        "不该硬编码产物在 crate 目录的 target/ 下：{pkgbuild}"
    );
    // build() 必须卸掉 makepkg 注入的编译器环境变量：cc-rs 把它们追加在
    // C 编译命令行末尾，会覆盖 aws-lc-sys 等 crate 为个别文件设的 -O0
    // （jitterentropy-base.c 禁优化，被 CFLAGS 的 -O2 覆盖后 #error 挂死）
    assert!(
        pkgbuild.contains("unset CFLAGS CXXFLAGS CPPFLAGS LDFLAGS DEBUG_RUSTFLAGS"),
        "build() 应卸掉 makepkg 的编译器环境变量（含 DEBUG_RUSTFLAGS——\
             OPTIONS=(debug) 时 rust.sh 会往 RUSTFLAGS 追加 -C debuginfo=2）：{pkgbuild}"
    );
    assert!(
        !pkgbuild.contains("install -Dm644") || !pkgbuild.contains("hicolor"),
        "没配 icon 时不该装图标：{pkgbuild}"
    );
    assert!(!pkgbuild.contains("url="), "未给 url 时不该输出空字段");
    // 未声明 depends → 整行不写（对齐 cargo-bundle 的 deb 行为）
    // 注意按行判断：makedepends= 里也含子串 "depends="
    assert!(
        !pkgbuild.lines().any(|l| l.starts_with("depends=")),
        "没声明依赖时不该写 depends= 行：{pkgbuild}"
    );
    assert!(
        pkgbuild.lines().any(|l| l == "makedepends=(cargo rust)"),
        "构建 Rust crate 必然需要 cargo/rust，这不算猜：{pkgbuild}"
    );
    // 没配 desktop_template → 由 aa-bundle 自动生成（不再是不产出）
    let (dname, dcontent) = desktop.expect("无模板时应自动生成 .desktop");
    assert_eq!(dname, "dev.aa.AaCode.desktop");
    assert!(dcontent.contains("Name=AaCode"), "{dcontent}");
    // 源码路径绝对化
    let src = dir.canonicalize().unwrap();
    assert!(
        pkgbuild.contains(&format!("_srcdir=\"{}\"", src.display())),
        "_srcdir 应为绝对路径：{pkgbuild}"
    );
}

/// 测试共用的 Options 构造：默认源码模式。prebuilt / bin_path 见
/// [`opts_with`]。
fn opts<'a>(dir: &'a Path, profile: Option<&'a str>) -> PkgbuildOptions<'a> {
    opts_with(dir, profile, false, None)
}

fn opts_with<'a>(
    dir: &'a Path,
    profile: Option<&'a str>,
    prebuilt: bool,
    bin_path: Option<&'a Path>,
) -> PkgbuildOptions<'a> {
    PkgbuildOptions {
        project_dir: dir,
        bundle_meta: vec!["Cargo.toml#[package, metadata, bundle-stable]".to_string()],
        bin_name: None,
        desktop_template: None,
        icon_name: None,
        depends: None,
        makedepends: None,
        arch: None,
        url: None,
        license: None,
        prefix: None,
        profile,
        prebuilt,
        bin_path,
    }
}

/// `--profile` 决定 PKGBUILD 的构建命令与产物目录：非 release 走
/// `--profile <name>` + `target/<name>/<bin>`；`dev` 映射到 cargo 的 `debug`。
#[test]
fn pkgbuild_profile_flag_controls_build_and_artifact_dir() {
    let dir = test_dir("profile");
    std::fs::write(
        dir.join("Cargo.toml"),
        r#"
[package]
name = "aacode"
version = "1.23.2"
license = "Apache-2.0"

[[bin]]
name = "aacode"

[package.metadata.bundle-stable]
identifier = "dev.aa.AaCode"
name = "AaCode"
"#,
    )
    .unwrap();

    let (pkgbuild, _) = generate_pkgbuild(&opts(&dir, Some("release-quick"))).unwrap();
    assert!(
        pkgbuild.contains("cargo build --profile release-quick -p aacode"),
        "{pkgbuild}"
    );
    assert!(
        pkgbuild.contains("$_target_dir/release-quick/aacode"),
        "{pkgbuild}"
    );

    let (pkgbuild, _) = generate_pkgbuild(&opts(&dir, Some("dev"))).unwrap();
    assert!(
        pkgbuild.contains("cargo build --profile dev -p aacode"),
        "{pkgbuild}"
    );
    assert!(pkgbuild.contains("$_target_dir/debug/aacode"), "{pkgbuild}");
}

#[test]
fn pkgbuild_settings_come_from_bundle_arch_subtable() {
    let dir = test_dir("arch_subtable");
    std::fs::write(
        dir.join("Cargo.toml"),
        r#"
[package]
name = "aacode"
version = "1.23.2"
license = "Apache-2.0"

[[bin]]
name = "aacode"

[package.metadata.bundle-stable]
identifier = "dev.aa.AaCode"
name = "AaCode"
short_description = "A coding agent built with Rust and GPUI"
desktop_template = "resources/aacode.desktop.in"

# Arch 打包参数作为 bundle 表的子表（方案 B：与 cargo-bundle 的 linux/osx 子表同构）
[package.metadata.bundle-stable.arch]
depends = ["gcc-libs", "glibc", "gtk3"]
"#,
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("resources")).unwrap();
    std::fs::write(
        dir.join("resources/aacode.desktop.in"),
        "Name=$APP_NAME\nExec=$APP_CLI %F\n",
    )
    .unwrap();

    let opts = PkgbuildOptions {
        project_dir: &dir,
        bundle_meta: vec!["Cargo.toml#[package, metadata, bundle-stable]".to_string()],
        bin_name: None,
        desktop_template: None,
        icon_name: None,
        depends: None,
        makedepends: None,
        arch: None,
        url: None,
        license: None,
        prefix: None,
        profile: None,
        prebuilt: false,
        bin_path: None,
    };
    let (pkgbuild, desktop) = generate_pkgbuild(&opts).unwrap();

    assert!(
        pkgbuild.contains("depends=(gcc-libs glibc gtk3)"),
        "依赖应来自 [bundle.arch] 子表而非命令行：{pkgbuild}"
    );
    // 默认 makedepends（子表未声明）
    assert!(pkgbuild.contains("makedepends=(cargo rust)"));
    // short_description 优先于 Cargo.toml 的 description
    assert!(
        pkgbuild.contains("pkgdesc=\"A coding agent built with Rust and GPUI\""),
        "pkgdesc 应取 short_description：{pkgbuild}"
    );
    // desktop_template 生效：产出 desktop 且渲染过
    let (name, content) = desktop.expect("配了 desktop_template 就应产出 desktop");
    assert_eq!(name, "dev.aa.AaCode.desktop");
    assert!(content.contains("Name=AaCode"));
    assert!(content.contains("Exec=/usr/bin/aacode %F"));
}

#[test]
fn bin_name_comes_from_bin_target_not_crate_name() {
    // [[bin]].name 与 package.name 不同：要用真实的 bin 目标，不能拿 crate 名顶替
    let dir = test_dir("bin_differs");
    std::fs::write(
        dir.join("Cargo.toml"),
        r#"
[package]
name = "some_crate"
version = "0.1.0"
license = "Apache-2.0"

[[bin]]
name = "the_real_binary"

[package.metadata.bundle-stable]
identifier = "dev.aa.AaCode"
name = "AaCode"
"#,
    )
    .unwrap();

    let opts = PkgbuildOptions {
        project_dir: &dir,
        bundle_meta: vec!["Cargo.toml#[package, metadata, bundle-stable]".to_string()],
        bin_name: None,
        desktop_template: None,
        icon_name: None,
        depends: None,
        makedepends: None,
        arch: None,
        url: None,
        license: None,
        prefix: None,
        profile: None,
        prebuilt: false,
        bin_path: None,
    };
    let (pkgbuild, _) = generate_pkgbuild(&opts).unwrap();
    assert!(
        pkgbuild.contains("$pkgdir/usr/bin/the_real_binary"),
        "应装真实的 [[bin]] 名，而不是 crate 名：{pkgbuild}"
    );
    // 但 cargo -p 用的是 package.name
    assert!(pkgbuild.contains("cargo build --release -p some_crate"));
}

#[test]
fn default_out_dir_lives_in_workspace_target() {
    let dir = test_dir("default_out");
    std::fs::write(
        dir.join("Cargo.toml"),
        r#"
[package]
name = "aacode"
version = "1.23.2"
license = "Apache-2.0"

[package.metadata.bundle-stable]
identifier = "dev.aa.AaCode"
name = "AaCode"
"#,
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();

    // 独立 crate（无 workspace）：cargo metadata 问出的 target 就是 <dir>/target，
    // 默认输出在其下的 aa-bundle/pkgbuild/<bin>/——不能猜在 crate 目录乱放，
    // 也不能不给 bin 名分目录（同一 workspace 打多个 crate 会互相覆盖）。
    let out = default_out_dir(&dir, None).unwrap();
    assert_eq!(
        out,
        dir.join("target").join("aa-bundle/pkgbuild/aacode"),
        "默认输出应为 <workspace target>/aa-bundle/pkgbuild/<bin>：{}",
        out.display()
    );
}

#[test]
fn multiple_bins_must_be_declared() {
    let dir = test_dir("multi_bin");
    std::fs::write(
        dir.join("Cargo.toml"),
        r#"
[package]
name = "aacode"
version = "1.23.2"
license = "Apache-2.0"

[[bin]]
name = "aacode"

[[bin]]
name = "aacode-cli"

[package.metadata.bundle-stable]
identifier = "dev.aa.AaCode"
name = "AaCode"
"#,
    )
    .unwrap();

    let opts = PkgbuildOptions {
        project_dir: &dir,
        bundle_meta: vec!["Cargo.toml#[package, metadata, bundle-stable]".to_string()],
        bin_name: None,
        desktop_template: None,
        icon_name: None,
        depends: None,
        makedepends: None,
        arch: None,
        url: None,
        license: None,
        prefix: None,
        profile: None,
        prebuilt: false,
        bin_path: None,
    };
    let err = generate_pkgbuild(&opts).unwrap_err().to_string();
    assert!(
        err.contains("2 个 [[bin]]"),
        "多个 bin 时不应猜，应要求声明：{err}"
    );
}

#[test]
fn missing_arch_subtable_falls_back_to_defaults() {
    let dir = test_dir("arch_defaults");
    std::fs::write(
        dir.join("Cargo.toml"),
        r#"
[package]
name = "aacode"
version = "1.23.2"
license = "Apache-2.0"

[[bin]]
name = "aacode"

[package.metadata.bundle-stable]
identifier = "dev.aa.AaCode"
name = "AaCode"
"#,
    )
    .unwrap();

    // 没有 [bundle.arch] 子表 → 走默认值
    let identity =
        crate::meta::load_bundle_meta("Cargo.toml#[package, metadata, bundle-stable]", &dir)
            .unwrap();
    let settings = arch_settings_from(&identity.arch, false);
    assert_eq!(settings.makedepends, vec!["cargo", "rust"]);
    assert_eq!(settings.prefix, "/usr");
    // 与 cargo-bundle 一致：不声明就不给依赖（而不是塞一份猜的基线）
    assert!(
        settings.depends.is_empty(),
        "默认依赖应为空：{:?}",
        settings.depends
    );
    // 未知字段被忽略（不像 cargo-bundle 那样 deny_unknown_fields）
    assert!(identity.desktop_template.is_none());
    // prebuilt 且未显式声明 → 清空：包内不 cargo build 也不 cargo metadata，
    // (cargo rust) 没有意义
    let prebuilt_settings = arch_settings_from(&identity.arch, true);
    assert!(
        prebuilt_settings.makedepends.is_empty(),
        "prebuilt 模式默认 makedepends 应为空：{:?}",
        prebuilt_settings.makedepends
    );
    // depends / prefix 等其余默认值不随模式变
    assert!(prebuilt_settings.depends.is_empty());
    assert_eq!(prebuilt_settings.prefix, "/usr");
}

#[test]
fn icon_list_drives_installation() {
    let dir = test_dir("icon_list");
    std::fs::write(
        dir.join("Cargo.toml"),
        r#"
[package]
name = "aacode"
version = "1.23.2"
license = "Apache-2.0"

[[bin]]
name = "aacode"

[package.metadata.bundle-stable]
identifier = "dev.aa.AaCode"
name = "AaCode"
icon = ["resources/app-icon.png", "resources/app-icon@2x.png"]

[package.metadata.bundle-stable.arch]
icon_dir = "share/icons/hicolor"
"#,
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("resources")).unwrap();
    std::fs::write(dir.join("resources/app-icon.png"), "small").unwrap();
    std::fs::write(dir.join("resources/app-icon@2x.png"), "x".repeat(2048)).unwrap();

    let opts = PkgbuildOptions {
        project_dir: &dir,
        bundle_meta: vec!["Cargo.toml#[package, metadata, bundle-stable]".to_string()],
        bin_name: None,
        desktop_template: None,
        icon_name: None,
        depends: None,
        makedepends: None,
        arch: None,
        url: None,
        license: None,
        prefix: None,
        profile: None,
        prebuilt: false,
        bin_path: None,
    };
    let (pkgbuild, _) = generate_pkgbuild(&opts).unwrap();
    // 两个图标都装，路径来自 icon 列表而非硬编码；目录来自 [bundle.arch].icon_dir
    assert!(
        pkgbuild.contains("share/icons/hicolor/512x512/apps/aacode.png"),
        "512 图标应按 icon 列表安装：{pkgbuild}"
    );
    assert!(
        pkgbuild.contains("share/icons/hicolor/1024x1024/apps/aacode.png"),
        "1024 图标应按 icon 列表安装：{pkgbuild}"
    );
    assert!(
        !pkgbuild.contains("$startdir_icon"),
        "图标引用应走 $_srcdir（曾因定义/引用变量名不一致导致空路径）：{pkgbuild}"
    );
    assert!(
        pkgbuild.contains("$_srcdir/resources/app-icon.png"),
        "图标路径应来自 icon 列表：{pkgbuild}"
    );
}

#[test]
fn bin_name_falls_back_to_package_name_when_no_bin_section() {
    // cargo 的规则：没有 [[bin]] 时 src/main.rs 自动成为以包名命名的二进制。
    // aacode 正是这种（无 [[bin]] 段），此前误报"没有 [[bin]] 目标"。
    let dir = test_dir("no_bin_section");
    std::fs::write(
        dir.join("Cargo.toml"),
        r#"
[package]
name = "aacode"
version = "1.23.2"
description = "A coding agent built with Rust and GPUI"
license = "Apache-2.0"

[package.metadata.bundle-stable]
identifier = "dev.aa.AaCode"
name = "AaCode"
"#,
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();

    let opts = PkgbuildOptions {
        project_dir: &dir,
        bundle_meta: vec!["Cargo.toml#[package, metadata, bundle-stable]".to_string()],
        bin_name: None,
        desktop_template: None,
        icon_name: None,
        depends: None,
        makedepends: None,
        arch: None,
        url: None,
        license: None,
        prefix: None,
        profile: None,
        prebuilt: false,
        bin_path: None,
    };
    let (pkgbuild, desktop) = generate_pkgbuild(&opts).unwrap();
    assert!(
        pkgbuild.contains("$pkgdir/usr/bin/aacode"),
        "无 [[bin]] 时应回退包名：{pkgbuild}"
    );
    assert!(pkgbuild.contains("cargo build --release -p aacode"));
    // 无 desktop_template → 自动生成 .desktop
    let (name, content) = desktop.expect("应自动生成 .desktop");
    assert_eq!(name, "dev.aa.AaCode.desktop");
    assert!(content.contains("Name=AaCode"), "{content}");
    assert!(content.contains("Exec=aacode"), "{content}");
    assert!(content.contains("Icon=aacode"), "{content}");
    assert!(content.contains("Terminal=false"), "{content}");
    // Comment 回退链：short_description 缺省 → Cargo.toml 的 [package].description
    assert!(
        content.contains("Comment=A coding agent built with Rust and GPUI"),
        "Comment 应回退到 package.description：{content}"
    );
    // 没配 category → 回退 Utility;
    assert!(content.contains("Categories=Utility;"), "{content}");
    assert!(
        content.contains("StartupWMClass=dev.aa.AaCode"),
        "{content}"
    );
}

#[test]
fn desktop_entry_uses_cargo_bundle_category_mapping() {
    // cargo-bundle 的 category 用 macOS LSApplicationCategoryType 体系的值
    let meta = crate::meta::BundleMeta {
        name: Some("AaCode".to_string()),
        short_description: Some("A coding agent".to_string()),
        category: Some("Developer Tool".to_string()),
        linux: Some(crate::meta::LinuxTable {
            mime_types: Some(vec!["text/plain".to_string()]),
            exec_args: Some("%F".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    // 图标安装名与 bin 名不同时，Icon= 必须用安装名（曾写死 bin 名的 bug）
    let out = crate::desktop::render_desktop_entry(&meta, "aacode", "my_icon", "dev.aa.AaCode", "");
    // "Developer Tool" → Development;（cargo-bundle category.rs:134）
    assert!(out.contains("Categories=Development;"), "{out}");
    assert!(out.contains("Comment=A coding agent"), "{out}");
    assert!(out.contains("Exec=aacode %F"), "{out}");
    assert!(out.contains("Icon=my_icon"), "{out}");
    assert!(out.contains("MimeType=text/plain;"), "{out}");
    // 刻意不写的键
    assert!(
        !out.contains("Version="),
        "Version 是 spec 版本不是 app 版本"
    );
    assert!(!out.contains("Encoding="), "Encoding 已废弃");
}

#[test]
fn desktop_rendering_substitutes_only_known_vars() {
    let out = crate::desktop::render_template(
        "Name=$APP_NAME\nExec=$APP_CLI %F\nIcon=$APP_ICON\nX=$UNKNOWN",
        &[
            ("APP_NAME", "AaCode"),
            ("APP_CLI", "/usr/bin/aacode"),
            ("APP_ICON", "aacode"),
        ],
    );
    assert!(out.contains("Name=AaCode"));
    assert!(out.contains("Exec=/usr/bin/aacode %F"));
    assert!(out.contains("Icon=aacode"));
    assert!(out.contains("X=$UNKNOWN"), "未提供的变量应原样保留");
}

/// prebuilt 模式测试共用的 fixture：临时目录 + 假 crate + 假产物
/// `target/release-quick/aacode`（targz 测试同款：写个非 ELF 文本即可，
/// 生成期只查存在性）。
fn prebuilt_fixture(tag: &str) -> (PathBuf, PathBuf) {
    let dir = test_dir(tag);
    std::fs::write(
        dir.join("Cargo.toml"),
        r#"
[package]
name = "aacode"
version = "1.23.2"
license = "Apache-2.0"

[[bin]]
name = "aacode"

[package.metadata.bundle-stable]
identifier = "dev.aa.AaCode"
name = "AaCode"
"#,
    )
    .unwrap();
    // resolve_target_dir 走 cargo metadata：临时目录里也能问出 target_directory
    // （= <临时目录>/target），所以假产物放这里就能被默认路径定位到。
    let bin = dir.join("target/release-quick/aacode");
    std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
    std::fs::write(&bin, "not a real elf").unwrap();
    (dir, bin)
}

#[test]
fn prebuilt_mode_installs_without_building() {
    let (dir, bin) = prebuilt_fixture("prebuilt_basic");
    let (pkgbuild, _) =
        generate_pkgbuild(&opts_with(&dir, Some("release-quick"), true, None)).unwrap();

    // build() 不编译：命令形态的 cargo build 不出现（echo 提示里的字符串不算）
    assert!(
        !pkgbuild.contains("\n  cargo build"),
        "prebuilt 的 build() 不该现场编译：{pkgbuild}"
    );
    // build() 只做存在性校验
    assert!(
        pkgbuild.contains("[ ! -x"),
        "prebuilt 的 build() 应校验钉死的二进制：{pkgbuild}"
    );
    // package() 直接 install 生成期钉死的绝对路径，不调 cargo metadata
    let pinned = bin.canonicalize().unwrap();
    assert!(
        pkgbuild.contains(&format!("install -Dm755 \"{}\"", pinned.display())),
        "prebuilt 应直接 install 钉死路径：{pkgbuild}"
    );
    assert!(
        !pkgbuild.contains("cargo metadata"),
        "prebuilt 的 package() 不该调 cargo metadata：{pkgbuild}"
    );
    // 包内不调 cargo → makedepends 整行不写
    assert!(
        !pkgbuild.contains("makedepends="),
        "prebuilt 默认 makedepends 应为空：{pkgbuild}"
    );
    // 头部注释写明「重编后需重新生成」
    assert!(
        pkgbuild.contains("重新构建后必须重新运行 aa-bundle --prebuilt"),
        "{pkgbuild}"
    );
    // 两模式共用的 unset 行（含 DEBUG_RUSTFLAGS）
    assert!(pkgbuild.contains("unset CFLAGS CXXFLAGS CPPFLAGS LDFLAGS DEBUG_RUSTFLAGS"));
}

#[test]
fn prebuilt_mode_requires_existing_binary() {
    let dir = test_dir("prebuilt_missing");
    std::fs::write(
        dir.join("Cargo.toml"),
        r#"
[package]
name = "aacode"
version = "1.23.2"
license = "Apache-2.0"

[[bin]]
name = "aacode"

[package.metadata.bundle-stable]
identifier = "dev.aa.AaCode"
name = "AaCode"
"#,
    )
    .unwrap();
    // 不建 target/release-quick/aacode → 生成期就报错（不留给 makepkg）
    let err = generate_pkgbuild(&opts_with(&dir, Some("release-quick"), true, None))
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("cargo build --profile release-quick -p aacode"),
        "错误消息应告诉用户先构建什么：{err}"
    );
    assert!(
        err.contains("prebuilt 模式不构建"),
        "错误消息应说明 prebuilt 语义：{err}"
    );
}

#[test]
fn prebuilt_bin_path_override_is_pinned() {
    let (dir, _default_bin) = prebuilt_fixture("prebuilt_override");
    // 覆盖路径指向别处的二进制（canonicalize 后钉死）
    let elsewhere = dir.join("elsewhere/aacode");
    std::fs::create_dir_all(elsewhere.parent().unwrap()).unwrap();
    std::fs::write(&elsewhere, "not a real elf either").unwrap();
    let (pkgbuild, _) = generate_pkgbuild(&opts_with(
        &dir,
        Some("release-quick"),
        true,
        Some(&elsewhere),
    ))
    .unwrap();
    let pinned = elsewhere.canonicalize().unwrap();
    assert!(
        pkgbuild.contains(&format!("install -Dm755 \"{}\"", pinned.display())),
        "--bin-path 覆盖应被钉死进 PKGBUILD：{pkgbuild}"
    );
    // 覆盖路径不存在 → 生成期报错
    let missing = dir.join("elsewhere/missing");
    let err = generate_pkgbuild(&opts_with(
        &dir,
        Some("release-quick"),
        true,
        Some(&missing),
    ))
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("--bin-path"),
        "应指出是 --bin-path 的问题：{err}"
    );
}

#[test]
fn prebuilt_makedepends_explicit_still_respected() {
    let dir = test_dir("prebuilt_makedepends");
    std::fs::write(
        dir.join("Cargo.toml"),
        r#"
[package]
name = "aacode"
version = "1.23.2"
license = "Apache-2.0"

[[bin]]
name = "aacode"

[package.metadata.bundle-stable]
identifier = "dev.aa.AaCode"
name = "AaCode"

[package.metadata.bundle-stable.arch]
makedepends = ["foo", "bar"]
"#,
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("target/release-quick")).unwrap();
    std::fs::write(dir.join("target/release-quick/aacode"), "not a real elf").unwrap();
    let (pkgbuild, _) =
        generate_pkgbuild(&opts_with(&dir, Some("release-quick"), true, None)).unwrap();
    assert!(
        pkgbuild.contains("makedepends=(foo bar)"),
        "显式声明的 makedepends 在 prebuilt 下仍应保留：{pkgbuild}"
    );
}
