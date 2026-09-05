use std::path::PathBuf;
use std::str::FromStr;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Theme {
    #[default]
    Default,
    Catppuccin,
    AyuDark,
    Ayuppuccin,
    Custom(String),
}

impl FromStr for Theme {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "default" | "kamui" => Ok(Self::Default),
            "catppuccin" | "catppuccin-mocha" | "mocha" => Ok(Self::Catppuccin),
            "ayu" | "ayu-dark" | "ayu_dark" => Ok(Self::AyuDark),
            "ayuppuccin" | "ayu-catppuccin" | "ayuppuccin-dark" => Ok(Self::Ayuppuccin),
            other => {
                // allow custom theme names (a-z,0-9,-,_)
                if custom_exists(other) {
                    Ok(Self::Custom(other.to_string()))
                } else {
                    Err(format!(
                        "unknown theme '{other}' (expected: default, catppuccin, ayu-dark, ayuppuccin or a file in ~/.config/kamui/themes/<name>.json)"
                    ))
                }
            }
        }
    }
}
impl std::fmt::Display for Theme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Default => f.write_str("default"),
            Self::Catppuccin => f.write_str("catppuccin"),
            Self::AyuDark => f.write_str("ayu-dark"),
            Self::Ayuppuccin => f.write_str("ayuppuccin"),
            Self::Custom(s) => f.write_str(s),
        }
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Palette {
    pub bg: String,
    pub fg: String,
    pub muted: String,
    pub mauve: String,
    pub blue: String,
    pub green: String,
    pub red: String,
    pub amber: String,
    pub teal: String,
    pub cyan: String,
}

impl Theme {
    pub fn palette(&self) -> Option<Palette> {
        match self {
            Self::Default => None,
            Self::Catppuccin => Some(Palette {
                bg: "#1e1e2e".into(),
                fg: "#cdd6f4".into(),
                muted: "#6c7086".into(),
                mauve: "#cba6f7".into(),
                blue: "#89b4fa".into(),
                green: "#a6e3a1".into(),
                red: "#f38ba8".into(),
                amber: "#fab387".into(),
                teal: "#94e2d5".into(),
                cyan: "#89dceb".into(),
            }),
            Self::AyuDark => Some(Palette {
                bg: "#0a0e14".into(),
                fg: "#bfbdb6".into(),
                muted: "#5c6773".into(),
                mauve: "#d4bfff".into(),
                blue: "#59c2ff".into(),
                green: "#aad94c".into(),
                red: "#f07178".into(),
                amber: "#ffb454".into(),
                teal: "#95e6cb".into(),
                cyan: "#95e6cb".into(),
            }),
            Self::Ayuppuccin => Some(Palette {
                bg: "#2c2c2e".into(),
                fg: "#bfbdb6".into(),
                muted: "#8a8986".into(),
                mauve: "#cba6f7".into(),
                blue: "#5ac1fe".into(),
                green: "#a9d94b".into(),
                red: "#ef7177".into(),
                amber: "#feb454".into(),
                teal: "#94e2d5".into(),
                cyan: "#95e6cb".into(),
            }),
            Self::Custom(name) => load_custom_palette(name),
        }
    }
    pub fn all() -> Vec<Theme> {
        let mut v = vec![
            Self::Default,
            Self::Catppuccin,
            Self::AyuDark,
            Self::Ayuppuccin,
        ];
        for n in list_custom_names() {
            v.push(Self::Custom(n));
        }
        v
    }
}

