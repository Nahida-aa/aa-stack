use gpui::App;
use aa_theme_gpui;

pub fn init(themes_to_load: aa_theme_gpui::LoadThemes, cx: &mut App) {
    aa_theme_gpui::init(themes_to_load, cx);
}
