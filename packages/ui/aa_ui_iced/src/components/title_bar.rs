//! 标题栏的「窗口框架」部分 —— 对应 Zed 的 `crates/platform_title_bar`。
//!
//! Zed 把这个 crate 分成两块：
//!
//! - [`PlatformTitleBar`](https://github.com/zed-industries/zed)：窗口框架本身 ——
//!   高度、底色（失焦变暗）、`WindowControlArea::Drag` 拖拽区、最小化/最大化/关闭
//!   按钮的摆位。按钮本身在 `platforms/{linux,windows}.rs` 里。
//! - `crates/title_bar`：标题栏**内容**（File/Edit/… 菜单、项目名、分支、协作者、
//!   用户菜单）。那是各应用自己拼的，不属于本组件。
//!
//! 本文件是前者的 iced 版：一条可拖拽的横条 + 两端的插槽（`left` / `right`）+ 窗口
//! 按钮。内容由宿主（aa_term 的 `title_bar` crate）填。
//!
//! **需要配合窗口设置**：iced 默认让窗口管理器画装饰（SSD），那就别用本组件；要让
//! 它有用，宿主得 `window::Settings { decorations: false, .. }` —— 也就是 CSD。
//! iced 拿不到「当前是 SSD 还是 CSD」（zed 用 `window.window_decorations()`），所以
//! 画不画窗口按钮由宿主决定（[`TitleBar::window_controls`]）。
//!
//! **未搬**：
//!
//! - **圆角与 CSD 边框**（zed：`CLIENT_SIDE_DECORATION_ROUNDING` 10px + 1px 边框，
//!   配合透明窗口）。iced 要圆角就得开 `transparent`，那要求合成器；裸 X11（无合成
//!   器）下会变成黑角，所以先不做。
//! - **按窗口管理器布局摆按钮**（zed：`cx.button_layout()`，Ubuntu 上关闭键在左）。
//!   iced 没有读 WM 布局的接口，统一摆右边（GNOME/Adwaita 的默认）。
//! - **最大化/还原图标切换**（zed 的 `Restore` 变体）：要 `window::is_maximized`
//!   异步查一次、再订阅变化，先不做，固定画最大化图标。
//! - 系统窗口标签页（`system_window_tabs.rs`）、Mac 红绿灯（`TRAFFIC_LIGHT_PADDING`）、
//!   双击标题栏最大化（zed 只有 Mac 与 Linux 有，iced 侧等真有需求再说）。

use aa_theme_iced::Theme as AppTheme;
use iced::widget::svg;
use iced::widget::{Row, button, container, mouse_area, row, space};
use iced::{Alignment, Background, Border, Element, Fill, Length, Padding};

/// 标题栏的最小高度（zed `platform_title_bar_height` 的下限）。
pub const TITLE_BAR_MIN_HEIGHT: f32 = 34.0;

/// 窗口按钮的边长（zed `LinuxWindowControls` 的 `w_5()`/`h_5()`，20px）。
///
/// 上游 Linux 的窗口按钮是 20×20 的圆形悬停块，不是 Windows 版那种
/// 36px 宽、占满标题栏高的整条 —— iced 版此前抄的是 Windows 规格，与
/// 同机跑的 Zed/aa-zed 观感对不上，现已统一到 Linux 规格。
pub const CAPTION_BUTTON_WIDTH: f32 = 20.0;

/// 窗口按钮里图标的边长（aa-zed 侧 `svg().size_4()`，即 16px）。
pub const CAPTION_ICON_SIZE: f32 = 16.0;

/// 按钮组内的间距（zed `LinuxWindowControls` 的 `gap_3()`，12px）。
pub const CAPTION_GROUP_SPACING: f32 = 12.0;

/// 按钮组两侧的内边距（zed `LinuxWindowControls` 的 `px_3()`，12px）。
pub const CAPTION_GROUP_PADDING: f32 = 12.0;

/// 左侧内容的内边距（zed 标题栏的 `pl_2()`）。
pub const TITLE_BAR_LEFT_PADDING: f32 = 8.0;

/// 平台标题栏高度。
///
/// 与 zed 的 `platform_title_bar_height` 同公式：`1.75 × rem`，下限 34px（Windows
/// 上上游固定 32，这里不做平台分支 —— 只有一条 Linux 路径）。
///
/// iced 没有 gpui 的 `window.rem_size()`，拿界面的基准字号（iced 默认 16px）当 rem：
/// `text_size` 就传它。
pub fn platform_title_bar_height(text_size: f32) -> f32 {
    (1.75 * text_size).max(TITLE_BAR_MIN_HEIGHT)
}

