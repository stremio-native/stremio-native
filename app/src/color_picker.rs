//! Host support for the Slint colour picker.
//!
//! The picker formats colours itself, but three things need the host: parsing
//! typed text back into a colour (Slint has no per-character string access),
//! sampling the pixel under the cursor for the eyedropper, and owning the saved
//! palettes so they survive a restart.

use slint::{Color, ComponentHandle, Model as _, ModelRc, VecModel};

use crate::{ColorUtils, CursorSample, MainWindow, ParsedColor, SavedGradient, config};

const MAX_SAVED: usize = 12;

pub fn install(ui: &MainWindow) {
    let utils = ui.global::<ColorUtils>();

    utils.on_parse(|text, mode| match parse(text.as_str(), mode) {
        Some(value) => ParsedColor { valid: true, value },
        None => ParsedColor {
            valid: false,
            value: Color::default(),
        },
    });

    utils.set_eyedropper_available(eyedropper::available());
    utils.on_sample_cursor(|| match eyedropper::sample() {
        Some((value, pressed)) => CursorSample {
            valid: true,
            value,
            pressed,
        },
        None => CursorSample {
            valid: false,
            value: Color::default(),
            pressed: false,
        },
    });

    let theme = config::with_config(|config| config.theme.clone());
    utils.set_saved_colors(ModelRc::new(VecModel::from(
        theme
            .saved_colors
            .iter()
            .filter_map(|hex| config::parse_color(hex))
            .collect::<Vec<_>>(),
    )));
    utils.set_saved_gradients(ModelRc::new(VecModel::from(
        theme
            .saved_gradients
            .iter()
            .filter_map(saved_gradient)
            .collect::<Vec<_>>(),
    )));

    let handle = ui.as_weak();
    utils.on_save_color(move |value| {
        let Some(ui) = handle.upgrade() else { return };
        let utils = ui.global::<ColorUtils>();
        let mut colors: Vec<Color> = utils.get_saved_colors().iter().collect();
        if colors.contains(&value) {
            return;
        }
        colors.push(value);
        while colors.len() > MAX_SAVED {
            colors.remove(0);
        }
        persist(|theme| theme.saved_colors = colors.iter().map(|color| hex(*color)).collect());
        utils.set_saved_colors(ModelRc::new(VecModel::from(colors)));
    });

    let handle = ui.as_weak();
    utils.on_save_gradient(move |value| {
        let Some(ui) = handle.upgrade() else { return };
        let utils = ui.global::<ColorUtils>();
        let mut gradients: Vec<SavedGradient> = utils.get_saved_gradients().iter().collect();
        gradients.push(value);
        while gradients.len() > MAX_SAVED {
            gradients.remove(0);
        }
        persist(|theme| {
            theme.saved_gradients = gradients
                .iter()
                .map(|gradient| config::SavedGradientConfig {
                    angle: gradient.angle,
                    start: hex(gradient.start),
                    end: hex(gradient.end),
                })
                .collect()
        });
        utils.set_saved_gradients(ModelRc::new(VecModel::from(gradients)));
    });
}

fn persist(update: impl FnOnce(&mut config::ThemeConfig)) {
    let mut current = config::load_config();
    update(&mut current.theme);
    config::save_config(&current);
}

fn saved_gradient(stored: &config::SavedGradientConfig) -> Option<SavedGradient> {
    Some(SavedGradient {
        angle: stored.angle,
        start: config::parse_color(&stored.start)?,
        end: config::parse_color(&stored.end)?,
    })
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

/// `mode` mirrors `ColorUtils.format-text`: 0 hex, 1 rgb, 2 hsl. Anything that
/// looks like a hex literal is accepted whatever the selected format is, so
/// pasting `#7F56D9` works without switching the dropdown first.
fn parse(text: &str, mode: i32) -> Option<Color> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if text.starts_with('#') || mode == 0 {
        return parse_hex(text);
    }
    let parts = components(text)?;
    match mode {
        1 => parse_rgb(&parts),
        2 => parse_hsl(&parts),
        _ => None,
    }
}

fn parse_hex(text: &str) -> Option<Color> {
    let digits = text.trim().trim_start_matches('#');
    if !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |index: usize| u8::from_str_radix(&digits[index..index + 2], 16).ok();
    // #RGB / #RGBA shorthand doubles each digit, as in CSS.
    let short = |index: usize| {
        u8::from_str_radix(&digits[index..index + 1], 16)
            .ok()
            .map(|value| value * 17)
    };
    match digits.len() {
        3 => Some(Color::from_rgb_u8(short(0)?, short(1)?, short(2)?)),
        4 => Some(Color::from_argb_u8(
            short(3)?,
            short(0)?,
            short(1)?,
            short(2)?,
        )),
        6 => Some(Color::from_rgb_u8(byte(0)?, byte(2)?, byte(4)?)),
        8 => Some(Color::from_argb_u8(byte(6)?, byte(0)?, byte(2)?, byte(4)?)),
        _ => None,
    }
}

