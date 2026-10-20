use aa_assets_gpui::GpuiAssets;
use aa_ui_zed::{Button, ButtonCommon, ButtonStyle, Label, TintColor, prelude::*, v_flex};
use gpui::{Application, Context, Render, Window, WindowOptions};

fn main() {
    let app = build_application()
        .with_assets(GpuiAssets);

    app.run(|cx| {
        // zed main.rs L670，原样
        theme_settings_gpui::init(aa_theme_gpui::LoadThemes::All(Box::new(GpuiAssets)), cx);

        // zed 的开窗风格（workspace.rs L11732）；c019950 的 open_window 返回 must-use Result
        if let Err(e) = cx.open_window(WindowOptions::default(), |_window, cx| {
            cx.new(|_| Counter { value: 42 })
        }) {
            eprintln!("failed to open main window: {e:#}");
        }
    });
}
fn build_application() -> Application {
    Application::new_inaccessible(gpui_platform::current_platform(false))
}
struct Counter {
    value: i32,
}

impl Render for Counter {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .items_center()
            .gap_4()
            .px_4()
            .py_4() // c019950 的 gpui Styled 只有缩写式 px_*/py_*，没有老式 padding_4/
            // padding()（fork 内部组件也是这么用的，见 toggle_button.rs / label_like.rs）
            .child(Label::new(self.value.to_string()))
            .child(
                Button::new("inc", "+") // 第一参是 id（ElementId），和手搓版 .id() 同源
                    .style(ButtonStyle::Filled)
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.value += 1;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("dec", "-")
                    .style(ButtonStyle::Subtle) // 默认款：透明底，hover/active 浮现
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.value -= 1;
                        cx.notify();
                    })),
            )
                        .child(
                Button::new("reset", "重置")
                    .style(ButtonStyle::Tinted(TintColor::Error)) // 语义色
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.value = 0;
                        cx.notify();
                    })),
            )
    }
}
