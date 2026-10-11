//! Colour emoji in names. The software renderer draws plain outline glyphs
//! only, so emoji in server and subscription names came out blank. They are
//! drawn here instead, from the system's colour emoji font (Noto Color Emoji,
//! Segoe UI Emoji, Apple Color Emoji) with swash, and shown as images between
//! the text runs.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::OnceLock;

use slint::{Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};
use swash::FontRef;
use swash::scale::image::Content;
use swash::scale::{Render, ScaleContext, Source, StrikeWith};
use swash::shape::ShapeContext;

/// Rendered once at this size and scaled down by Slint.
const PX: f32 = 64.0;

/// A name split into text runs and emoji.
#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    Text(String),
    Emoji(String),
}

/// Emoji-related characters: pictographs, symbols, regional indicators,
/// skin tones, joiners, variation selectors, keycaps and tag sequences.
pub fn is_emoji_char(c: char) -> bool {
    let u = c as u32;
    (0x1F000..=0x1FAFF).contains(&u)
        || (0x2600..=0x27BF).contains(&u)
        || (0x2B00..=0x2BFF).contains(&u)
        || (0xE0020..=0xE007F).contains(&u)
        || matches!(c, '\u{200D}' | '\u{FE0F}' | '\u{20E3}')
}

/// Splits `s` into text and emoji runs; spaces stay with the text.
pub fn split(s: &str) -> Vec<Part> {
    let mut parts: Vec<Part> = vec![];
    for c in s.chars() {
        let emoji = is_emoji_char(c);
        match parts.last_mut() {
            Some(Part::Emoji(run)) if emoji => run.push(c),
            Some(Part::Text(run)) if !emoji => run.push(c),
            _ => parts.push(if emoji { Part::Emoji(c.into()) } else { Part::Text(c.into()) }),
        }
    }
    parts
}

struct EmojiFont {
    data: Vec<u8>,
    index: usize,
}

fn font() -> Option<&'static EmojiFont> {
    static FONT: OnceLock<Option<EmojiFont>> = OnceLock::new();
    FONT.get_or_init(|| {
        let (path, index) = font_path()?;
        let data = std::fs::read(path).ok()?;
        FontRef::from_index(&data, index)?;
        Some(EmojiFont { data, index })
    })
    .as_ref()
}

fn font_path() -> Option<(std::path::PathBuf, usize)> {
    #[cfg(windows)]
    {
        let dir = std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into());
        Some((std::path::Path::new(&dir).join("Fonts").join("seguiemj.ttf"), 0))
    }
    #[cfg(target_os = "macos")]
    {
        Some(("/System/Library/Fonts/Apple Color Emoji.ttc".into(), 0))
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let out = std::process::Command::new("fc-match").args(["-f", "%{file}\n%{index}", "emoji:color"]).output().ok()?;
        let text = String::from_utf8(out.stdout).ok()?;
        let mut lines = text.lines();
        let path = std::path::PathBuf::from(lines.next()?.trim());
        let index = lines.next().and_then(|i| i.trim().parse().ok()).unwrap_or(0);
        path.is_file().then_some((path, index))
    }
}

/// RGBA glyph images of an emoji run (one per emoji after shaping, so flags
/// and ZWJ families come out whole). Empty without a colour emoji font.
pub fn render(run: &str) -> Vec<SharedPixelBuffer<Rgba8Pixel>> {
    let Some(f) = font() else { return vec![] };
    let Some(font) = FontRef::from_index(&f.data, f.index) else { return vec![] };
    let mut glyphs = vec![];
    let mut shaper_cx = ShapeContext::new();
    let mut shaper = shaper_cx.builder(font).size(PX).build();
    shaper.add_str(run);
    shaper.shape_with(|cluster| glyphs.extend(cluster.glyphs.iter().map(|g| g.id).filter(|&id| id != 0)));

    let mut scale_cx = ScaleContext::new();
    let mut scaler = scale_cx.builder(font).size(PX).hint(false).build();
    let sources = [Source::ColorOutline(0), Source::ColorBitmap(StrikeWith::BestFit), Source::Outline];
    let renderer = Render::new(&sources);
    glyphs
        .into_iter()
        .filter_map(|id| {
            let img = renderer.render(&mut scaler, id)?;
            let (w, h) = (img.placement.width, img.placement.height);
            if w == 0 || h == 0 {
                return None;
            }
            let rgba: Vec<u8> = match img.content {
                Content::Color => img.data,
                // A plain outline (no colour version): grey, readable on both themes.
                Content::Mask => img.data.iter().flat_map(|&a| [128, 128, 128, a]).collect(),
                Content::SubpixelMask => return None,
            };
            Some(SharedPixelBuffer::clone_from_slice(&rgba, w, h))
        })
        .collect()
}

