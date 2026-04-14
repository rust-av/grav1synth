/// Built-in film grain presets embedded directly into the binary at compile time.
///
/// Presets are split into two groups:
///
/// **Standalone** presets (`Super8`, `MaxMid`) are used exactly as named.
///
/// **Format** presets (`16mm`, `Classic35`, `Modern35`) support an optional film stock
/// modifier suffix (`-1`, `-2`, `-3`). Omitting the suffix selects the default stock
/// (Fujifilm Eterna 250D). Example valid names: `16mm`, `16mm-1`, `Classic35-2`.
///
/// All lookups are case-insensitive.

// ── Film stock modifier table ─────────────────────────────────────────────────

/// One film stock variant available for format-based presets.
pub struct FilmStock {
    /// Command-line suffix (empty string = default, otherwise `-1` / `-2` / `-3`).
    pub suffix: &'static str,
    /// Human-readable label shown in `grav1synth presets`.
    pub description: &'static str,
}

/// The four film stocks, indexed so that `FILM_STOCKS[0]` is always the default.
pub const FILM_STOCKS: &[FilmStock] = &[
    FilmStock {
        suffix: "",
        description: "Fujifilm Eterna 250D",
    },
    FilmStock {
        suffix: "-1",
        description: "Fujifilm Eterna 500T",
    },
    FilmStock {
        suffix: "-2",
        description: "Kodak Vision3 250D",
    },
    FilmStock {
        suffix: "-3",
        description: "Kodak Vision3 200T",
    },
];

// ── Format presets (support film stock modifiers) ─────────────────────────────

/// A preset that supports film stock modifier suffixes.
pub struct FormatPreset {
    /// Base name used on the command line (e.g. `16mm`).
    pub name: &'static str,
    /// Description shown in `grav1synth presets`.
    pub description: &'static str,
    /// Grain table data for each film stock, in the same order as `FILM_STOCKS`.
    pub stocks: [&'static str; 4],
}

pub const FORMAT_PRESETS: &[FormatPreset] = &[
    FormatPreset {
        name: "16mm",
        description: "Based on 16mm film size",
        stocks: [
            include_str!("../grain-files/16mm_FJ_8543_VD.txt"),  // default
            include_str!("../grain-files/16mm_FJ_8553_ET.txt"),   // -1
            include_str!("../grain-files/16mm_KD_5207_Vis3.txt"), // -2
            include_str!("../grain-files/16mm_KD_5213_Vis3.txt"), // -3
        ],
    },
    FormatPreset {
        name: "Classic35",
        description: "Based on Super 35mm film size",
        stocks: [
            include_str!("../grain-files/Classic35_FJ_8543_VD.txt"),
            include_str!("../grain-files/Classic35_FJ_8553_ET.txt"),
            include_str!("../grain-files/Classic35_KD_5207_Vis3.txt"),
            include_str!("../grain-files/Classic35_KD_5213_Vis3.txt"),
        ],
    },
    FormatPreset {
        name: "Modern35",
        description: "Based on 35mm Full Frame film size",
        stocks: [
            include_str!("../grain-files/Modern35_FJ_8543_VD.txt"),
            include_str!("../grain-files/Modern35_FJ_8553_ET.txt"),
            include_str!("../grain-files/Modern35_KD_5207_Vis3.txt"),
            include_str!("../grain-files/Modern35_KD_5213_Vis3.txt"),
        ],
    },
];

// ── Standalone presets (no film stock modifier) ───────────────────────────────

/// A preset that is used exactly as named, with no modifier suffixes.
pub struct StandalonePreset {
    pub name: &'static str,
    pub description: &'static str,
    pub data: &'static str,
}

pub const STANDALONE_PRESETS: &[StandalonePreset] = &[
    StandalonePreset {
        name: "Super8",
        description: "Based on Super 8mm film size",
        data: include_str!("../grain-files/Super8.txt"),
    },
    StandalonePreset {
        name: "MaxMid",
        description: "Custom synthetic heavy midtone grain",
        data: include_str!("../grain-files/M_MaxMid.txt"),
    },
];

// ── Lookup ────────────────────────────────────────────────────────────────────

/// Returns the grain table text for `name`, or `None` if the name is not recognised.
///
/// Matching is case-insensitive. For format presets, an optional suffix selects the
/// film stock: no suffix → default (Fujifilm Eterna 250D), `-1` → Eterna 500T,
/// `-2` → Kodak Vision3 250D, `-3` → Kodak Vision3 200T.
pub fn get_preset(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();

    // Check standalone presets first.
    for p in STANDALONE_PRESETS {
        if p.name.to_ascii_lowercase() == lower {
            return Some(p.data);
        }
    }

    // Check format presets with optional film stock suffix.
    for fp in FORMAT_PRESETS {
        let base = fp.name.to_ascii_lowercase();
        if lower == base {
            return Some(fp.stocks[0]);
        }
        for (i, stock) in FILM_STOCKS[1..].iter().enumerate() {
            if lower == format!("{base}{}", stock.suffix) {
                return Some(fp.stocks[i + 1]);
            }
        }
    }

    None
}

