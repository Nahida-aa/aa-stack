//! 图标名枚举 —— 变体清单由 `build.rs` 扫描 `assets/icons/` 自动生成
//! （gpui-kit 同款做法：SVG 文件名是唯一事实源，加图标 = 放文件重编译）。
//!
//! strum/serde 的 derive 在生成代码里原样保留：`IntoEnumIterator`（aa_ui_zed
//! 的图标全集展示页）、`EnumString`（资产目录反解析）、`IntoStaticStr` 都有
//! 消费者，不能丢。序列化行为也不变（默认按变体名 PascalCase 输出）。

include!(concat!(env!("OUT_DIR"), "/icon_name.rs"));

impl IconName {
    /// 返回内嵌在本 crate 里的 SVG 字节。
    ///
    /// gpui 侧走应用自己的 AssetSource 运行时加载（`aa_assets_core` 的
    /// RustEmbed 已经把整套图标嵌进去了）；iced 侧没有资产源可用，只能吃
    /// 编译期内嵌的字节。只有窗口控制的四枚走内嵌 —— iced 侧目前也只有它们
    /// 用得到，不把整套 298 个 SVG 再嵌一遍占二进制体积。
    pub fn svg_bytes(self) -> Option<&'static [u8]> {
        match self {
            IconName::GenericMinimize => Some(include_bytes!(
                "../../../../assets/icons/generic_minimize.svg"
            )),
            IconName::GenericMaximize => Some(include_bytes!(
                "../../../../assets/icons/generic_maximize.svg"
            )),
            IconName::GenericRestore => Some(include_bytes!(
                "../../../../assets/icons/generic_restore.svg"
            )),
            IconName::GenericClose => {
                Some(include_bytes!("../../../../assets/icons/generic_close.svg"))
            }
            IconName::Menu => Some(include_bytes!("../../../../assets/icons/menu.svg")),
            IconName::ChevronDown => {
                Some(include_bytes!("../../../../assets/icons/chevron_down.svg"))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use strum::{IntoEnumIterator as _, ParseError};

    use crate::IconName;

    /// 资产根：`CARGO_MANIFEST_DIR`（`packages/ui/icons`）上跳三层到仓库根。
    fn asset_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../assets")
    }

    /// 双向保障之一：每个枚举变体都有对应的 SVG 文件（`path()` 生成的路径
    /// 一定能命中真实文件）。
    #[test]
    fn test_all_icons_exist() {
        for icon in IconName::iter() {
            let icon_path = asset_path().join(&*icon.path());
            assert!(
                icon_path.exists(),
                "Icon {icon:?} does not exist at {icon_path:?}",
            );
        }
    }

    /// 双向保障之二：资产目录里出现的每个 SVG 文件名都能解析回枚举 ——
    /// 防止放了文件却忘了建变体。
    #[test]
    fn test_no_dangling_icons() -> Result<(), ParseError> {
        let icons_dir = asset_path().join("icons");

        for entry in std::fs::read_dir(&icons_dir).expect("failed to read icons directory") {
            let path = entry.expect("failed to read icons directory entry").path();
            if path.extension().is_none_or(|extension| extension != "svg") {
                continue;
            }
            let file_stem = path
                .file_stem()
                .and_then(|file_stem| file_stem.to_str())
                .expect("icon file name is not valid UTF-8");

            file_stem.parse::<IconName>()?;
        }

        Ok(())
    }
}
