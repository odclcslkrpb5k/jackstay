//! Thumbnails from every format decoded, and refusals of what is not one (docs/ui.md,
//! "Thumbnails"). The helper process and its confinement are tested through `jackstayd
//! --thumbnail` (crates/jackstayd/tests/thumbnail.rs).

use std::io::Cursor;

use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use jackstay_proto::control::{MAX_THUMBNAIL_HEIGHT, MAX_THUMBNAIL_WIDTH, Thumbnail};
use jackstay_thumbnail::{Error, fit, thumbnail};
use proptest::prelude::*;

#[track_caller]
fn ok<T, E: std::fmt::Debug>(r: Result<T, E>) -> T {
    match r {
        Ok(v) => v,
        Err(e) => panic!("expected Ok, got Err({e:?})"),
    }
}

const RED: [u8; 4] = [255, 0, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];

/// `width` by `height`, red on the left half and blue on the right.
fn halves(width: u32, height: u32) -> DynamicImage {
    DynamicImage::ImageRgba8(RgbaImage::from_fn(width, height, |x, _| {
        Rgba(if x < width / 2 { RED } else { BLUE })
    }))
}

fn encoded(image: &DynamicImage, format: ImageFormat) -> Vec<u8> {
    let image = if format == ImageFormat::Jpeg {
        DynamicImage::ImageRgb8(image.to_rgb8())
    } else {
        image.clone()
    };
    let mut out = Cursor::new(Vec::new());
    ok(image.write_to(&mut out, format));
    out.into_inner()
}

fn pixel(t: &Thumbnail, x: u16, y: u16) -> [u8; 4] {
    let at = (usize::from(y) * usize::from(t.width()) + usize::from(x)) * 4;
    let p = &t.rgba()[at..at + 4];
    [p[0], p[1], p[2], p[3]]
}

fn near(a: [u8; 4], b: [u8; 4]) -> bool {
    a.iter().zip(b).all(|(&x, y)| x.abs_diff(y) <= 24)
}

#[test]
fn every_format_decoded_scales_down_to_fit() {
    for format in
        [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::WebP, ImageFormat::Gif, ImageFormat::Bmp]
    {
        let t = ok(thumbnail(&encoded(&halves(300, 100), format)));
        assert_eq!((t.width(), t.height()), (128, 42), "{format:?}");
        assert_eq!((t.image_width(), t.image_height()), (300, 100), "{format:?}");
        assert!(near(pixel(&t, 10, 20), RED), "{format:?}: {:?}", pixel(&t, 10, 20));
        assert!(near(pixel(&t, 117, 20), BLUE), "{format:?}: {:?}", pixel(&t, 117, 20));
    }
}

#[test]
fn a_small_image_is_kept_as_it_is_with_its_alpha() {
    let image = DynamicImage::ImageRgba8(RgbaImage::from_fn(3, 2, |x, y| {
        Rgba([u8::try_from(x * 80).unwrap_or(0), u8::try_from(y * 100).unwrap_or(0), 7, 128])
    }));
    let t = ok(thumbnail(&encoded(&image, ImageFormat::Png)));
    assert_eq!((t.width(), t.height(), t.image_width(), t.image_height()), (3, 2, 3, 2));
    assert_eq!(t.rgba(), image.to_rgba8().as_raw().as_slice(), "straight alpha, unchanged");
}

#[test]
fn a_tall_or_wide_image_keeps_its_aspect_and_a_side_of_at_least_one() {
    let t = ok(thumbnail(&encoded(&halves(10, 1000), ImageFormat::Png)));
    assert_eq!((t.width(), t.height()), (1, 64));
    let t = ok(thumbnail(&encoded(&halves(4000, 3), ImageFormat::Png)));
    assert_eq!((t.width(), t.height()), (128, 1));
    let t = ok(thumbnail(&encoded(&halves(64, 64), ImageFormat::Png)));
    assert_eq!((t.width(), t.height()), (64, 64), "square: the height decides");
}

