//! The Look: one optional JSON object that says how a clip is dressed.
//!
//! It rides along with the job options (`--look <json>` or `--look
//! @file.json`). Every section and every field is optional and absent
//! means "what the engine does today". Parsing is lenient on purpose and
//! never fails a job: unknown fields are ignored, numbers are clamped into
//! their range, a bad enum value or a malformed colour counts as absent,
//! and JSON that does not parse at all is no look (one warning line).
//!
//! v1 contract (`captions`, `headline`, `bar` and `logo` are applied; the
//! other sections are parsed so later stages can read them):
//!
//! ```text
//! look: { v: 1,
//!   captions: { show, x, y, size, font, case, color, active, accent,
//!               outline, outline_w, shadow, box, box_opacity, max_words, anim },
//!   headline: { x, y, size, ink, card, accent, anim, seconds },
//!   bar:      { pos, height },
//!   logo:     { x, y, size, opacity },
//!   camera:   { feel, zoom, punch },
//!   effects:  { vignette, grade, fill_dim },
//!   layout:   { split } }
//! ```
//!
//! `x`/`y` are the centre of an element as a fraction of the output frame
//! (0..1, right and down). Sizes multiply today's size. Colours are
//! `#RRGGBB` strings.

use std::path::Path;

use serde_json::Value;

use crate::captions::ass::Anim;

/// What this engine can do with a look, announced to the app in the `hello`
/// snapshot (`caps`). Later chunks append to it.
pub const CAPS: &[&str] = &[
    "look",
    "look.captions",
    "look.headline",
    "look.bar",
    "look.logo",
];

/// The fonts libass can reach (the provisioned ones).
pub const FONTS: [&str; 4] = ["Anton", "Archivo Black", "Inter Medium", "JetBrains Mono"];

/// An sRGB colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    /// `#RRGGBB` (the `#` is optional); anything else is `None`.
    pub fn parse(s: &str) -> Option<Rgb> {
        let h = s.trim().trim_start_matches('#');
        if h.len() != 6 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let p = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
        Some(Rgb(p(0)?, p(2)?, p(4)?))
    }

    /// ASS colour `&HAABBGGRR` (alpha 00 = opaque, FF = clear).
    pub fn ass(self, alpha: u8) -> String {
        format!("&H{alpha:02X}{:02X}{:02X}{:02X}", self.2, self.1, self.0)
    }
}

/// Caption letter case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Case {
    /// UPPERCASE.
    Upper,
    /// The words as transcribed.
    AsIs,
}

/// What sits behind a caption line, or a headline's card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoxLook {
    /// No box (removes the one a style has).
    None,
    /// An opaque box in this colour.
    Color(Rgb),
}

/// Caption section. `None` fields keep the style's own value.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CaptionsLook {
    /// `false` = no caption lines at all (the headline is unaffected).
    pub show: Option<bool>,
    /// Centre of the caption block (fraction of the frame). When either is
    /// set the block is anchored there, middle-centre; the other is 0.5.
    pub x: Option<f64>,
    pub y: Option<f64>,
    /// Type size multiplier, 0.5..2.
    pub size: Option<f64>,
    /// One of [`FONTS`].
    pub font: Option<&'static str>,
    pub case: Option<Case>,
    /// Text colour (not-yet-spoken; also the spoken colour unless `active`
    /// is set).
    pub color: Option<Rgb>,
    /// Colour of the word being spoken / already spoken.
    pub active: Option<Rgb>,
    /// Keyword colour.
    pub accent: Option<Rgb>,
    /// Text outline colour (ignored while a box is drawn: the box takes
    /// the outline slot).
    pub outline: Option<Rgb>,
    /// Outline width in px at a 1080-wide frame, 0..8 (box padding when a
    /// box is drawn).
    pub outline_w: Option<f64>,
    /// Shadow depth in px at a 1080-wide frame, 0..6.
    pub shadow: Option<f64>,
    /// Box behind the line: a colour, or `None` to remove the style's box.
    pub box_: Option<BoxLook>,
    /// Box opacity 0..1 (1 when a box colour is set without it).
    pub box_opacity: Option<f64>,
    /// Words per line, 1..8.
    pub max_words: Option<usize>,
    pub anim: Option<Anim>,
}

