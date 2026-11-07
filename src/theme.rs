use ratatui::style::Color;

/// High-contrast UI palette. Foreground colors are tuned for legibility without
/// relying on fully saturated neon colors or harsh pure-black/pure-white pairs.
#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub name: &'static str,
    pub bg: Color,
    pub panel: Color,
    pub border: Color,
    pub muted: Color,
    pub text: Color,
    pub primary: Color,
    pub success: Color,
    pub warning: Color,
    pub info: Color,
    pub accent: Color,
    pub selected_row: Color,
    pub search_fg: Color,
    pub search_bg: Color,
}

/// Built-in palettes, selected directly with the unmodified F2 through F10 keys.
pub const THEMES: [Theme; 9] = [
    Theme {
        name: "Abyss Cyan",
        bg: Color::Rgb(8, 18, 28),
        panel: Color::Rgb(17, 34, 48),
        border: Color::Rgb(82, 120, 142),
        muted: Color::Rgb(172, 193, 207),
        text: Color::Rgb(240, 247, 250),
        primary: Color::Rgb(91, 211, 215),
        success: Color::Rgb(119, 219, 164),
        warning: Color::Rgb(246, 199, 113),
        info: Color::Rgb(135, 187, 235),
        accent: Color::Rgb(205, 164, 224),
        selected_row: Color::Rgb(39, 65, 81),
        search_fg: Color::Rgb(10, 24, 31),
        search_bg: Color::Rgb(91, 211, 215),
    },
    Theme {
        name: "Carbon Amber",
        bg: Color::Rgb(18, 19, 21),
        panel: Color::Rgb(32, 34, 36),
        border: Color::Rgb(105, 111, 114),
        muted: Color::Rgb(187, 192, 195),
        text: Color::Rgb(245, 245, 244),
        primary: Color::Rgb(241, 187, 101),
        success: Color::Rgb(133, 211, 161),
        warning: Color::Rgb(247, 207, 114),
        info: Color::Rgb(131, 190, 220),
        accent: Color::Rgb(224, 154, 174),
        selected_row: Color::Rgb(58, 58, 56),
        search_fg: Color::Rgb(28, 24, 16),
        search_bg: Color::Rgb(241, 187, 101),
    },
    Theme {
        name: "Dracula Contrast",
        bg: Color::Rgb(23, 22, 31),
        panel: Color::Rgb(37, 35, 48),
        border: Color::Rgb(122, 108, 145),
        muted: Color::Rgb(195, 188, 210),
        text: Color::Rgb(247, 245, 250),
        primary: Color::Rgb(193, 167, 235),
        success: Color::Rgb(142, 218, 164),
        warning: Color::Rgb(241, 204, 122),
        info: Color::Rgb(143, 191, 226),
        accent: Color::Rgb(229, 158, 195),
        selected_row: Color::Rgb(64, 56, 79),
        search_fg: Color::Rgb(22, 18, 29),
        search_bg: Color::Rgb(193, 167, 235),
    },
    Theme {
        name: "Solarized Light",
        bg: Color::Rgb(250, 247, 238),
        panel: Color::Rgb(237, 233, 220),
        border: Color::Rgb(105, 115, 120),
        muted: Color::Rgb(84, 98, 103),
        text: Color::Rgb(30, 40, 44),
        primary: Color::Rgb(0, 90, 115),
        success: Color::Rgb(29, 103, 64),
        warning: Color::Rgb(133, 76, 16),
        info: Color::Rgb(48, 85, 142),
        accent: Color::Rgb(132, 53, 104),
        selected_row: Color::Rgb(212, 224, 219),
        search_fg: Color::Rgb(255, 255, 255),
        search_bg: Color::Rgb(0, 90, 115),
    },
    Theme {
        name: "Paper High Contrast",
        bg: Color::Rgb(250, 251, 253),
        panel: Color::Rgb(237, 241, 246),
        border: Color::Rgb(80, 94, 112),
        muted: Color::Rgb(73, 86, 103),
        text: Color::Rgb(23, 32, 46),
        primary: Color::Rgb(20, 76, 142),
        success: Color::Rgb(25, 109, 66),
        warning: Color::Rgb(137, 75, 0),
        info: Color::Rgb(34, 88, 146),
        accent: Color::Rgb(135, 45, 100),
        selected_row: Color::Rgb(213, 229, 247),
        search_fg: Color::Rgb(255, 255, 255),
        search_bg: Color::Rgb(20, 76, 142),
    },
    Theme {
        name: "Forest Signal",
        bg: Color::Rgb(10, 25, 20),
        panel: Color::Rgb(19, 42, 32),
        border: Color::Rgb(91, 132, 108),
        muted: Color::Rgb(180, 204, 186),
        text: Color::Rgb(241, 248, 242),
        primary: Color::Rgb(131, 213, 166),
        success: Color::Rgb(169, 219, 112),
        warning: Color::Rgb(239, 196, 115),
        info: Color::Rgb(132, 191, 200),
        accent: Color::Rgb(211, 170, 211),
        selected_row: Color::Rgb(39, 68, 51),
        search_fg: Color::Rgb(17, 29, 19),
        search_bg: Color::Rgb(131, 213, 166),
    },
    Theme {
        name: "Violet Pulse",
        bg: Color::Rgb(25, 19, 34),
        panel: Color::Rgb(43, 32, 55),
        border: Color::Rgb(130, 108, 150),
        muted: Color::Rgb(198, 185, 211),
        text: Color::Rgb(249, 244, 252),
        primary: Color::Rgb(206, 174, 235),
        success: Color::Rgb(141, 217, 184),
        warning: Color::Rgb(241, 198, 124),
        info: Color::Rgb(151, 190, 232),
        accent: Color::Rgb(231, 164, 204),
        selected_row: Color::Rgb(72, 53, 87),
        search_fg: Color::Rgb(28, 20, 35),
        search_bg: Color::Rgb(206, 174, 235),
    },
    Theme {
        name: "Arctic Blue",
        bg: Color::Rgb(12, 25, 37),
        panel: Color::Rgb(22, 43, 59),
        border: Color::Rgb(94, 126, 146),
        muted: Color::Rgb(182, 203, 216),
        text: Color::Rgb(242, 248, 252),
        primary: Color::Rgb(120, 206, 223),
        success: Color::Rgb(132, 218, 178),
        warning: Color::Rgb(239, 200, 119),
        info: Color::Rgb(148, 193, 230),
        accent: Color::Rgb(211, 174, 210),
        selected_row: Color::Rgb(39, 67, 87),
        search_fg: Color::Rgb(14, 29, 39),
        search_bg: Color::Rgb(120, 206, 223),
    },
    Theme {
        name: "Monochrome Max",
        bg: Color::Rgb(12, 12, 13),
        panel: Color::Rgb(27, 27, 29),
        border: Color::Rgb(119, 119, 122),
        muted: Color::Rgb(196, 196, 198),
        text: Color::Rgb(248, 248, 249),
        primary: Color::Rgb(212, 219, 226),
        success: Color::Rgb(166, 219, 180),
        warning: Color::Rgb(239, 206, 133),
        info: Color::Rgb(166, 201, 226),
        accent: Color::Rgb(218, 183, 216),
        selected_row: Color::Rgb(57, 57, 61),
        search_fg: Color::Rgb(15, 15, 16),
        search_bg: Color::Rgb(212, 219, 226),
    },
];

