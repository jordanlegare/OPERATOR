use ratatui::style::Color;

/// High-contrast UI palette. All foreground colors are selected for legibility
/// against the theme's background and panel colors.
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

/// Built-in palettes, selected with Ctrl+Shift+1 through Ctrl+Shift+9.
pub const THEMES: [Theme; 9] = [
    Theme {
        name: "Abyss Cyan",
        bg: Color::Rgb(7, 13, 24),
        panel: Color::Rgb(16, 30, 48),
        border: Color::Rgb(66, 98, 127),
        muted: Color::Rgb(167, 184, 204),
        text: Color::Rgb(244, 248, 255),
        primary: Color::Rgb(80, 230, 230),
        success: Color::Rgb(124, 250, 174),
        warning: Color::Rgb(255, 209, 102),
        info: Color::Rgb(130, 182, 255),
        accent: Color::Rgb(231, 168, 255),
        selected_row: Color::Rgb(35, 59, 84),
        search_fg: Color::Rgb(6, 16, 27),
        search_bg: Color::Rgb(255, 228, 94),
    },
    Theme {
        name: "Carbon Amber",
        bg: Color::Rgb(0, 0, 0),
        panel: Color::Rgb(17, 17, 17),
        border: Color::Rgb(112, 112, 112),
        muted: Color::Rgb(194, 194, 194),
        text: Color::Rgb(255, 255, 255),
        primary: Color::Rgb(255, 176, 0),
        success: Color::Rgb(0, 255, 102),
        warning: Color::Rgb(255, 230, 0),
        info: Color::Rgb(66, 191, 255),
        accent: Color::Rgb(255, 92, 138),
        selected_row: Color::Rgb(48, 48, 48),
        search_fg: Color::Rgb(0, 0, 0),
        search_bg: Color::Rgb(255, 255, 0),
    },
    Theme {
        name: "Dracula Contrast",
        bg: Color::Rgb(23, 21, 32),
        panel: Color::Rgb(36, 34, 54),
        border: Color::Rgb(101, 91, 130),
        muted: Color::Rgb(193, 187, 215),
        text: Color::Rgb(248, 247, 255),
        primary: Color::Rgb(139, 233, 253),
        success: Color::Rgb(80, 250, 123),
        warning: Color::Rgb(241, 250, 140),
        info: Color::Rgb(189, 147, 249),
        accent: Color::Rgb(255, 121, 198),
        selected_row: Color::Rgb(59, 54, 86),
        search_fg: Color::Rgb(20, 18, 29),
        search_bg: Color::Rgb(255, 223, 112),
    },
    Theme {
        name: "Solarized Light",
        bg: Color::Rgb(253, 246, 227),
        panel: Color::Rgb(238, 232, 213),
        border: Color::Rgb(101, 123, 131),
        muted: Color::Rgb(88, 110, 117),
        text: Color::Rgb(0, 43, 54),
        primary: Color::Rgb(0, 93, 125),
        success: Color::Rgb(20, 105, 48),
        warning: Color::Rgb(139, 73, 0),
        info: Color::Rgb(36, 76, 151),
        accent: Color::Rgb(139, 42, 105),
        selected_row: Color::Rgb(211, 225, 224),
        search_fg: Color::Rgb(255, 255, 255),
        search_bg: Color::Rgb(121, 65, 0),
    },
    Theme {
        name: "Paper High Contrast",
        bg: Color::Rgb(255, 255, 255),
        panel: Color::Rgb(242, 245, 249),
        border: Color::Rgb(55, 65, 81),
        muted: Color::Rgb(55, 65, 81),
        text: Color::Rgb(15, 23, 42),
        primary: Color::Rgb(0, 82, 170),
        success: Color::Rgb(0, 105, 54),
        warning: Color::Rgb(142, 73, 0),
        info: Color::Rgb(0, 79, 146),
        accent: Color::Rgb(151, 0, 91),
        selected_row: Color::Rgb(211, 230, 255),
        search_fg: Color::Rgb(0, 0, 0),
        search_bg: Color::Rgb(255, 214, 0),
    },
    Theme {
        name: "Forest Signal",
        bg: Color::Rgb(7, 26, 18),
        panel: Color::Rgb(15, 42, 30),
        border: Color::Rgb(62, 112, 86),
        muted: Color::Rgb(180, 209, 189),
        text: Color::Rgb(242, 255, 245),
        primary: Color::Rgb(110, 231, 183),
        success: Color::Rgb(163, 230, 53),
        warning: Color::Rgb(255, 209, 102),
        info: Color::Rgb(125, 211, 252),
        accent: Color::Rgb(240, 171, 252),
        selected_row: Color::Rgb(33, 71, 53),
        search_fg: Color::Rgb(10, 22, 15),
        search_bg: Color::Rgb(217, 249, 157),
    },
    Theme {
        name: "Violet Pulse",
        bg: Color::Rgb(22, 11, 36),
        panel: Color::Rgb(36, 19, 57),
        border: Color::Rgb(104, 74, 143),
        muted: Color::Rgb(209, 191, 230),
        text: Color::Rgb(255, 248, 255),
        primary: Color::Rgb(214, 168, 255),
        success: Color::Rgb(131, 242, 197),
        warning: Color::Rgb(255, 209, 102),
        info: Color::Rgb(139, 197, 255),
        accent: Color::Rgb(255, 141, 185),
        selected_row: Color::Rgb(60, 36, 88),
        search_fg: Color::Rgb(23, 14, 34),
        search_bg: Color::Rgb(253, 230, 138),
    },
    Theme {
        name: "Arctic Blue",
        bg: Color::Rgb(7, 21, 33),
        panel: Color::Rgb(16, 43, 61),
        border: Color::Rgb(79, 113, 132),
        muted: Color::Rgb(193, 213, 226),
        text: Color::Rgb(245, 251, 255),
        primary: Color::Rgb(103, 232, 249),
        success: Color::Rgb(134, 239, 172),
        warning: Color::Rgb(253, 224, 71),
        info: Color::Rgb(147, 197, 253),
        accent: Color::Rgb(249, 168, 212),
        selected_row: Color::Rgb(35, 70, 91),
        search_fg: Color::Rgb(16, 32, 42),
        search_bg: Color::Rgb(253, 224, 71),
    },
    Theme {
        name: "Monochrome Max",
        bg: Color::Rgb(0, 0, 0),
        panel: Color::Rgb(18, 18, 18),
        border: Color::Rgb(133, 133, 133),
        muted: Color::Rgb(214, 214, 214),
        text: Color::Rgb(255, 255, 255),
        primary: Color::Rgb(0, 255, 255),
        success: Color::Rgb(0, 255, 0),
        warning: Color::Rgb(255, 255, 0),
        info: Color::Rgb(77, 159, 255),
        accent: Color::Rgb(255, 102, 255),
        selected_row: Color::Rgb(48, 48, 48),
        search_fg: Color::Rgb(0, 0, 0),
        search_bg: Color::Rgb(255, 255, 255),
    },
];

#[cfg(test)]
mod tests {
    use super::THEMES;

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
}