/// 窗口按钮。对应 zed 的 `WindowsCaptionButton`（少了 `Restore`，见模块头）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowControl {
    /// 最小化。
    Minimize,
    /// 最大化。
    Maximize,
    /// 关闭。
    Close,
}

impl WindowControl {
    /// 按钮上的图标 —— aa-zed 侧 `WindowControlType::icon()` 的同款
    /// （`aa_icons` 的 `Generic*` SVG，上游是 Segoe MDL2 图标字体，Linux 上没有）。
    pub fn icon(self) -> aa_icons::IconName {
        match self {
            WindowControl::Minimize => aa_icons::IconName::GenericMinimize,
            WindowControl::Maximize => aa_icons::IconName::GenericMaximize,
            WindowControl::Close => aa_icons::IconName::GenericClose,
        }
    }

    /// 稳定的控件 id：测试按 id 定位/点击（iced 的 `button` 不收 id，
    /// 由 [`caption_button`] 外面套的容器挂上）。
    pub fn id(self) -> iced::widget::Id {
        iced::widget::Id::new(match self {
            WindowControl::Minimize => "window-control-minimize",
            WindowControl::Maximize => "window-control-maximize",
            WindowControl::Close => "window-control-close",
        })
    }
}

/// 标题栏（窗口框架）。
///
/// 用法：宿主填两端的插槽，把「按下整条 = 拖窗口」与三个窗口按钮的消息接上，
/// 再自己决定窗口按钮要不要画。
///
/// ```ignore
/// TitleBar::new(&theme)
///     .focused(self.focused)
///     .text_size(16.)
///     .left(menu_bar)
///     .right(status_items)
///     .window_controls(true)
///     .on_drag(Message::DragWindow)
///     .on_window_control(Message::WindowControl)
///     .into_element()
/// ```
pub struct TitleBar<'a, Message> {
    theme: &'a AppTheme,
    focused: bool,
    text_size: f32,
    height: Option<f32>,
    window_controls: bool,
    left: Vec<Element<'a, Message>>,
    right: Vec<Element<'a, Message>>,
    on_drag: Option<Message>,
    on_window_control: Option<Box<dyn Fn(WindowControl) -> Message + 'a>>,
}

impl<'a, Message> TitleBar<'a, Message> {
    /// 新建一条；`theme` 是取色用的主题。
    pub fn new(theme: &'a AppTheme) -> Self {
        TitleBar {
            theme,
            focused: true,
            text_size: 16.0,
            height: None,
            window_controls: false,
            left: Vec::new(),
            right: Vec::new(),
            on_drag: None,
            on_window_control: None,
        }
    }

    /// 窗口是否有焦点。
    ///
    /// 有影响是因为 Linux 上失焦的标题栏要变暗 —— 上游 `title_bar_color()` 在
    /// linux/freebsd 上这么分（`title_bar_background` / `title_bar_inactive_background`），
    /// 别的平台不分。
    pub fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    /// 基准字号，用来算高度（[`platform_title_bar_height`] 的入参）。
    pub fn text_size(mut self, text_size: f32) -> Self {
        self.text_size = text_size;
        self
    }

    /// 直接指定高度；不给就按 [`platform_title_bar_height`] 算。
    pub fn height(mut self, height: f32) -> Self {
        self.height = Some(height);
        self
    }

    /// 画不画最小化/最大化/关闭（只有 CSD 才该画，见模块头）。
    pub fn window_controls(mut self, window_controls: bool) -> Self {
        self.window_controls = window_controls;
        self
    }

    /// 加一条左侧内容（菜单、项目名、分支……）。
    pub fn left(mut self, content: impl Into<Element<'a, Message>>) -> Self {
        self.left.push(content.into());
        self
    }

    /// 加一条右侧内容（协作者、状态、用户菜单……）。
    pub fn right(mut self, content: impl Into<Element<'a, Message>>) -> Self {
        self.right.push(content.into());
        self
    }

    /// 按下标题栏空白处（不是按钮上）时产出的消息 —— 宿主拿它去
    /// `iced::window::drag(id)`。
    pub fn on_drag(mut self, message: Message) -> Self {
        self.on_drag = Some(message);
        self
    }

