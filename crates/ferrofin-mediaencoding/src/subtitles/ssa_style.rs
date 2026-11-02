//! SSA/ASS styles as libse 4.0.12 reads them from a script header.
//!
//! Port of `Nikse.SubtitleEdit.Core.Common.SsaStyle` and the header helpers of
//! `AdvancedSubStationAlpha` (`GetStylesFromHeader`, `GetSsaStyle`, `GetSsaColor`,
//! `GetSsaColorString`) that the SSA and TTML writers use to carry a source ASS
//! script's styles over.

use super::dotnet::{
    Color, Decimal, color_from_html, eq_ordinal_ignore_case, hex_number_try_parse, int_try_parse,
};
use super::libse_text::split_to_lines;

/// `SsaStyle.DefaultAssStyleFormat`.
pub const DEFAULT_ASS_STYLE_FORMAT: &str = "Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding";

/// An SSA/ASS style (`SsaStyle`).
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::struct_excessive_bools)] // libse's `SsaStyle`, field for field
pub struct SsaStyle {
    /// `Name`.
    pub name: String,
    /// `FontName`.
    pub font_name: String,
    /// `FontSize`.
    pub font_size: Decimal,
    /// `Italic`.
    pub italic: bool,
    /// `Bold`.
    pub bold: bool,
    /// `Underline`.
    pub underline: bool,
    /// `Strikeout`.
    pub strikeout: bool,
    /// `Primary`.
    pub primary: Color,
    /// `Secondary`.
    pub secondary: Color,
    /// `Tertiary` (SSA only; `Color.Empty` unless a header sets it).
    pub tertiary: Color,
    /// `Outline`.
    pub outline: Color,
    /// `Background`.
    pub background: Color,
    /// `ShadowWidth`.
    pub shadow_width: Decimal,
    /// `OutlineWidth`.
    pub outline_width: Decimal,
    /// `Alignment`, as written.
    pub alignment: String,
    /// `MarginLeft`.
    pub margin_left: i32,
    /// `MarginRight`.
    pub margin_right: i32,
    /// `MarginVertical`.
    pub margin_vertical: i32,
    /// `ScaleX`.
    pub scale_x: Decimal,
    /// `ScaleY`.
    pub scale_y: Decimal,
    /// `Spacing`.
    pub spacing: Decimal,
    /// `Angle`.
    pub angle: Decimal,
    /// `BorderStyle`, as written.
    pub border_style: String,
}

impl Default for SsaStyle {
    /// `new SsaStyle()`.
    fn default() -> Self {
        Self {
            name: "Default".to_owned(),
            font_name: "Arial".to_owned(),
            font_size: Decimal::from_int(20),
            italic: false,
            bold: false,
            underline: false,
            strikeout: false,
            primary: Color::WHITE,
            secondary: Color::YELLOW,
            tertiary: Color::default(),
            outline: Color::BLACK,
            background: Color::BLACK,
            shadow_width: Decimal::from_int(1),
            outline_width: Decimal::from_int(1),
            alignment: "2".to_owned(),
            margin_left: 10,
            margin_right: 10,
            margin_vertical: 10,
            scale_x: Decimal::from_int(100),
            scale_y: Decimal::from_int(100),
            spacing: Decimal::from_int(0),
            angle: Decimal::from_int(0),
            border_style: "1".to_owned(),
        }
    }
}

impl SsaStyle {
    /// `SsaStyle.ToRawAss(DefaultAssStyleFormat)`: the `Style:` line for the
    /// default ASS style format.
    #[must_use]
    pub fn to_raw_ass(&self) -> String {
        let flag = |v: bool| if v { "-1" } else { "0" }.to_owned();
        let fields: Vec<String> = DEFAULT_ASS_STYLE_FORMAT["Format: ".len()..]
            .to_lowercase()
            .split(',')
            .map(|f| match f.trim() {
                "name" => self.name.clone(),
                "fontname" => self.font_name.clone(),
                "fontsize" => self.font_size.to_hash_format(1),
                "primarycolour" => ssa_color_string(self.primary),
                "secondarycolour" => ssa_color_string(self.secondary),
                "outlinecolour" => ssa_color_string(self.outline),
                "backcolour" => ssa_color_string(self.background),
                "bold" => flag(self.bold),
                "italic" => flag(self.italic),
                "underline" => flag(self.underline),
                "strikeout" => flag(self.strikeout),
                "outline" => self.outline_width.to_invariant_string(),
                "shadow" => self.shadow_width.to_invariant_string(),
                "alignment" => self.alignment.clone(),
                "marginl" => self.margin_left.to_string(),
                "marginr" => self.margin_right.to_string(),
                "marginv" => self.margin_vertical.to_string(),
                "borderstyle" => self.border_style.clone(),
                "encoding" => "1".to_owned(),
                "scalex" => self.scale_x.to_invariant_string(),
                "scaley" => self.scale_y.to_invariant_string(),
                "spacing" => self.spacing.to_invariant_string(),
                "angle" => self.angle.to_invariant_string(),
                _ => String::new(),
            })
            .collect();
        format!("Style: {}", fields.join(","))
    }
}

