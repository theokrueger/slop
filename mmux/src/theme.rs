use serde::{Deserialize, Serialize};

/// Terminal/UI appearance. Fixed for the whole mmux session because terminal
/// panes may start before a browser connects and applications query terminal
/// colors at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Appearance {
    #[default]
    Dark,
    Light,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// xterm OSC color format: 16-bit components with each byte doubled.
    pub fn xterm(self) -> String {
        format!(
            "{:02x}{:02x}/{:02x}{:02x}/{:02x}{:02x}",
            self.r, self.r, self.g, self.g, self.b, self.b
        )
    }
}

#[derive(Debug, Clone)]
pub struct Palette {
    pub background: Rgb,
    pub foreground: Rgb,
    pub cursor: Rgb,
    /// ANSI colors 0-15, matching the browser xterm theme.
    pub ansi: [Rgb; 16],
}

const BASE03: Rgb = Rgb::new(0x00, 0x2b, 0x36);
const BASE02: Rgb = Rgb::new(0x07, 0x36, 0x42);
const BASE01: Rgb = Rgb::new(0x58, 0x6e, 0x75);
const BASE00: Rgb = Rgb::new(0x65, 0x7b, 0x83);
const BASE1: Rgb = Rgb::new(0x93, 0xa1, 0xa1);
const BASE2: Rgb = Rgb::new(0xee, 0xe8, 0xd5);
const BASE3: Rgb = Rgb::new(0xfd, 0xf6, 0xe3);
const YELLOW: Rgb = Rgb::new(0xb5, 0x89, 0x00);
const ORANGE: Rgb = Rgb::new(0xcb, 0x4b, 0x16);
const RED: Rgb = Rgb::new(0xdc, 0x32, 0x2f);
const MAGENTA: Rgb = Rgb::new(0xd3, 0x36, 0x82);
const VIOLET: Rgb = Rgb::new(0x6c, 0x71, 0xc4);
const BLUE: Rgb = Rgb::new(0x26, 0x8b, 0xd2);
const CYAN: Rgb = Rgb::new(0x2a, 0xa1, 0x98);
const GREEN: Rgb = Rgb::new(0x85, 0x99, 0x00);

pub fn palette(appearance: Appearance) -> Palette {
    Palette {
        background: match appearance {
            Appearance::Dark => BASE03,
            Appearance::Light => BASE3,
        },
        foreground: match appearance {
            Appearance::Dark => BASE1,
            Appearance::Light => BASE00,
        },
        cursor: match appearance {
            Appearance::Dark => BASE1,
            Appearance::Light => BASE00,
        },
        // Bright slots use Solarized accents rather than canonical base grays,
        // so `01;32`/`01;34` prompts stay visibly green/blue.
        ansi: [
            BASE02, RED, GREEN, YELLOW, BLUE, MAGENTA, CYAN, BASE2, BASE01, ORANGE, GREEN, YELLOW,
            BLUE, VIOLET, CYAN, BASE1,
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xterm_color_format_doubles_components() {
        assert_eq!(BASE03.xterm(), "0000/2b2b/3636");
        assert_eq!(RED.xterm(), "dcdc/3232/2f2f");
    }

    #[test]
    fn dark_and_light_palettes_are_solarized() {
        assert_eq!(palette(Appearance::Dark).background, BASE03);
        assert_eq!(palette(Appearance::Dark).foreground, BASE1);
        assert_eq!(palette(Appearance::Light).background, BASE3);
        assert_eq!(palette(Appearance::Light).foreground, BASE00);
        assert_eq!(palette(Appearance::Dark).ansi[2], GREEN);
        assert_eq!(palette(Appearance::Dark).ansi[10], GREEN);
    }

    #[test]
    fn appearance_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&Appearance::Dark).unwrap(),
            "\"dark\""
        );
        assert_eq!(
            serde_json::to_string(&Appearance::Light).unwrap(),
            "\"light\""
        );
    }
}
