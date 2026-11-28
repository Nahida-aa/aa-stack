//! 图标按钮 —— 对应 Zed 的 `ui::IconButton`（Square 形态 + `ButtonStyle::Subtle`）。
//!
//! Subtle 的三态照搬 `ButtonLike`：常态 `ghost_element_background`（近乎透明）、
//! 悬停 `ghost_element_hover`、按下 `ghost_element_active`，图标色恒为文本色。
//! Square 形态的按钮盒 = 图标边长（上游 `icon_size.square(window, cx)`）——
//! 命中区确实只有这么大，上游如此。

use aa_icons::IconName;
use aa_theme_iced::Theme as AppTheme;
use iced::widget::{button, svg};
use iced::{Background, Border, Element, Length};

use super::icon::IconSize;

/// 一个图标按钮。
pub struct IconButton<Message> {
    icon: IconName,
    size: IconSize,
    on_press: Option<Message>,
}

impl<Message: Clone> IconButton<Message> {
    /// 新建一个图标按钮；`on_press` 为 `None` 时按钮可画但点不动（置灰的场合
    /// 由调用方把图标色调暗——上游禁用态走 `icon_disabled`，见 `Icon` 文档）。
    pub fn new(icon: IconName, on_press: impl Into<Option<Message>>) -> Self {
        Self {
            icon,
            size: IconSize::default(),
            on_press: on_press.into(),
        }
    }

    /// 指定图标尺寸。
    pub fn icon_size(mut self, size: IconSize) -> Self {
        self.size = size;
        self
    }

    /// 渲染成元素；`theme` 用于取色。
    pub fn into_element<'a>(self, theme: &'a AppTheme) -> Element<'a, Message>
    where
        Message: Clone + 'a,
    {
        let colors = theme.colors();
        let handle =
            svg::Handle::from_memory(self.icon.svg_bytes().expect("图标未随 aa_icons 内嵌"));
        let px = self.size.pixels();

        button(
            svg::Svg::new(handle)
                .width(Length::Fixed(px))
                .height(Length::Fixed(px))
                .style(move |_, _| svg::Style {
                    color: Some(colors.text),
                }),
        )
        .width(Length::Fixed(px))
        .height(Length::Fixed(px))
        .padding(0)
        .on_press_maybe(self.on_press)
        .style(move |_, status| {
            let background = match status {
                button::Status::Hovered => Some(colors.ghost_element_hover),
                button::Status::Pressed => Some(colors.ghost_element_active),
                _ => None,
            };
            button::Style {
                background: background.map(Background::Color),
                text_color: colors.text,
                border: Border::default(),
                ..button::Style::default()
            }
        })
        .into()
    }
}