/// Headline motion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadlineAnim {
    Pop,
    Fade,
    None,
}

/// Headline section. Only matters while a headline is on.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HeadlineLook {
    /// Centre of the card (fraction of the frame). When either is set the
    /// card is anchored there; the other is 0.5 (x) or where the card sits
    /// today (y).
    pub x: Option<f64>,
    pub y: Option<f64>,
    /// Type and card padding multiplier, 0.5..2.
    pub size: Option<f64>,
    pub ink: Option<Rgb>,
    /// Card colour, or `None` for no card (the text gets an outline).
    pub card: Option<BoxLook>,
    pub accent: Option<Rgb>,
    pub anim: Option<HeadlineAnim>,
    /// Seconds on screen, >= 0 (0 = the whole clip).
    pub seconds: Option<f64>,
}

/// Where the progress bar runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarPos {
    Top,
    Bottom,
}

/// Progress bar section. Only matters while the progress bar is on.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BarLook {
    pub pos: Option<BarPos>,
    /// Thickness multiplier, 0.5..3.
    pub height: Option<f64>,
}

/// Logo section. Only matters while a logo file is set.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LogoLook {
    /// Centre of the logo box (fraction of the frame). When either is set
    /// it wins over the corner; the other is 0.5.
    pub x: Option<f64>,
    pub y: Option<f64>,
    /// 0.4..2.5.
    pub size: Option<f64>,
    /// 0..1.
    pub opacity: Option<f64>,
}

/// How the virtual camera moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraFeel {
    Locked,
    Steady,
    Smooth,
    Lively,
}

/// Camera section (parsed, not applied yet).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CameraLook {
    pub feel: Option<CameraFeel>,
    /// 0.8..1.4.
    pub zoom: Option<f64>,
    /// Emphasis punch-in strength, 1.0..1.4.
    pub punch: Option<f64>,
}

/// Colour grade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grade {
    None,
    Warm,
    Cool,
    Mono,
    Punchy,
}

/// Effects section (parsed, not applied yet).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EffectsLook {
    /// 0..1.
    pub vignette: Option<f64>,
    pub grade: Option<Grade>,
    /// Darkening of the blurred fill behind letterboxed video, 0..1.
    pub fill_dim: Option<f64>,
}

/// Layout section (parsed, not applied yet).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LayoutLook {
    /// Where the split-screen seam sits (fraction of the height), 0.3..0.7.
    pub split: Option<f64>,
}

/// A whole look. A section is `Some` when the key was an object.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Look {
    /// Contract version the sender wrote (informational).
    pub v: Option<u32>,
    pub captions: Option<CaptionsLook>,
    pub headline: Option<HeadlineLook>,
    pub bar: Option<BarLook>,
    pub logo: Option<LogoLook>,
    pub camera: Option<CameraLook>,
    pub effects: Option<EffectsLook>,
    pub layout: Option<LayoutLook>,
}

impl Look {
    /// No effective field anywhere: the job renders as it always did.
    pub fn is_empty(&self) -> bool {
        fn blank<T: Default + PartialEq>(s: &Option<T>) -> bool {
            s.as_ref().is_none_or(|s| *s == T::default())
        }
        blank(&self.captions)
            && blank(&self.headline)
            && blank(&self.bar)
            && blank(&self.logo)
            && blank(&self.camera)
            && blank(&self.effects)
            && blank(&self.layout)
    }

    /// Read a look out of a JSON value, leniently (never fails).
    pub fn from_value(v: &Value) -> Look {
        let Some(o) = v.as_object() else {
            tracing::warn!("look: not a JSON object, ignored");
            return Look::default();
        };
        let sect = |k: &str| o.get(k).and_then(Value::as_object);
        Look {
            v: o.get("v")
                .and_then(Value::as_f64)
                .map(|n| n.clamp(0.0, 1000.0) as u32),
            captions: sect("captions").map(captions),
            headline: sect("headline").map(headline),
            bar: sect("bar").map(bar),
            logo: sect("logo").map(logo),
            camera: sect("camera").map(camera),
            effects: sect("effects").map(effects),
            layout: sect("layout").map(layout),
        }
    }

