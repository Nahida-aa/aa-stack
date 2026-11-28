//! 图标组件 —— 对应 Zed 的 `ui::Icon`。
//!
//! 图形来自 [`aa_icons`]（与 gpui 侧同一套 SVG），着色跟随主题的文本色令牌。
//! iced 没有资产源，字节经 [`aa_icons::IconName::svg_bytes`] 编译期内嵌。

use aa_icons::IconName;
use aa_theme_iced::Theme as AppTheme;
use iced::widget::svg;
use iced::{Element, Length};

/// 图标尺寸 —— 与 zed `ui::IconSize` 同名同值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IconSize {
    /// 10px
    Indicator,
    /// 12px
    XSmall,
    /// 14px
    Small,
    /// 16px（默认）
    #[default]
    Medium,
    /// 48px
    XLarge,
}

impl IconSize {
    /// 图标的像素边长。
    pub fn pixels(self) -> f32 {
        match self {
            Self::Indicator => 10.0,
            Self::XSmall => 12.0,
            Self::Small => 14.0,
            Self::Medium => 16.0,
            Self::XLarge => 48.0,
        }
    }
}

/// 一个图标（SVG，着色跟随文本色令牌）。
///
/// 与上游同款构建方式：`Icon::new(IconName::Menu).size(IconSize::Small)`，
/// 渲染时递入主题取色。
pub struct Icon {
    icon: IconName,
    size: IconSize,
}

impl Icon {
    /// 新建一个图标。
    pub fn new(icon: IconName) -> Self {
        Self {
            icon,
            size: IconSize::default(),
        }
    }

    /// 指定尺寸。
    pub fn size(mut self, size: IconSize) -> Self {
        self.size = size;
        self
    }

    /// 渲染成元素；`theme` 用于取文本色令牌。
    ///
    /// 上游的图标色走 `Color::Default`（= 文本色），这里同一来源；svg 的
    /// `color` 过滤器保 alpha 换 RGB，SVG 里写死的笔画色会被整体替换。
    pub fn into_element<'a, Message: Clone + 'a>(
        self,
        theme: &'a AppTheme,
    ) -> Element<'a, Message> {
        let colors = theme.colors();
        let handle =
            svg::Handle::from_memory(self.icon.svg_bytes().expect("图标未随 aa_icons 内嵌"));
        svg::Svg::new(handle)
            .width(Length::Fixed(self.size.pixels()))
            .height(Length::Fixed(self.size.pixels()))
            .style(move |_, _| svg::Style {
                color: Some(colors.text),
            })
            .into()
    }
}
