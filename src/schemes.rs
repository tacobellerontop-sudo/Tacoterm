//! Built-in color schemes, picked with `color_scheme = "…"` in the config.

pub struct Scheme {
    pub name: &'static str,
    pub background: &'static str,
    pub foreground: &'static str,
    pub cursor: &'static str,
    pub selection: &'static str,
    /// 0-7 normal, 8-15 bright.
    pub palette: [&'static str; 16],
}

pub const SCHEMES: &[Scheme] = &[
    Scheme {
        name: "tokyo-night",
        background: "#1a1b26",
        foreground: "#c0caf5",
        cursor: "#c0caf5",
        selection: "#33467c",
        palette: [
            "#15161e", "#f7768e", "#9ece6a", "#e0af68", "#7aa2f7", "#bb9af7", "#7dcfff", "#a9b1d6",
            "#414868", "#f7768e", "#9ece6a", "#e0af68", "#7aa2f7", "#bb9af7", "#7dcfff", "#c0caf5",
        ],
    },
    Scheme {
        name: "catppuccin-mocha",
        background: "#1e1e2e",
        foreground: "#cdd6f4",
        cursor: "#f5e0dc",
        selection: "#585b70",
        palette: [
            "#45475a", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#bac2de",
            "#585b70", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#a6adc8",
        ],
    },
    Scheme {
        name: "dracula",
        background: "#282a36",
        foreground: "#f8f8f2",
        cursor: "#f8f8f2",
        selection: "#44475a",
        palette: [
            "#21222c", "#ff5555", "#50fa7b", "#f1fa8c", "#bd93f9", "#ff79c6", "#8be9fd", "#f8f8f2",
            "#6272a4", "#ff6e6e", "#69ff94", "#ffffa5", "#d6acff", "#ff92df", "#a4ffff", "#ffffff",
        ],
    },
    Scheme {
        name: "one-half-dark",
        background: "#282c34",
        foreground: "#dcdfe4",
        cursor: "#a3b3cc",
        selection: "#474e5d",
        palette: [
            "#282c34", "#e06c75", "#98c379", "#e5c07b", "#61afef", "#c678dd", "#56b6c2", "#dcdfe4",
            "#5a6374", "#e06c75", "#98c379", "#e5c07b", "#61afef", "#c678dd", "#56b6c2", "#dcdfe4",
        ],
    },
    Scheme {
        name: "gruvbox-dark",
        background: "#282828",
        foreground: "#ebdbb2",
        cursor: "#ebdbb2",
        selection: "#504945",
        palette: [
            "#282828", "#cc241d", "#98971a", "#d79921", "#458588", "#b16286", "#689d6a", "#a89984",
            "#928374", "#fb4934", "#b8bb26", "#fabd2f", "#83a598", "#d3869b", "#8ec07c", "#ebdbb2",
        ],
    },
    Scheme {
        name: "nord",
        background: "#2e3440",
        foreground: "#d8dee9",
        cursor: "#d8dee9",
        selection: "#434c5e",
        palette: [
            "#3b4252", "#bf616a", "#a3be8c", "#ebcb8b", "#81a1c1", "#b48ead", "#88c0d0", "#e5e9f0",
            "#4c566a", "#bf616a", "#a3be8c", "#ebcb8b", "#81a1c1", "#b48ead", "#8fbcbb", "#eceff4",
        ],
    },
    Scheme {
        name: "solarized-dark",
        background: "#002b36",
        foreground: "#839496",
        cursor: "#93a1a1",
        selection: "#274642",
        palette: [
            "#073642", "#dc322f", "#859900", "#b58900", "#268bd2", "#d33682", "#2aa198", "#eee8d5",
            "#002b36", "#cb4b16", "#586e75", "#657b83", "#839496", "#6c71c4", "#93a1a1", "#fdf6e3",
        ],
    },
    Scheme {
        name: "one-half-light",
        background: "#fafafa",
        foreground: "#383a42",
        cursor: "#4f525d",
        selection: "#bfceff",
        palette: [
            "#383a42", "#e45649", "#50a14f", "#c18301", "#0184bc", "#a626a4", "#0997b3", "#fafafa",
            "#4f525e", "#e06c75", "#98c379", "#e5c07b", "#61afef", "#c678dd", "#56b6c2", "#ffffff",
        ],
    },
    Scheme {
        name: "campbell",
        background: "#0c0c0c",
        foreground: "#cccccc",
        cursor: "#cccccc",
        selection: "#264f78",
        palette: [
            "#0c0c0c", "#c50f1f", "#13a10e", "#c19c00", "#0037da", "#881798", "#3a96dd", "#cccccc",
            "#767676", "#e74856", "#16c60c", "#f9f1a5", "#3b78ff", "#b4009e", "#61d6d6", "#f2f2f2",
        ],
    },
];

/// Look a scheme up by name, ignoring case, spaces and underscores
/// (`"Tokyo Night"`, `"tokyo_night"` and `"tokyo-night"` all match).
pub fn find(name: &str) -> Option<&'static Scheme> {
    let want = normalize(name);
    SCHEMES.iter().find(|s| s.name == want)
}

/// The scheme after `name` in [`SCHEMES`], wrapping around.
pub fn next(name: &str) -> &'static Scheme {
    let want = normalize(name);
    let i = SCHEMES.iter().position(|s| s.name == want);
    &SCHEMES[i.map_or(0, |i| (i + 1) % SCHEMES.len())]
}

fn normalize(name: &str) -> String {
    name.trim().to_lowercase().replace([' ', '_'], "-")
}

pub fn names() -> String {
    SCHEMES
        .iter()
        .map(|s| s.name)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse_hex;

    #[test]
    fn every_scheme_parses() {
        for s in SCHEMES {
            for c in [s.background, s.foreground, s.cursor, s.selection]
                .into_iter()
                .chain(s.palette)
            {
                assert!(parse_hex(c).is_some(), "{}: bad color {c}", s.name);
            }
        }
    }

    #[test]
    fn lookup_is_forgiving_and_cycles() {
        assert_eq!(find("Tokyo Night").unwrap().name, "tokyo-night");
        assert_eq!(find("one_half_dark").unwrap().name, "one-half-dark");
        assert!(find("nope").is_none());
        assert_eq!(next("campbell").name, SCHEMES[0].name);
        assert_eq!(next("tokyo-night").name, SCHEMES[1].name);
    }
}