    /// 窗口按钮的消息；不给就不画按钮（画了也点不动）。
    pub fn on_window_control(mut self, map: impl Fn(WindowControl) -> Message + 'a) -> Self {
        self.on_window_control = Some(Box::new(map));
        self
    }

    /// 拼出这条标题栏。
    pub fn into_element(self) -> Element<'a, Message>
    where
        Message: Clone + 'a,
    {
        let colors = self.theme.colors();
        // 上游 `title_bar_color()`：linux 上失焦用木炭底，聚焦用主底色。
        let background = if self.focused {
            colors.title_bar_background
        } else {
            colors.title_bar_inactive_background
        };
        let height = self
            .height
            .unwrap_or_else(|| platform_title_bar_height(self.text_size));

        let mut content = row![]
            .height(Length::Fixed(height))
            .width(Fill)
            .align_y(Alignment::Center)
            .padding(Padding {
                top: 0.0,
                right: 0.0,
                bottom: 0.0,
                left: TITLE_BAR_LEFT_PADDING,
            });

        content = content.extend(self.left);
        content = content.push(space::horizontal());
        content = content.extend(self.right);

        if self.window_controls
            && let Some(map) = self.on_window_control
        {
            // 按钮组：组内间距与两侧内边距照 zed `LinuxWindowControls` 的
            // `gap_3()`/`px_3()`，整组贴在条的最右端。
            let mut controls = iced::widget::Row::new()
                .spacing(CAPTION_GROUP_SPACING)
                .padding([0.0, CAPTION_GROUP_PADDING]);
            for control in [
                WindowControl::Minimize,
                WindowControl::Maximize,
                WindowControl::Close,
            ] {
                controls = controls.push(caption_button(self.theme, control, &*map));
            }
            content = content.push(controls);
        }

        let bar = container(content)
            .width(Fill)
            .style(move |_| container::Style {
                background: Some(Background::Color(background)),
                ..container::Style::default()
            });

        // 整条拖拽；按钮自己会吃掉按下事件（`mouse_area` 见到子控件 capture 就
        // 不再处理），所以按按钮不会顺带拖窗口 —— 与上游在按钮组上
        // `stop_propagation` 同一个效果。
        match self.on_drag {
            Some(on_drag) => mouse_area(bar).on_press(on_drag).into(),
            None => bar.into(),
        }
    }
}

