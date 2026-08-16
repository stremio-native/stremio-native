//! Persistence for the theme studio.
//!
//! The page mutates the Slint `Theme` global directly so edits are instant;
//! these handlers mirror the same edits into the stored palette so they survive
//! a restart. Derived tokens (text tints, overlays, dividers) are recomputed
//! here exactly as `ThemePage.apply-theme` computes them.

use slint::{Color, ComponentHandle};

use crate::{MainWindow, Theme, config};

pub fn install(ui: &MainWindow) {
    ui.on_update_theme_color(|key, value| {
        let mut current = config::load_config();
        if !assign(&mut current.theme, key.as_str(), value) {
            return;
        }
        config::save_config(&current);
    });

    let handle_preset = ui.as_weak();
    ui.on_apply_theme_preset(move |accent, background, secondary, card, border, text| {
        let Some(ui) = handle_preset.upgrade() else {
            return;
        };
        let mut current = config::load_config();
        current.theme.active_preset_idx = ui.global::<Theme>().get_active_preset_idx();
        let theme = &mut current.theme;
        theme.accent = hex(accent);
        theme.background = hex(background);
        theme.secondary_background = hex(secondary);
        theme.card_background = hex(card);
        theme.card_border = hex(border);
        theme.text_primary = hex(text);
        theme.text_secondary = hex(text.with_alpha(0.7));
        theme.text_muted = hex(text.with_alpha(0.5));
        theme.modal_background = hex(card);
        theme.sidebar_background = hex(card.with_alpha(0.85));
        theme.control_background = hex(text.with_alpha(0.08));
        theme.overlay = hex(text.with_alpha(0.08));
        theme.overlay_hover = hex(text.with_alpha(0.14));
        theme.divider = hex(text.with_alpha(0.12));
        config::save_config(&current);
    });

    let handle = ui.as_weak();
    ui.on_reset_theme_defaults(move || {
        let Some(ui) = handle.upgrade() else { return };
        let mut current = config::load_config();
        // The saved palettes belong to the colour picker, not to the theme, so
        // resetting the look doesn't throw away swatches the user collected.
        let defaults = config::ThemeConfig {
            saved_colors: current.theme.saved_colors.clone(),
            saved_gradients: current.theme.saved_gradients.clone(),
            ..config::ThemeConfig::default()
        };
        current.theme = defaults;
        config::save_config(&current);
        crate::apply_theme(&ui, &current);
    });
}

fn assign(theme: &mut config::ThemeConfig, key: &str, value: Color) -> bool {
    let encoded = hex(value);
    match key {
        "accent" => theme.accent = encoded,
        "accent_hover" => theme.accent_hover = encoded,
        "secondary_accent" => theme.success = encoded,
        "tertiary_accent" => theme.warning = encoded,
        "quaternary_accent" => theme.info = encoded,
        "background" => theme.background = encoded,
        "secondary_background" => theme.secondary_background = encoded,
        "card_background" => {
            theme.modal_background = encoded.clone();
            theme.card_background = encoded;
        }
        "card_border" => theme.card_border = encoded,
        "modal_background" => theme.modal_background = encoded,
        "sidebar_background" => theme.sidebar_background = encoded,
        "title_bar" => theme.title_bar = encoded,
        "text_primary" => {
            theme.text_secondary = hex(value.with_alpha(0.7));
            theme.text_muted = hex(value.with_alpha(0.5));
            theme.text_primary = encoded;
        }
        "text_secondary" => theme.text_secondary = encoded,
        "text_muted" => theme.text_muted = encoded,
        "overlay" => {
            theme.overlay = encoded.clone();
            theme.control_background = encoded;
        }
        "divider" => theme.divider = encoded,
        "danger" => theme.danger = encoded,
        _ => return false,
    }
    true
}

fn hex(color: Color) -> String {
    format!(
        "#{:02X}{:02X}{:02X}{:02X}",
        color.red(),
        color.green(),
        color.blue(),
        color.alpha()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assigns_known_keys_only() {
        let mut theme = config::ThemeConfig::default();
        assert!(assign(&mut theme, "accent", Color::from_rgb_u8(1, 2, 3)));
        assert_eq!(theme.accent, "#010203FF");
        assert!(!assign(
            &mut theme,
            "not_a_token",
            Color::from_rgb_u8(1, 2, 3)
        ));
    }

    #[test]
    fn text_primary_also_refreshes_its_tints() {
        let mut theme = config::ThemeConfig::default();
        assign(
            &mut theme,
            "text_primary",
            Color::from_rgb_u8(0xff, 0xff, 0xff),
        );
        assert_eq!(theme.text_primary, "#FFFFFFFF");
        assert_eq!(theme.text_secondary, "#FFFFFFB3");
        assert_eq!(theme.text_muted, "#FFFFFF80");
    }
}
