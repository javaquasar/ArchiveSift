use crate::app::{AppearanceConfig, UiDensity, UiTheme};
use eframe::egui;

pub(super) fn apply(ctx: &egui::Context, config: AppearanceConfig) {
    ctx.options_mut(|options| options.zoom_with_keyboard = false);
    ctx.set_theme(match config.theme {
        UiTheme::System => egui::ThemePreference::System,
        UiTheme::Light => egui::ThemePreference::Light,
        UiTheme::Dark => egui::ThemePreference::Dark,
    });
    ctx.set_zoom_factor(
        f32::from(
            config
                .zoom_percent
                .clamp(AppearanceConfig::MIN_ZOOM, AppearanceConfig::MAX_ZOOM),
        ) / 100.0,
    );
    let (item_spacing, control_height) = match config.density {
        UiDensity::Compact => (egui::vec2(6.0, 3.0), 18.0),
        UiDensity::Comfortable => (egui::vec2(8.0, 5.0), 22.0),
    };
    for theme in [egui::Theme::Light, egui::Theme::Dark] {
        ctx.style_mut_of(theme, |style| {
            style.spacing.item_spacing = item_spacing;
            style.spacing.interact_size.y = control_height;
        });
    }
}

pub(super) fn editor(ui: &mut egui::Ui, config: &mut AppearanceConfig) -> bool {
    let previous = *config;
    ui.heading("Appearance");
    ui.horizontal_wrapped(|ui| {
        ui.label("Theme");
        ui.selectable_value(&mut config.theme, UiTheme::System, "System");
        ui.selectable_value(&mut config.theme, UiTheme::Light, "Light");
        ui.selectable_value(&mut config.theme, UiTheme::Dark, "Dark");
    });
    ui.horizontal_wrapped(|ui| {
        ui.label("Density");
        ui.selectable_value(&mut config.density, UiDensity::Compact, "Compact");
        ui.selectable_value(&mut config.density, UiDensity::Comfortable, "Comfortable");
    });
    let save_zoom = ui
        .horizontal_wrapped(|ui| {
            ui.label("Interface scale");
            let slider = ui.add(
                egui::Slider::new(
                    &mut config.zoom_percent,
                    AppearanceConfig::MIN_ZOOM..=AppearanceConfig::MAX_ZOOM,
                )
                .suffix("%"),
            );
            if ui.button("Reset appearance").clicked() {
                *config = AppearanceConfig::default();
                return previous != *config;
            }
            (slider.changed() && !slider.dragged()) || slider.drag_stopped()
        })
        .inner;
    save_zoom || config.theme != previous.theme || config.density != previous.density
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::AppConfig;

    #[test]
    fn saved_preferences_apply_absolute_scale_and_density_to_both_themes() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("settings.json");
        let config = AppConfig {
            appearance: AppearanceConfig {
                theme: UiTheme::Dark,
                density: UiDensity::Compact,
                zoom_percent: 150,
            },
            ..AppConfig::default()
        };
        config.save_to(&path).unwrap();
        let loaded = AppConfig::load_from(&path).unwrap().appearance;
        assert_eq!(loaded, config.appearance);
        let ctx = egui::Context::default();
        for _ in 0..2 {
            apply(&ctx, loaded);
            let _ = ctx.run(egui::RawInput::default(), |_| {});
            assert_eq!(ctx.zoom_factor(), 1.5);
            assert_eq!(ctx.theme(), egui::Theme::Dark);
            for theme in [egui::Theme::Light, egui::Theme::Dark] {
                assert_eq!(ctx.style_of(theme).spacing.interact_size.y, 18.0);
            }
        }
    }
}