#[cfg(test)]
mod tests {
    use ratatui::style::Color;

    use super::THEMES;

    fn luminance(color: Color) -> f64 {
        let Color::Rgb(red, green, blue) = color else {
            panic!("built-in theme colors must use RGB values");
        };

        fn channel(value: u8) -> f64 {
            let value = f64::from(value) / 255.0;
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        }

        0.2126 * channel(red) + 0.7152 * channel(green) + 0.0722 * channel(blue)
    }

    fn contrast_ratio(foreground: Color, background: Color) -> f64 {
        let (lighter, darker) = {
            let foreground = luminance(foreground);
            let background = luminance(background);
            if foreground >= background {
                (foreground, background)
            } else {
                (background, foreground)
            }
        };
        (lighter + 0.05) / (darker + 0.05)
    }

    #[test]
    fn provides_nine_named_high_contrast_palettes() {
        assert_eq!(THEMES.len(), 9);
        assert!(THEMES.iter().all(|theme| !theme.name.is_empty()));
    }

    #[test]
    fn every_palette_has_distinct_name() {
        for (index, theme) in THEMES.iter().enumerate() {
            assert!(!THEMES[..index].iter().any(|previous| previous.name == theme.name));
        }
    }

    #[test]
    fn readable_foregrounds_meet_wcag_aa_contrast_against_panels() {
        for theme in THEMES {
            for foreground in [
                theme.muted,
                theme.text,
                theme.primary,
                theme.success,
                theme.warning,
                theme.info,
                theme.accent,
            ] {
                assert!(
                    contrast_ratio(foreground, theme.panel) >= 4.5,
                    "{} has a foreground color below 4.5:1 contrast against its panel",
                    theme.name
                );
            }
        }
    }

    #[test]
    fn search_highlight_color_follows_each_theme_primary_color() {
        for theme in THEMES {
            assert_eq!(
                theme.search_bg, theme.primary,
                "{} search highlight should use its palette's primary color",
                theme.name
            );
        }
    }

    #[test]
    fn selected_rows_and_search_highlights_remain_readable() {
        for theme in THEMES {
            for foreground in [
                theme.muted,
                theme.text,
                theme.primary,
                theme.success,
                theme.warning,
                theme.info,
                theme.accent,
            ] {
                assert!(
                    contrast_ratio(foreground, theme.selected_row) >= 4.5,
                    "{} has a selected-row foreground below 4.5:1 contrast",
                    theme.name
                );
            }

            assert!(
                contrast_ratio(theme.search_fg, theme.search_bg) >= 4.5,
                "{} has a search highlight below 4.5:1 contrast",
                theme.name
            );
        }
    }
}