    /// Parse JSON text. Malformed JSON is no look, with one warning line.
    pub fn parse(text: &str) -> Look {
        match serde_json::from_str::<Value>(text) {
            Ok(v) => Look::from_value(&v),
            Err(e) => {
                tracing::warn!("look: malformed JSON, ignored ({e})");
                Look::default()
            }
        }
    }

    /// A `--look` argument: JSON text, or `@path` to a JSON file. An
    /// unreadable file is no look, with one warning line.
    pub fn from_arg(arg: &str) -> Look {
        let a = arg.trim();
        match a.strip_prefix('@') {
            Some(p) => match std::fs::read_to_string(Path::new(p.trim())) {
                // Editors love a BOM on Windows.
                Ok(t) => Look::parse(t.trim_start_matches('\u{feff}')),
                Err(e) => {
                    tracing::warn!("look: cannot read {}, ignored ({e})", p.trim());
                    Look::default()
                }
            },
            None => Look::parse(a),
        }
    }
}

type Obj = serde_json::Map<String, Value>;

/// A number clamped into `lo..=hi`.
fn num(o: &Obj, k: &str, lo: f64, hi: f64) -> Option<f64> {
    o.get(k)
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite())
        .map(|n| n.clamp(lo, hi))
}

fn color(o: &Obj, k: &str) -> Option<Rgb> {
    o.get(k).and_then(Value::as_str).and_then(Rgb::parse)
}

fn word(o: &Obj, k: &str) -> Option<String> {
    o.get(k)
        .and_then(Value::as_str)
        .map(|s| s.trim().to_ascii_lowercase())
}

fn captions(o: &Obj) -> CaptionsLook {
    CaptionsLook {
        show: o.get("show").and_then(Value::as_bool),
        x: num(o, "x", 0.0, 1.0),
        y: num(o, "y", 0.0, 1.0),
        size: num(o, "size", 0.5, 2.0),
        font: o.get("font").and_then(Value::as_str).and_then(|f| {
            FONTS
                .iter()
                .find(|n| n.eq_ignore_ascii_case(f.trim()))
                .copied()
        }),
        case: match word(o, "case").as_deref() {
            Some("upper") => Some(Case::Upper),
            Some("asis") => Some(Case::AsIs),
            _ => None,
        },
        color: color(o, "color"),
        active: color(o, "active"),
        accent: color(o, "accent"),
        outline: color(o, "outline"),
        outline_w: num(o, "outline_w", 0.0, 8.0),
        shadow: num(o, "shadow", 0.0, 6.0),
        box_: match o.get("box") {
            Some(Value::Null) => Some(BoxLook::None),
            Some(Value::String(s)) if s.trim().eq_ignore_ascii_case("none") => Some(BoxLook::None),
            Some(Value::String(s)) => Rgb::parse(s).map(BoxLook::Color),
            _ => None,
        },
        box_opacity: num(o, "box_opacity", 0.0, 1.0),
        max_words: num(o, "max_words", 1.0, 8.0).map(|n| n.round() as usize),
        anim: word(o, "anim").and_then(|a| Anim::from_name(&a)),
    }
}

fn headline(o: &Obj) -> HeadlineLook {
    HeadlineLook {
        x: num(o, "x", 0.0, 1.0),
        y: num(o, "y", 0.0, 1.0),
        size: num(o, "size", 0.5, 2.0),
        ink: color(o, "ink"),
        card: match o.get("card") {
            Some(Value::Null) => Some(BoxLook::None),
            Some(Value::String(s)) if s.trim().eq_ignore_ascii_case("none") => Some(BoxLook::None),
            Some(Value::String(s)) => Rgb::parse(s).map(BoxLook::Color),
            _ => None,
        },
        accent: color(o, "accent"),
        anim: match word(o, "anim").as_deref() {
            Some("pop") => Some(HeadlineAnim::Pop),
            Some("fade") => Some(HeadlineAnim::Fade),
            Some("none") => Some(HeadlineAnim::None),
            _ => None,
        },
        seconds: num(o, "seconds", 0.0, 3600.0),
    }
}