/// Splits `12, 34, 56` or `rgb(12 34 56)` into its numeric components.
fn components(text: &str) -> Option<Vec<f32>> {
    let inner = text
        .trim_start_matches(|c: char| c.is_ascii_alphabetic())
        .trim_matches(|c: char| c == '(' || c == ')' || c.is_whitespace());
    let values: Vec<f32> = inner
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|part| !part.is_empty())
        .map(|part| part.trim_end_matches('%').parse::<f32>())
        .collect::<Result<_, _>>()
        .ok()?;
    (values.len() == 3 || values.len() == 4).then_some(values)
}

fn parse_rgb(parts: &[f32]) -> Option<Color> {
    let channel = |value: f32| value.clamp(0.0, 255.0).round() as u8;
    let alpha = parts.get(3).copied().unwrap_or(1.0).clamp(0.0, 1.0);
    Some(Color::from_argb_u8(
        (alpha * 255.0).round() as u8,
        channel(parts[0]),
        channel(parts[1]),
        channel(parts[2]),
    ))
}

fn parse_hsl(parts: &[f32]) -> Option<Color> {
    let hue = parts[0].rem_euclid(360.0);
    let saturation = (parts[1] / 100.0).clamp(0.0, 1.0);
    let lightness = (parts[2] / 100.0).clamp(0.0, 1.0);
    let alpha = parts.get(3).copied().unwrap_or(1.0).clamp(0.0, 1.0);

    let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
    let secondary = chroma * (1.0 - ((hue / 60.0) % 2.0 - 1.0).abs());
    let base = lightness - chroma / 2.0;
    let (r, g, b) = match hue as u32 / 60 {
        0 => (chroma, secondary, 0.0),
        1 => (secondary, chroma, 0.0),
        2 => (0.0, chroma, secondary),
        3 => (0.0, secondary, chroma),
        4 => (secondary, 0.0, chroma),
        _ => (chroma, 0.0, secondary),
    };
    let channel = |value: f32| ((value + base).clamp(0.0, 1.0) * 255.0).round() as u8;
    Some(Color::from_argb_u8(
        (alpha * 255.0).round() as u8,
        channel(r),
        channel(g),
        channel(b),
    ))
}

/// Reads the colour of the pixel under the cursor, plus whether the primary
/// mouse button is held. The picker polls this while a pick is in progress and
/// commits on the next press, which is how it can sample outside our own
/// windows without installing a global input hook.
#[cfg(windows)]
mod eyedropper {
    use slint::Color;
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{CLR_INVALID, GetDC, GetPixel, ReleaseDC};
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

    pub fn available() -> bool {
        true
    }

    pub fn sample() -> Option<(Color, bool)> {
        let mut cursor = POINT::default();
        // SAFETY: `cursor` is a valid out-parameter; the screen DC is released
        // on every path out of this function.
        unsafe {
            if GetCursorPos(&mut cursor).is_err() {
                return None;
            }
            let screen = GetDC(None);
            if screen.0.is_null() {
                return None;
            }
            let pixel = GetPixel(screen, cursor.x, cursor.y);
            ReleaseDC(None, screen);
            if pixel.0 == CLR_INVALID {
                return None;
            }
            let pressed = GetAsyncKeyState(VK_LBUTTON.0 as i32) as u16 & 0x8000 != 0;
            Some((
                Color::from_rgb_u8(
                    (pixel.0 & 0xff) as u8,
                    ((pixel.0 >> 8) & 0xff) as u8,
                    ((pixel.0 >> 16) & 0xff) as u8,
                ),
                pressed,
            ))
        }
    }
}

#[cfg(not(windows))]
mod eyedropper {
    use slint::Color;

    pub fn available() -> bool {
        false
    }

    pub fn sample() -> Option<(Color, bool)> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_in_every_length() {
        assert_eq!(
            parse("#7F56D9", 0),
            Some(Color::from_rgb_u8(0x7f, 0x56, 0xd9))
        );
        assert_eq!(
            parse("7f56d9", 0),
            Some(Color::from_rgb_u8(0x7f, 0x56, 0xd9))
        );
        assert_eq!(parse("#f00", 0), Some(Color::from_rgb_u8(0xff, 0, 0)));
        assert_eq!(
            parse("#7F56D980", 0),
            Some(Color::from_argb_u8(0x80, 0x7f, 0x56, 0xd9))
        );
        assert_eq!(parse("#zz", 0), None);
        assert_eq!(parse("", 0), None);
    }

    #[test]
    fn parses_rgb_and_hsl() {
        assert_eq!(
            parse("127, 86, 217", 1),
            Some(Color::from_rgb_u8(127, 86, 217))
        );
        assert_eq!(
            parse("rgb(127 86 217)", 1),
            Some(Color::from_rgb_u8(127, 86, 217))
        );
        assert_eq!(
            parse("0, 100%, 50%", 2),
            Some(Color::from_rgb_u8(255, 0, 0))
        );
        assert_eq!(
            parse("120, 100%, 50%", 2),
            Some(Color::from_rgb_u8(0, 255, 0))
        );
        assert_eq!(
            parse("0, 0%, 100%", 2),
            Some(Color::from_rgb_u8(255, 255, 255))
        );
        assert_eq!(parse("nonsense", 1), None);
    }

    #[test]
    fn hex_accepted_regardless_of_selected_format() {
        assert_eq!(
            parse("#7F56D9", 2),
            Some(Color::from_rgb_u8(0x7f, 0x56, 0xd9))
        );
    }
}