#[test]
fn what_is_not_an_image_is_refused() {
    assert_eq!(thumbnail(b""), Err(Error::NotAnImage));
    assert_eq!(thumbnail(b"hello, world"), Err(Error::NotAnImage));
    // A format that exists but is not decoded here.
    assert_eq!(thumbnail(b"II*\0\x08\0\0\0"), Err(Error::NotAnImage), "TIFF");
    let png = encoded(&halves(300, 100), ImageFormat::Png);
    assert_eq!(thumbnail(&png[..png.len() / 2]), Err(Error::Undecodable), "cut short");
    assert_eq!(thumbnail(b"GIF89a\xff\xff"), Err(Error::Undecodable));
}

/// A BMP header: `width` by `height` at 32 bits, `info_len` bytes of info header, the pixels at
/// `offset`, with `pixels` bytes of them.
fn bmp(
    width: i32,
    height: i32,
    info_len: u32,
    compression: u32,
    offset: u32,
    pixels: usize,
) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(b"BM");
    let file_len = u32::try_from(usize::try_from(offset).unwrap_or(0) + pixels).unwrap_or(u32::MAX);
    b.extend_from_slice(&file_len.to_le_bytes());
    b.extend_from_slice(&[0; 4]);
    b.extend_from_slice(&offset.to_le_bytes());
    b.extend_from_slice(&info_len.to_le_bytes());
    b.extend_from_slice(&width.to_le_bytes());
    b.extend_from_slice(&height.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&32u16.to_le_bytes());
    b.extend_from_slice(&compression.to_le_bytes());
    b.extend_from_slice(&u32::try_from(pixels).unwrap_or(0).to_le_bytes());
    b.extend_from_slice(&[0; 16]);
    b.resize(usize::try_from(offset).unwrap_or(0), 0);
    b.extend(std::iter::repeat_n(0x80, pixels));
    b
}

#[test]
fn a_bomb_is_refused_before_it_is_allocated() {
    // 40000 x 40000 at 4 bytes: 6 GB, from a 54-byte file.
    let bomb = bmp(40_000, 40_000, 40, 0, 54, 0);
    let started = std::time::Instant::now();
    assert_eq!(thumbnail(&bomb), Err(Error::Undecodable));
    assert!(started.elapsed().as_secs() < 2, "refused from its header");
    // Just within the limit and whole: decoded.
    let fine = bmp(200, 100, 40, 0, 54, 200 * 100 * 4);
    assert_eq!(ok(thumbnail(&fine)).image_width(), 200);
}

#[test]
fn synergys_malformed_bitmap_is_refused() {
    // As Synergy 1.14 makes it (dogfood, 2026-10-27): a 108-byte header with BI_BITFIELDS, but the
    // pixels at 54, so where its masks should be are pixels. glycin refuses it too.
    let synergy = bmp(16, 16, 108, 3, 54, 16 * 16 * 4);
    assert_eq!(thumbnail(&synergy), Err(Error::Undecodable));
}

proptest! {
    #[test]
    fn a_fit_is_within_the_limits_and_never_larger(w in 1u32.., h in 1u32..) {
        let (nw, nh) = fit(w, h);
        prop_assert!((1..=MAX_THUMBNAIL_WIDTH).contains(&nw));
        prop_assert!((1..=MAX_THUMBNAIL_HEIGHT).contains(&nh));
        prop_assert!(u32::from(nw) <= w && u32::from(nh) <= h);
        // One side reaches its limit or the image's own size.
        prop_assert!(nw == MAX_THUMBNAIL_WIDTH || nh == MAX_THUMBNAIL_HEIGHT || (u32::from(nw), u32::from(nh)) == (w, h));
    }

    #[test]
    fn any_bytes_after_a_real_header_do_not_panic(
        format in prop::sample::select(vec![ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::WebP, ImageFormat::Gif, ImageFormat::Bmp]),
        flips in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..8),
    ) {
        let mut bytes = encoded(&halves(20, 10), format);
        for (at, value) in flips {
            let i = at.index(bytes.len());
            bytes[i] = value;
        }
        if let Ok(t) = thumbnail(&bytes) {
            prop_assert!(t.width() <= MAX_THUMBNAIL_WIDTH && t.height() <= MAX_THUMBNAIL_HEIGHT);
        }
    }
}
