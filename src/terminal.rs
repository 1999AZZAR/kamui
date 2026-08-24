use std::io::IsTerminal;

pub const RESET: &str = "\x1b[0m";
pub const BOLD: &str = "\x1b[1m";
pub const DIM: &str = "\x1b[2m";
pub const CYAN: &str = "\x1b[36m";
pub const GREEN: &str = "\x1b[32m";
pub const RED: &str = "\x1b[31m";
pub const YELLOW: &str = "\x1b[33m";
pub const BLUE: &str = "\x1b[34m";
pub const WHITE: &str = "\x1b[37m";
pub const BLACK: &str = "\x1b[30m";
pub const BG_BLUE: &str = "\x1b[48;5;24m";
pub const BG_GREEN: &str = "\x1b[48;5;29m";
pub const BG_RED: &str = "\x1b[48;5;52m";
pub const BG_GRAY: &str = "\x1b[48;5;238m";
pub const BG_DARK: &str = "\x1b[48;5;236m";

/// Minimal semantic styles; `Ui::style` applies one or more, honouring the colour gate.
#[allow(dead_code)]
#[derive(Clone, Copy)]
pub enum Style {
    Bold,
    Dim,
    Cyan,
    Green,
    Red,
    Yellow,
    #[expect(dead_code)] // wired to markdown styling in a parallel change
    Blue,
    White,
    Black,
    BgBlue,
    BgGreen,
    BgRed,
    BgGray,
    BgDark,
}

impl Style {
    fn code(self) -> &'static str {
        match self {
            Style::Bold => BOLD,
            Style::Dim => DIM,
            Style::Cyan => CYAN,
            Style::Green => GREEN,
            Style::Red => RED,
            Style::Yellow => YELLOW,
            Style::Blue => BLUE,
            Style::White => WHITE,
            Style::Black => BLACK,
            Style::BgBlue => BG_BLUE,
            Style::BgGreen => BG_GREEN,
            Style::BgRed => BG_RED,
            Style::BgGray => BG_GRAY,
            Style::BgDark => BG_DARK,
        }
    }
}

#[derive(Clone, Copy)]
pub struct Ui {
    interactive: bool,
    color: bool,
}

impl Ui {
    pub fn stdio() -> Self {
        let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
        Self {
            interactive,
            color: interactive && std::env::var_os("NO_COLOR").is_none(),
        }
    }

    pub fn interactive(self) -> bool {
        self.interactive
    }

    #[cfg(test)]
    pub fn plain() -> Self {
        Self {
            interactive: false,
            color: false,
        }
    }

    /// Wrap `text` in the given styles, or return it plain when colour is disabled.
    pub fn style(self, text: &str, styles: &[Style]) -> String {
        if !self.color {
            return text.to_string();
        }
        let mut out = String::with_capacity(text.len() + styles.len() * 5 + 4);
        for style in styles {
            out.push_str(style.code());
        }
        out.push_str(text);
        out.push_str(RESET);
        out
    }
}