/// 一个窗口按钮。
///
/// 样式抄 zed 的 `LinuxWindowControls`（同机可见的那套）：20×20 的圆形悬停块
/// （`ghost_element_hover`）、16px 图标、图标色 `icon`/悬停 `icon_muted` ——
/// 关闭键**不做**红底白字，那是 Windows 版 `WindowsCaptionButton` 的规格，
/// Linux 上游没有。图标与 aa-zed 侧 `WindowControlType::icon` 同款（`aa_icons`
/// 内嵌的 `Generic*` SVG）；iced 的 svg `color` 过滤器保 alpha 换 RGB，SVG 里
/// 写死的笔画色会被整体替换，主题色跟得上来。
fn caption_button<'a, Message: Clone + 'a>(
    theme: &'a AppTheme,
    control: WindowControl,
    map: &dyn Fn(WindowControl) -> Message,
) -> Element<'a, Message> {
    let colors = theme.colors();
    // 窗口控制四枚随 aa_icons 内嵌，必然取得到。
    let handle = svg::Handle::from_memory(control.icon().svg_bytes().expect("窗口控制图标已内嵌"));
    // 图标色照 zed Linux：常态 `icon`，悬停 `icon_muted`。
    let icon_color = move |hovered: bool| {
        if hovered {
            colors.icon_muted
        } else {
            colors.icon
        }
    };

    let button = button(
        svg::Svg::new(handle)
            .width(CAPTION_ICON_SIZE)
            .height(CAPTION_ICON_SIZE)
            .style(move |_, status| svg::Style {
                color: Some(icon_color(status == svg::Status::Hovered)),
            }),
    )
    .width(Length::Fixed(CAPTION_BUTTON_WIDTH))
    .height(Length::Fixed(CAPTION_BUTTON_WIDTH))
    .padding(0)
    .on_press(map(control))
    .style(move |_, status| {
        // 悬停/按下的底色是同色圆形小块（半径取边长一半 = 圆），与上游
        // `rounded_2xl` 在 20px 盒子上的实际观感一致。
        let background = match status {
            button::Status::Hovered | button::Status::Pressed => Some(colors.ghost_element_hover),
            _ => None,
        };
        button::Style {
            background: background.map(Background::Color),
            text_color: colors.text,
            border: Border {
                radius: (CAPTION_BUTTON_WIDTH / 2.0).into(),
                ..Border::default()
            },
            ..button::Style::default()
        }
    });

    // iced 的 `button` 不收 id；套一层零内边距容器把测试用的稳定 id 挂上去
    // （容器原样包住按钮，点击落点就是按钮中心）。
    container(button).id(control.id()).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aa_theme_iced::Theme;
    use iced::widget::text;

    static THEME: std::sync::LazyLock<Theme> = std::sync::LazyLock::new(Theme::dark);

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Msg {
        Drag,
        Control(WindowControl),
    }

    /// 一条带窗口按钮与拖拽的标题栏，两端各塞一个标记。
    fn bar() -> Element<'static, Msg> {
        TitleBar::new(&THEME)
            .text_size(16.)
            .left(text("File"))
            .right(text("99%"))
            .window_controls(true)
            .on_drag(Msg::Drag)
            .on_window_control(Msg::Control)
            .into_element()
    }

    #[test]
    fn the_height_is_one_and_three_quarter_lines_with_a_floor() {
        // 1.75 × 16 = 28 → 触到 34 的下限。
        assert_eq!(platform_title_bar_height(16.), TITLE_BAR_MIN_HEIGHT);
        // 字号大到 1.75 倍超过下限时按比例长。
        assert_eq!(platform_title_bar_height(24.), 42.);
    }

    #[test]
    fn the_bar_renders_the_slots_on_both_ends() {
        let mut ui = iced_test::simulator(bar());
        assert!(ui.find("File").is_ok(), "左侧内容要画出来");
        assert!(ui.find("99%").is_ok(), "右侧内容要画出来");
        assert!(
            ui.find(WindowControl::Close.id()).is_ok(),
            "关闭键（按 id 找：图标是 SVG，没有可查的文本）"
        );
    }

    #[test]
    fn clicking_a_window_control_reports_it() {
        for control in [WindowControl::Minimize, WindowControl::Maximize] {
            let mut ui = iced_test::simulator(bar());
            assert!(ui.click(control.id()).is_ok(), "点得到 {control:?}");
            let messages: Vec<Msg> = ui.into_messages().collect();
            assert_eq!(messages, vec![Msg::Control(control)]);
        }
    }

    /// 按窗口按钮**不该**同时拖窗口：按钮吃掉了按下事件（上游是在按钮组上
    /// `stop_propagation`）。
    #[test]
    fn clicking_a_button_does_not_start_a_drag() {
        let mut ui = iced_test::simulator(bar());
        assert!(ui.click(WindowControl::Close.id()).is_ok());
        let messages: Vec<Msg> = ui.into_messages().collect();
        assert!(
            !messages.contains(&Msg::Drag),
            "点按钮不该拖窗口：{messages:?}"
        );
    }

    /// 空白处按下就是拖窗口。
    #[test]
    fn clicking_the_empty_area_starts_a_drag() {
        let mut ui = iced_test::simulator(bar());
        let bounds = ui
            .find("File")
            .expect("左侧内容")
            .visible_bounds()
            .expect("可见");
        // 往右挪开一点，落在左边内容与右边内容之间的空白里。
        ui.point_at(iced::Point::new(bounds.x + 120.0, bounds.center_y()));
        ui.simulate([
            iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)),
            iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                iced::mouse::Button::Left,
            )),
        ]);
        let messages: Vec<Msg> = ui.into_messages().collect();
        assert_eq!(messages, vec![Msg::Drag], "空白处按下应产出拖拽消息");
    }

    /// 不画按钮时，那一段也是拖拽区（没有按钮可点）。
    #[test]
    fn without_controls_the_bar_has_no_caption_buttons() {
        let mut ui = iced_test::simulator(
            TitleBar::new(&THEME)
                .left(text("File"))
                .on_drag(Msg::Drag)
                .into_element(),
        );
        assert!(ui.find("File").is_ok());
        assert!(
            ui.find(WindowControl::Close.id()).is_err(),
            "没接消息就不画按钮（画了也点不动）"
        );
    }
}