/// `AdvancedSubStationAlpha.GetSsaColorString`: `&HAABBGGRR`, alpha inverted.
#[must_use]
pub fn ssa_color_string(c: Color) -> String {
    format!("&H{:02X}{:02X}{:02X}{:02X}", 255 - c.a, c.b, c.g, c.r)
}

/// `AdvancedSubStationAlpha.GetStylesFromHeader`: the name of every `Style:` line,
/// in order.
///
/// libse also reads styles out of a TTML header; a header from Ferrofin's SSA/ASS
/// loaders never is one.
#[must_use]
pub fn styles_from_header(header: &str) -> Vec<String> {
    split_to_lines(header)
        .iter()
        .filter(|line| line.to_lowercase().starts_with("style:"))
        .filter_map(|line| {
            let end = line.find(',')?;
            (end > 0).then(|| line.get(6..end).unwrap_or("").trim().to_owned())
        })
        .collect()
}

/// `AdvancedSubStationAlpha.GetSsaStyle(styleName, header)`: the named style, its
/// columns read by the header's last `Format:` line before it; a style the header
/// does not define is the default style under that name.
#[must_use]
#[allow(clippy::too_many_lines)] // libse's one-loop `GetSsaStyle`, kept in its order
pub fn ssa_style_from_header(style_name: &str, header: &str) -> SsaStyle {
    #[derive(Default)]
    struct Columns {
        name: Option<usize>,
        font_name: Option<usize>,
        font_size: Option<usize>,
        primary: Option<usize>,
        secondary: Option<usize>,
        tertiary: Option<usize>,
        outline_colour: Option<usize>,
        back: Option<usize>,
        bold: Option<usize>,
        italic: Option<usize>,
        underline: Option<usize>,
        strikeout: Option<usize>,
        outline: Option<usize>,
        shadow: Option<usize>,
        alignment: Option<usize>,
        margin_l: Option<usize>,
        margin_r: Option<usize>,
        margin_v: Option<usize>,
        scale_x: Option<usize>,
        scale_y: Option<usize>,
        spacing: Option<usize>,
        angle: Option<usize>,
        border_style: Option<usize>,
    }

    let mut columns = Columns::default();
    let mut style = SsaStyle {
        name: style_name.to_owned(),
        ..SsaStyle::default()
    };
    for line in split_to_lines(header) {
        let s = line.trim().to_lowercase();
        if s.starts_with("format:") {
            // libse keeps the indexes of an earlier `Format:` line it does not override.
            if line.chars().count() > 10 {
                let lower = line.to_lowercase();
                for (i, f) in lower.get(8..).unwrap_or("").split(',').enumerate() {
                    let slot = match f.trim() {
                        "name" => &mut columns.name,
                        "fontname" => &mut columns.font_name,
                        "fontsize" => &mut columns.font_size,
                        "primarycolour" => &mut columns.primary,
                        "secondarycolour" => &mut columns.secondary,
                        "tertiarycolour" => &mut columns.tertiary,
                        "outlinecolour" => &mut columns.outline_colour,
                        "backcolour" => &mut columns.back,
                        "bold" => &mut columns.bold,
                        "italic" => &mut columns.italic,
                        "underline" => &mut columns.underline,
                        "strikeout" => &mut columns.strikeout,
                        "outline" => &mut columns.outline,
                        "shadow" => &mut columns.shadow,
                        "alignment" => &mut columns.alignment,
                        "marginl" => &mut columns.margin_l,
                        "marginr" => &mut columns.margin_r,
                        "marginv" => &mut columns.margin_v,
                        "scalex" => &mut columns.scale_x,
                        "scaley" => &mut columns.scale_y,
                        "spacing" => &mut columns.spacing,
                        "angle" => &mut columns.angle,
                        "borderstyle" => &mut columns.border_style,
                        _ => continue,
                    };
                    *slot = Some(i);
                }
            }
        } else if s.replace(' ', "").starts_with("style:") {
            if line.chars().count() > 10 {
                for (i, raw) in line.get(6..).unwrap_or("").split(',').enumerate() {
                    let f = raw.trim();
                    let at = |slot: Option<usize>| slot == Some(i);
                    let flag = f == "-1" || f == "1";
                    // The first column that matches wins, as in libse's if/else chain.
                    if at(columns.name) {
                        f.clone_into(&mut style.name);
                    } else if at(columns.font_name) {
                        f.clone_into(&mut style.font_name);
                    } else if at(columns.font_size) {
                        if let Some(v) = Decimal::try_parse(f, false) {
                            style.font_size = v;
                        }
                    } else if at(columns.primary) {
                        style.primary = ssa_color(f, Color::WHITE);
                    } else if at(columns.secondary) {
                        style.secondary = ssa_color(f, Color::YELLOW);
                    } else if at(columns.tertiary) {
                        style.tertiary = ssa_color(f, Color::YELLOW);
                    } else if at(columns.outline_colour) {
                        style.outline = ssa_color(f, Color::BLACK);
                    } else if at(columns.back) {
                        style.background = ssa_color(f, Color::BLACK);
                    } else if at(columns.bold) {
                        style.bold = flag;
                    } else if at(columns.italic) {
                        style.italic = flag;
                    } else if at(columns.underline) {
                        style.underline = flag;
                    } else if at(columns.strikeout) {
                        style.strikeout = flag;
                    } else if at(columns.outline) {
                        if let Some(v) = Decimal::try_parse(f, false) {
                            style.outline_width = v;
                        }
                    } else if at(columns.shadow) {
                        if let Some(v) = Decimal::try_parse(f, false) {
                            style.shadow_width = v;
                        }
                    } else if at(columns.alignment) {
                        f.clone_into(&mut style.alignment);
                    } else if at(columns.margin_l) {
                        if let Some(v) = int_try_parse(f) {
                            style.margin_left = v;
                        }
                    } else if at(columns.margin_r) {
                        if let Some(v) = int_try_parse(f) {
                            style.margin_right = v;
                        }
                    } else if at(columns.margin_v) {
                        if let Some(v) = int_try_parse(f) {
                            style.margin_vertical = v;
                        }
                    } else if at(columns.scale_x) {
                        if let Some(v) = Decimal::try_parse(f, false) {
                            style.scale_x = v;
                        }
                    } else if at(columns.scale_y) {
                        if let Some(v) = Decimal::try_parse(f, false) {
                            style.scale_y = v;
                        }
                    } else if at(columns.spacing) {
                        if let Some(v) = Decimal::try_parse(f, false) {
                            style.spacing = v;
                        }
                    } else if at(columns.angle) {
                        if let Some(v) = Decimal::try_parse(f, true) {
                            style.angle = v;
                        }
                    } else if at(columns.border_style) {
                        f.clone_into(&mut style.border_style);
                    }
                }
            }
            if eq_ordinal_ignore_case(style_name, &style.name)
                || (eq_ordinal_ignore_case(style_name, "*Default")
                    && eq_ordinal_ignore_case(&style.name, "Default"))
            {
                return style;
            }
        }
    }
    SsaStyle {
        name: style_name.to_owned(),
        ..SsaStyle::default()
    }
}