fn bar(o: &Obj) -> BarLook {
    BarLook {
        pos: match word(o, "pos").as_deref() {
            Some("top") => Some(BarPos::Top),
            Some("bottom") => Some(BarPos::Bottom),
            _ => None,
        },
        height: num(o, "height", 0.5, 3.0),
    }
}

fn logo(o: &Obj) -> LogoLook {
    LogoLook {
        x: num(o, "x", 0.0, 1.0),
        y: num(o, "y", 0.0, 1.0),
        size: num(o, "size", 0.4, 2.5),
        opacity: num(o, "opacity", 0.0, 1.0),
    }
}

fn camera(o: &Obj) -> CameraLook {
    CameraLook {
        feel: match word(o, "feel").as_deref() {
            Some("locked") => Some(CameraFeel::Locked),
            Some("steady") => Some(CameraFeel::Steady),
            Some("smooth") => Some(CameraFeel::Smooth),
            Some("lively") => Some(CameraFeel::Lively),
            _ => None,
        },
        zoom: num(o, "zoom", 0.8, 1.4),
        punch: num(o, "punch", 1.0, 1.4),
    }
}

fn effects(o: &Obj) -> EffectsLook {
    EffectsLook {
        vignette: num(o, "vignette", 0.0, 1.0),
        grade: match word(o, "grade").as_deref() {
            Some("none") => Some(Grade::None),
            Some("warm") => Some(Grade::Warm),
            Some("cool") => Some(Grade::Cool),
            Some("mono") => Some(Grade::Mono),
            Some("punchy") => Some(Grade::Punchy),
            _ => None,
        },
        fill_dim: num(o, "fill_dim", 0.0, 1.0),
    }
}

