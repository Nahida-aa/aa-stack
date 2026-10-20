//! 本 crate 的 prelude。对齐 zed `crates/ui/src/prelude.rs`，但只包含 fork
//! 当前实际存在的部分（components 只搬运了 button/icon/indicator/keybinding/
//! label/stack/tooltip）。
//!
//! 用法与 zed 一致：`use aa_ui_zed::prelude::*;` 一次拿全。

pub use gpui::prelude::*;
pub use gpui::{
    AbsoluteLength, AnyElement, App, Context, DefiniteLength, Div, Element, ElementId,
    InteractiveElement, ParentElement, Pixels, Rems, RenderOnce, SharedString, Styled, Window, div,
    px, relative, rems,
};

pub use aa_theme_gpui::ActiveTheme;

pub use crate::styles::{PlatformStyle, Severity, StyledTypography, TextSize};
pub use crate::traits::clickable::*;
pub use crate::traits::disableable::*;
pub use crate::traits::fixed::*;
pub use crate::traits::styled_ext::*;
pub use crate::traits::toggleable::*;
pub use crate::traits::transformable::*;
pub use crate::traits::visible_on_hover::*;
pub use crate::{Button, ButtonCommon, ButtonStyle, TintColor};
pub use crate::{Color, Icon, IconName, IconSize};
pub use crate::{Label, LabelCommon, LabelSize, LineHeightStyle};
pub use crate::{h_flex, v_flex};
