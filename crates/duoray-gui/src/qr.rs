//! QR codes of a subscription or server link, to move it to a phone.

use qrcodegen::{QrCode, QrCodeEcc};

/// The pixels of one module, and the quiet zone around the code (4 modules,
/// as the standard asks).
const MODULE: usize = 8;
const BORDER: usize = 4;

/// A QR image of `text`: dark modules on white, whatever the theme, so phone
/// cameras read it. `None` if the text is too long for a QR code.
pub fn image(text: &str) -> Option<slint::Image> {
    let code = QrCode::encode_text(text, QrCodeEcc::Medium).ok()?;
    let side = (code.size() as usize + 2 * BORDER) * MODULE;
    let mut buffer = slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(side as u32, side as u32);
    let pixels = buffer.make_mut_slice();
    for y in 0..side {
        for x in 0..side {
            let module = |p: usize| (p / MODULE) as i32 - BORDER as i32;
            let dark = code.get_module(module(x), module(y));
            pixels[y * side + x] = if dark { slint::Rgb8Pixel::new(0, 0, 0) } else { slint::Rgb8Pixel::new(255, 255, 255) };
        }
    }
    Some(slint::Image::from_rgb8(buffer))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_links_and_refuses_what_does_not_fit() {
        let img = image("https://vip.dualizm.space/sub/CzFa8yV6D71fjuhc").unwrap();
        let size = img.size();
        assert_eq!(size.width, size.height);
        assert_eq!(size.width as usize % MODULE, 0);
        assert!(image(&"x".repeat(5000)).is_none(), "beyond a QR code's capacity");
    }
}