fn layout(o: &Obj) -> LayoutLook {
    LayoutLook {
        split: num(o, "split", 0.3, 0.7),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_convert_to_ass_order() {
        assert_eq!(Rgb::parse("#FF3B30"), Some(Rgb(255, 59, 48)));
        assert_eq!(Rgb(255, 59, 48).ass(0), "&H00303BFF");
        assert_eq!(Rgb(0x11, 0x22, 0x33).ass(0x80), "&H80332211");
        assert_eq!(Rgb::parse("00e5ff"), Some(Rgb(0, 229, 255)));
        assert_eq!(Rgb::parse("#FFF"), None);
        assert_eq!(Rgb::parse("#GG0000"), None);
        assert_eq!(Rgb::parse(""), None);
    }

    #[test]
    fn empty_and_junk_looks_are_empty() {
        assert!(Look::parse("{}").is_empty());
        assert!(Look::parse(r#"{"v":1}"#).is_empty());
        assert!(Look::parse(r#"{"captions":{}}"#).is_empty());
        assert!(Look::parse(r#"{"captions":{"font":"Comic Sans","size":"big"}}"#).is_empty());
        assert!(Look::parse("{nope").is_empty());
        assert!(Look::parse("[1,2]").is_empty());
        assert!(Look::parse("").is_empty());
        assert!(!Look::parse(r#"{"captions":{"x":0.2}}"#).is_empty());
        assert!(!Look::parse(r#"{"bar":{"pos":"top"}}"#).is_empty());
    }

    #[test]
    fn captions_fields_are_clamped_and_validated() {
        let l = Look::parse(
            r##"{"v":1,"unknown":7,"captions":{"show":false,"x":-3,"y":9,"size":9,
                "font":"anton","case":"Upper","color":"#ffffff","active":"red",
                "accent":"#00FF00","outline":"#101010","outline_w":99,"shadow":-1,
                "box":"#000000","box_opacity":2,"max_words":20,"anim":"Bounce","extra":1}}"##,
        );
        let c = l.captions.unwrap();
        assert_eq!(l.v, Some(1));
        assert_eq!(c.show, Some(false));
        assert_eq!((c.x, c.y, c.size), (Some(0.0), Some(1.0), Some(2.0)));
        assert_eq!(c.font, Some("Anton"));
        assert_eq!(c.case, Some(Case::Upper));
        assert_eq!(c.color, Some(Rgb(255, 255, 255)));
        assert_eq!(c.active, None); // malformed colour = absent
        assert_eq!(c.accent, Some(Rgb(0, 255, 0)));
        assert_eq!((c.outline_w, c.shadow), (Some(8.0), Some(0.0)));
        assert_eq!(c.box_, Some(BoxLook::Color(Rgb(0, 0, 0))));
        assert_eq!((c.box_opacity, c.max_words), (Some(1.0), Some(8)));
        assert_eq!(c.anim, Some(Anim::Bounce));
    }

    #[test]
    fn bad_enums_fall_back_to_absent() {
        let c = Look::parse(
            r#"{"captions":{"case":"title","anim":"spin","font":"Arial","box":"teal"}}"#,
        )
        .captions
        .unwrap();
        assert_eq!(c, CaptionsLook::default());
        // box: null and "none" remove the box.
        for j in [
            r#"{"captions":{"box":null}}"#,
            r#"{"captions":{"box":"None"}}"#,
        ] {
            assert_eq!(Look::parse(j).captions.unwrap().box_, Some(BoxLook::None));
        }
    }

    #[test]
    fn other_sections_parse_with_their_ranges() {
        let l = Look::parse(
            r##"{"headline":{"x":0.5,"size":0.1,"ink":"#111111","anim":"fade","seconds":-4},
                "bar":{"pos":"top","height":9},"logo":{"size":0.1,"opacity":3},
                "camera":{"feel":"lively","zoom":5,"punch":0.2},
                "effects":{"vignette":2,"grade":"mono","fill_dim":0.4},"layout":{"split":0.9}}"##,
        );
        let h = l.headline.unwrap();
        assert_eq!(
            (h.size, h.seconds, h.anim),
            (Some(0.5), Some(0.0), Some(HeadlineAnim::Fade))
        );
        assert_eq!(h.card, None);
        let b = l.bar.unwrap();
        assert_eq!((b.pos, b.height), (Some(BarPos::Top), Some(3.0)));
        let g = l.logo.unwrap();
        assert_eq!((g.size, g.opacity), (Some(0.4), Some(1.0)));
        let c = l.camera.unwrap();
        assert_eq!(
            (c.feel, c.zoom, c.punch),
            (Some(CameraFeel::Lively), Some(1.4), Some(1.0))
        );
        let e = l.effects.unwrap();
        assert_eq!(
            (e.vignette, e.grade, e.fill_dim),
            (Some(1.0), Some(Grade::Mono), Some(0.4))
        );
        assert_eq!(l.layout.unwrap().split, Some(0.7));
    }

    #[test]
    fn headline_card_is_a_colour_or_none() {
        let card = |j: &str| Look::parse(j).headline.unwrap().card;
        assert_eq!(
            card(r##"{"headline":{"card":"#111111"}}"##),
            Some(BoxLook::Color(Rgb(0x11, 0x11, 0x11)))
        );
        assert_eq!(card(r#"{"headline":{"card":null}}"#), Some(BoxLook::None));
        assert_eq!(card(r#"{"headline":{"card":"None"}}"#), Some(BoxLook::None));
        assert_eq!(card(r#"{"headline":{"card":"teal"}}"#), None);
        assert_eq!(card(r#"{"headline":{"card":7}}"#), None);
        assert!(!Look::parse(r#"{"headline":{"card":null}}"#).is_empty());
    }

    #[test]
    fn caps_announce_the_look_and_its_sections() {
        assert!(CAPS.contains(&"look"));
        for s in ["captions", "headline", "bar", "logo"] {
            assert!(CAPS.contains(&format!("look.{s}").as_str()), "{s}");
        }
    }

    #[test]
    fn look_arg_reads_json_or_a_file_the_same() {
        let json = r##"{"captions":{"x":0.3,"y":0.8,"color":"#FFFFFF","anim":"slide"}}"##;
        let path = std::env::temp_dir().join(format!("digiclip-look-{}.json", std::process::id()));
        std::fs::write(&path, format!("\u{feff}{json}")).unwrap();
        let from_file = Look::from_arg(&format!("@{}", path.display()));
        let _ = std::fs::remove_file(&path);
        assert_eq!(from_file, Look::from_arg(json));
        assert!(!from_file.is_empty());
        // A missing file is no look, not an error.
        assert!(Look::from_arg("@/no/such/look.json").is_empty());
    }
}