pub fn themes_dir() -> PathBuf {
    crate::config::global_config_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("themes")
}
fn custom_exists(name: &str) -> bool {
    themes_dir().join(format!("{name}.json")).is_file()
}
fn list_custom_names() -> Vec<String> {
    let dir = themes_dir();
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return vec![];
    };
    let mut out = vec![];
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) == Some("json")
            && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
        {
            out.push(stem.to_string());
        }
    }
    out.sort();
    out
}
fn load_custom_palette(name: &str) -> Option<Palette> {
    let path = themes_dir().join(format!("{name}.json"));
    let data = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&data).ok()?;
    // support either flat {bg,fg,...} or {defs:{}, theme:{}} like ayuppuccin.json
    let get = |k: &str| v.get(k).and_then(|x| x.as_str()).map(|s| s.to_string());
    // if has defs, resolve refs
    if let Some(defs) = v.get("defs") {
        let resolve = |key: &str| {
            // theme.<key> may be {dark:"mauve"} -> resolve via defs
            let theme = v.get("theme")?.get(key)?;
            let ref_name = theme
                .get("dark")
                .and_then(|x| x.as_str())
                .or_else(|| theme.as_str())?;
            defs.get(ref_name)
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
                .or_else(|| Some(ref_name.to_string()))
        };
        return Some(Palette {
            bg: resolve("background")?,
            fg: resolve("text")?,
            muted: resolve("textMuted").or_else(|| get("fg_muted"))?,
            mauve: defs
                .get("mauve")
                .and_then(|x| x.as_str())
                .unwrap_or("#cba6f7")
                .to_string(),
            blue: defs
                .get("ayu_blue")
                .and_then(|x| x.as_str())
                .or_else(|| defs.get("blue").and_then(|x| x.as_str()))
                .unwrap_or("#89b4fa")
                .to_string(),
            green: defs
                .get("ayu_green")
                .and_then(|x| x.as_str())
                .or_else(|| defs.get("green").and_then(|x| x.as_str()))
                .unwrap_or("#a6e3a1")
                .to_string(),
            red: defs
                .get("ayu_red")
                .and_then(|x| x.as_str())
                .or_else(|| defs.get("red").and_then(|x| x.as_str()))
                .unwrap_or("#f38ba8")
                .to_string(),
            amber: defs
                .get("ayu_amber")
                .and_then(|x| x.as_str())
                .or_else(|| defs.get("amber").and_then(|x| x.as_str()))
                .unwrap_or("#fab387")
                .to_string(),
            teal: defs
                .get("teal")
                .and_then(|x| x.as_str())
                .unwrap_or("#94e2d5")
                .to_string(),
            cyan: defs
                .get("teal")
                .and_then(|x| x.as_str())
                .unwrap_or("#89dceb")
                .to_string(),
        });
    }
    Some(Palette {
        bg: get("bg")?,
        fg: get("fg")?,
        muted: get("muted").or_else(|| get("fg_muted"))?,
        mauve: get("mauve").unwrap_or("#cba6f7".into()),
        blue: get("blue").unwrap_or("#89b4fa".into()),
        green: get("green").unwrap_or("#a6e3a1".into()),
        red: get("red").unwrap_or("#f38ba8".into()),
        amber: get("amber").unwrap_or("#fab387".into()),
        teal: get("teal").unwrap_or("#94e2d5".into()),
        cyan: get("cyan").unwrap_or("#89dceb".into()),
    })
}

#[allow(dead_code)]
pub fn hex_to_rgb(hex: &str) -> (u8, u8, u8) {
    let h = hex.trim_start_matches('#');
    let r = u8::from_str_radix(&h[0..2], 16).unwrap_or(0);
    let g = u8::from_str_radix(&h[2..4], 16).unwrap_or(0);
    let b = u8::from_str_radix(&h[4..6], 16).unwrap_or(0);
    (r, g, b)
}
#[allow(dead_code)]
pub fn fg_true(hex: &str) -> String {
    let (r, g, b) = hex_to_rgb(hex);
    format!("\x1b[38;2;{r};{g};{b}m")
}
#[allow(dead_code)]
pub fn bg_true(hex: &str) -> String {
    let (r, g, b) = hex_to_rgb(hex);
    format!("\x1b[48;2;{r};{g};{b}m")
}
#[allow(dead_code)]
pub fn ratatui_fg(hex: &str) -> ratatui::style::Color {
    let (r, g, b) = hex_to_rgb(hex);
    ratatui::style::Color::Rgb(r, g, b)
}
