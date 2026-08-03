//! The reference theme catalog, pinned to `sorted_theme_names()` over Textual's
//! built-in themes plus the automatic entry.
//!
//! Rendering only distinguishes light from dark, so each catalog entry carries
//! the polarity the reference theme declares.

use super::setup::Theme;

/// Reference `AUTO_THEME`.
pub const AUTO_THEME: &str = "auto";

/// One catalog entry: the reference name and whether that theme is dark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogTheme {
    pub name: &'static str,
    pub dark: bool,
}

/// Reference `BUILTIN_THEMES`, already ordered as `sorted_theme_names()` returns
/// them: the automatic entry, then light names, then dark names, each sorted.
pub const THEMES: [CatalogTheme; 22] = [
    CatalogTheme {
        name: AUTO_THEME,
        dark: false,
    },
    CatalogTheme {
        name: "ansi-light",
        dark: false,
    },
    CatalogTheme {
        name: "atom-one-light",
        dark: false,
    },
    CatalogTheme {
        name: "catppuccin-latte",
        dark: false,
    },
    CatalogTheme {
        name: "rose-pine-dawn",
        dark: false,
    },
    CatalogTheme {
        name: "solarized-light",
        dark: false,
    },
    CatalogTheme {
        name: "textual-light",
        dark: false,
    },
    CatalogTheme {
        name: "ansi-dark",
        dark: true,
    },
    CatalogTheme {
        name: "atom-one-dark",
        dark: true,
    },
    CatalogTheme {
        name: "catppuccin-frappe",
        dark: true,
    },
    CatalogTheme {
        name: "catppuccin-macchiato",
        dark: true,
    },
    CatalogTheme {
        name: "catppuccin-mocha",
        dark: true,
    },
    CatalogTheme {
        name: "dracula",
        dark: true,
    },
    CatalogTheme {
        name: "flexoki",
        dark: true,
    },
    CatalogTheme {
        name: "gruvbox",
        dark: true,
    },
    CatalogTheme {
        name: "monokai",
        dark: true,
    },
    CatalogTheme {
        name: "nord",
        dark: true,
    },
    CatalogTheme {
        name: "rose-pine",
        dark: true,
    },
    CatalogTheme {
        name: "rose-pine-moon",
        dark: true,
    },
    CatalogTheme {
        name: "solarized-dark",
        dark: true,
    },
    CatalogTheme {
        name: "textual-dark",
        dark: true,
    },
    CatalogTheme {
        name: "tokyo-night",
        dark: true,
    },
];

/// Reference `sorted_theme_names`.
#[must_use]
pub fn sorted_theme_names() -> Vec<&'static str> {
    THEMES.iter().map(|theme| theme.name).collect()
}

/// The polarity a configured theme renders with. The Rust-native `system`,
/// `light`, and `dark` values stay valid alongside the reference catalog.
#[must_use]
pub fn theme_polarity(value: &str) -> Option<Theme> {
    match value {
        AUTO_THEME | "system" | "default" => Some(Theme::System),
        "light" => Some(Theme::Light),
        "dark" => Some(Theme::Dark),
        name => THEMES.iter().find(|theme| theme.name == name).map(|theme| {
            if theme.dark {
                Theme::Dark
            } else {
                Theme::Light
            }
        }),
    }
}

/// Every value the configuration accepts, for schema validation and help text.
#[must_use]
pub fn accepted_theme_values() -> Vec<&'static str> {
    let mut values = vec!["system", "light", "dark"];
    values.extend(THEMES.iter().map(|theme| theme.name));
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_matches_the_reference_order_and_polarity() {
        let names = sorted_theme_names();
        assert_eq!(names.first().copied(), Some(AUTO_THEME));
        let light = names
            .iter()
            .skip(1)
            .take_while(|name| theme_polarity(name) == Some(Theme::Light))
            .copied()
            .collect::<Vec<_>>();
        let dark = names
            .iter()
            .skip(1 + light.len())
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(
            light,
            [
                "ansi-light",
                "atom-one-light",
                "catppuccin-latte",
                "rose-pine-dawn",
                "solarized-light",
                "textual-light",
            ]
        );
        assert_eq!(dark.first().copied(), Some("ansi-dark"));
        assert_eq!(dark.last().copied(), Some("tokyo-night"));
        let mut sorted_light = light.clone();
        sorted_light.sort_unstable();
        assert_eq!(light, sorted_light);
        let mut sorted_dark = dark.clone();
        sorted_dark.sort_unstable();
        assert_eq!(dark, sorted_dark);
        assert_eq!(names.len(), 22);
    }

    #[test]
    fn every_catalog_value_is_accepted_by_the_configuration_schema() {
        let mut accepted = accepted_theme_values();
        accepted.sort_unstable();
        let mut schema_values = vibe_core::config::THEME_VALUES.to_vec();
        schema_values.sort_unstable();
        assert_eq!(
            accepted, schema_values,
            "the picker and the configuration schema must accept the same themes"
        );
    }

    #[test]
    fn polarity_covers_the_catalog_and_the_native_values() {
        assert_eq!(theme_polarity(AUTO_THEME), Some(Theme::System));
        assert_eq!(theme_polarity("system"), Some(Theme::System));
        assert_eq!(theme_polarity("nord"), Some(Theme::Dark));
        assert_eq!(theme_polarity("catppuccin-latte"), Some(Theme::Light));
        assert_eq!(theme_polarity("not-a-theme"), None);
        assert!(accepted_theme_values().contains(&"gruvbox"));
        assert!(accepted_theme_values().contains(&"system"));
    }
}