/// The images of an emoji run, cached (names repeat on every list rebuild).
/// UI thread only: `slint::Image` stays on it.
pub fn images(run: &str) -> Vec<Image> {
    thread_local! {
        static CACHE: RefCell<HashMap<String, Vec<Image>>> = RefCell::new(HashMap::new());
    }
    CACHE.with(|cache| {
        cache
            .borrow_mut()
            .entry(run.to_string())
            .or_insert_with(|| render(run).into_iter().map(Image::from_rgba8).collect())
            .clone()
    })
}

/// Parts for the UI's RichName, or an empty model when `name` has no emoji
/// (the plain Text is shown then). Emoji the font lacks are left out.
pub fn name_parts(name: &str) -> ModelRc<crate::NamePart> {
    let parts = split(name);
    if !parts.iter().any(|p| matches!(p, Part::Emoji(_))) {
        return ModelRc::default();
    }
    let mut out = vec![];
    for part in parts {
        match part {
            Part::Text(text) => out.push(crate::NamePart { text: text.into(), emoji: Image::default() }),
            Part::Emoji(run) => {
                out.extend(images(&run).into_iter().map(|emoji| crate::NamePart { text: "".into(), emoji }));
            }
        }
    }
    ModelRc::new(VecModel::from(out))
}

/// `name` without a leading flag (that one is shown as the row's flag image).
pub fn without_leading_flag(name: &str) -> &str {
    let name = name.trim();
    let regional = |c: char| ('\u{1F1E6}'..='\u{1F1FF}').contains(&c);
    let mut chars = name.char_indices();
    match (chars.next(), chars.next()) {
        (Some((_, a)), Some((i, b))) if regional(a) && regional(b) => name[i + b.len_utf8()..].trim_start(),
        _ => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_text_and_emoji() {
        assert_eq!(
            split("LTE 🚀⚡ Fast 👨‍👩‍👧"),
            [
                Part::Text("LTE ".into()),
                Part::Emoji("🚀⚡".into()),
                Part::Text(" Fast ".into()),
                Part::Emoji("👨\u{200D}👩\u{200D}👧".into()),
            ]
        );
        assert_eq!(split("Германия"), [Part::Text("Германия".into())]);
        assert_eq!(without_leading_flag(" 🇩🇪 Германия 🚀"), "Германия 🚀");
        assert_eq!(without_leading_flag("LTE 🇩🇪"), "LTE 🇩🇪");
    }

    /// Needs a colour emoji font (skipped without one). DUORAY_EMOJI_PNG=dir
    /// writes the images for a look.
    #[test]
    fn renders_emoji_runs() {
        if font().is_none() {
            return;
        }
        let imgs = render("🚀🇩🇪👨‍👩‍👧🔥");
        assert_eq!(imgs.len(), 4, "rocket, flag, family (one ligature), fire");
        if let Some(dir) = std::env::var_os("DUORAY_EMOJI_PNG") {
            for (i, img) in imgs.iter().enumerate() {
                let file = std::fs::File::create(std::path::Path::new(&dir).join(format!("emoji{i}.png"))).unwrap();
                let mut enc = png::Encoder::new(file, img.width(), img.height());
                enc.set_color(png::ColorType::Rgba);
                enc.set_depth(png::BitDepth::Eight);
                enc.write_header().unwrap().write_image_data(img.as_bytes()).unwrap();
            }
        }
        assert!(imgs.iter().all(|img| img.width() > 8 && img.height() > 8));
        // The rocket is colourful (the family is a grey silhouette in Noto).
        assert!(imgs[0].as_slice().iter().any(|p| p.a > 0 && (p.r != p.g || p.g != p.b)), "in colour");
    }
}

