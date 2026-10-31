use std::sync::RwLock;

use rataflow::{Palette, Theme};
use ratatui::style::{Color, Modifier, Style};
use serde::Deserialize;

/// Colors from the `[theme]` table: a `preset` (`dark`, `light` or `contrast`) and
/// any colors to override, as names (`red`), indexes (`81`) or hex (`#5fd7ff`).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ThemeConfig {
    pub preset: Option<String>,
    pub canvas: Option<String>,
    pub surface: Option<String>,
    pub muted: Option<String>,
    pub subtle: Option<String>,
    pub accent: Option<String>,
    pub text: Option<String>,
    pub success: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug)]
pub struct Colors {
    pub canvas: Color,
    pub surface: Color,
    pub muted: Color,
    pub subtle: Color,
    pub accent: Color,
    pub text: Color,
    pub success: Color,
    pub error: Color,
}

impl Colors {
    const DARK: Self = Self {
        canvas: Color::Indexed(234),
        surface: Color::Indexed(236),
        muted: Color::Indexed(245),
        subtle: Color::Indexed(238),
        accent: Color::Indexed(81),
        text: Color::Indexed(252),
        success: Color::Indexed(78),
        error: Color::Indexed(203),
    };

    const LIGHT: Self = Self {
        canvas: Color::Indexed(255),
        surface: Color::Indexed(254),
        muted: Color::Indexed(243),
        subtle: Color::Indexed(250),
        accent: Color::Indexed(25),
        text: Color::Indexed(235),
        success: Color::Indexed(28),
        error: Color::Indexed(160),
    };

    const CONTRAST: Self = Self {
        canvas: Color::Black,
        surface: Color::Indexed(234),
        muted: Color::Indexed(250),
        subtle: Color::Indexed(240),
        accent: Color::Indexed(226),
        text: Color::White,
        success: Color::Indexed(46),
        error: Color::Indexed(196),
    };

    pub fn from_config(config: &ThemeConfig) -> Result<Self, String> {
        let mut colors = match config.preset.as_deref().unwrap_or("dark") {
            "dark" => Self::DARK,
            "light" => Self::LIGHT,
            "contrast" | "high-contrast" => Self::CONTRAST,
            other => {
                return Err(format!(
                    "Unknown theme preset {other}. Use dark, light or contrast."
                ));
            }
        };
        for (value, slot) in [
            (&config.canvas, &mut colors.canvas),
            (&config.surface, &mut colors.surface),
            (&config.muted, &mut colors.muted),
            (&config.subtle, &mut colors.subtle),
            (&config.accent, &mut colors.accent),
            (&config.text, &mut colors.text),
            (&config.success, &mut colors.success),
            (&config.error, &mut colors.error),
        ] {
            if let Some(value) = value {
                *slot = value
                    .parse()
                    .map_err(|_| format!("Invalid theme color {value}."))?;
            }
        }
        Ok(colors)
    }
}

static COLORS: RwLock<Colors> = RwLock::new(Colors::DARK);
static CONFIG: RwLock<Option<ThemeConfig>> = RwLock::new(None);

/// Sets the colors from the `[theme]` table.
pub fn init(config: &ThemeConfig) -> Result<(), String> {
    let colors = Colors::from_config(config)?;
    if let (Ok(mut current), Ok(mut stored)) = (COLORS.write(), CONFIG.write()) {
        *current = colors;
        *stored = Some(config.clone());
    }
    Ok(())
}

/// Changes some colors or the preset while termail runs; unset fields keep their
/// current value.
pub fn update(changes: &ThemeConfig) -> Result<(), String> {
    let mut config = CONFIG
        .read()
        .ok()
        .and_then(|config| config.clone())
        .unwrap_or_default();
    if changes.preset.is_some() {
        // A new preset starts from its own colors.
        config = ThemeConfig::default();
    }
    for (change, slot) in [
        (&changes.preset, &mut config.preset),
        (&changes.canvas, &mut config.canvas),
        (&changes.surface, &mut config.surface),
        (&changes.muted, &mut config.muted),
        (&changes.subtle, &mut config.subtle),
        (&changes.accent, &mut config.accent),
        (&changes.text, &mut config.text),
        (&changes.success, &mut config.success),
        (&changes.error, &mut config.error),
    ] {
        if change.is_some() {
            slot.clone_from(change);
        }
    }
    init(&config)
}

fn colors() -> Colors {
    COLORS.read().map(|colors| *colors).unwrap_or(Colors::DARK)
}

pub fn canvas() -> Color {
    colors().canvas
}

pub fn surface() -> Color {
    colors().surface
}

pub fn muted_color() -> Color {
    colors().muted
}

pub fn subtle() -> Color {
    colors().subtle
}

pub fn accent() -> Color {
    colors().accent
}

pub fn text_color() -> Color {
    colors().text
}

pub fn error_color() -> Color {
    colors().error
}

pub fn success_color() -> Color {
    colors().success
}

pub fn flow() -> Theme {
    let colors = colors();
    Theme::Custom(Palette {
        canvas_bg: colors.canvas,
        surface: colors.surface,
        muted: colors.muted,
        subtle: colors.subtle,
        accent: colors.accent,
        text: colors.text,
        success: colors.success,
        error: colors.error,
    })
}

pub fn border(active: bool) -> Style {
    Style::default().fg(if active { accent() } else { subtle() })
}

pub fn title(active: bool) -> Style {
    let style = Style::default().fg(if active { accent() } else { muted_color() });
    if active {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    }
}

pub fn text() -> Style {
    Style::default().fg(text_color())
}

pub fn muted() -> Style {
    Style::default().fg(muted_color())
}

pub fn selection() -> Style {
    Style::default()
        .fg(text_color())
        .bg(subtle())
        .add_modifier(Modifier::BOLD)
}

pub fn keycap() -> Style {
    Style::default()
        .fg(text_color())
        .bg(subtle())
        .add_modifier(Modifier::BOLD)
}

/// A hint under the pointer: the accent color behind the key.
pub fn hover() -> Style {
    Style::default()
        .fg(canvas())
        .bg(accent())
        .add_modifier(Modifier::BOLD)
}