/// `AdvancedSubStationAlpha.GetSsaColor`: `&HBBGGRR&`, `&HAABBGGRR&` (alpha
/// inverted), or a decimal `BGR` integer (SSA); anything else is `default`.
#[must_use]
pub fn ssa_color(f: &str, default: Color) -> Color {
    // libse's `Substring` calls count characters, so this works on characters too: a
    // multi-byte character just makes the hex parse fail, and libse's caught
    // exception gives `default`.
    let mut s: Vec<char> = f.trim().trim_matches('&').to_lowercase().chars().collect();
    if s.first() == Some(&'h') && s.len() < 7 {
        while s.len() < 7 {
            s.insert(1, '0');
        }
    }
    let take = |from: usize, len: usize| s[from..from + len].iter().collect::<String>();
    // `"#" + bb gg rr` of the six characters at `at`, swapped to `#rrggbb`.
    let swap = |at: usize| format!("#{}{}{}", take(at + 4, 2), take(at + 2, 2), take(at, 2));
    if s.first() == Some(&'h') && s.len() == 7 {
        return color_from_html(&swap(1)).unwrap_or(default);
    }
    if s.first() == Some(&'h') && s.len() == 9 {
        // `int.TryParse(..., NumberStyles.HexNumber, ...)`; ASS stores alpha inverted.
        let alpha = hex_number_try_parse(&take(1, 2)).map_or(255, |a| 255 - i64::from(a));
        return color_from_html(&swap(3))
            .and_then(|c| c.with_alpha(alpha))
            .unwrap_or(default);
    }
    if let Some(number) = int_try_parse(f) {
        // `Color.FromArgb(number)`, then the bytes swapped from BGR to RGB.
        #[allow(clippy::cast_sign_loss)]
        let temp = Color::from_argb(number as u32);
        return Color {
            a: 255,
            r: temp.b,
            g: temp.g,
            b: temp.r,
        };
    }
    default
}
