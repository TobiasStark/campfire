//! Vorschaubilder fuer geteilte Dateien (M4-04): kleine JPEGs, hoechstens 100 KB.

use image::codecs::jpeg::JpegEncoder;
use image::DynamicImage;
use std::path::Path;

/// Laengste Kante der Vorschau in Pixeln.
pub const THUMB_EDGE: u32 = 320;
/// Groesste erlaubte Vorschau in Bytes.
pub const MAX_THUMB_BYTES: usize = 100 * 1024;

/// Verkleinert auf `THUMB_EDGE` und kodiert als JPEG; die Qualitaet sinkt, bis es klein genug ist.
pub fn encode_thumbnail(img: &DynamicImage) -> Option<Vec<u8>> {
    let small = img.thumbnail(THUMB_EDGE, THUMB_EDGE).to_rgb8();
    for quality in [80u8, 65, 50, 35, 20] {
        let mut out = Vec::new();
        if JpegEncoder::new_with_quality(&mut out, quality)
            .encode_image(&small)
            .is_ok()
            && out.len() <= MAX_THUMB_BYTES
        {
            return Some(out);
        }
    }
    None
}

/// Dekodiert ein Bild aus fremden Bytes zu RGBA, mit harten Grenzen: Ein kleines, praepariertes
/// Bild (Dekompressionsbombe) darf weder den Arbeitsspeicher fluten noch eine Textur ueber
/// `max_edge` erzeugen. Groessere Bilder werden auf `fit_edge` verkleinert.
/// `None`, wenn es sich nicht (innerhalb der Grenzen) lesen laesst.
pub fn decode_rgba_limited(
    bytes: &[u8],
    max_edge: u32,
    max_alloc: u64,
    fit_edge: u32,
) -> Option<(u32, u32, Vec<u8>)> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(max_edge);
    limits.max_image_height = Some(max_edge);
    limits.max_alloc = Some(max_alloc);
    reader.limits(limits);
    let mut img = reader.decode().ok()?;
    if img.width() > fit_edge || img.height() > fit_edge {
        img = img.thumbnail(fit_edge, fit_edge);
    }
    let rgba = img.into_rgba8();
    Some((rgba.width(), rgba.height(), rgba.into_raw()))
}

/// Vorschau eines Bildes (PNG, JPEG, GIF, WebP, BMP). `None`, wenn es sich nicht lesen laesst.
pub fn image_thumbnail(path: &Path) -> Option<Vec<u8>> {
    let img = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?;
    encode_thumbnail(&img)
}

/// Vorschau eines Videos aus dem ersten dekodierbaren Bild.
#[cfg(feature = "ffmpeg")]
pub fn video_thumbnail(path: &Path) -> Option<Vec<u8>> {
    extern crate ffmpeg_next as ffmpeg;
    use ffmpeg::{codec, format, format::Pixel, frame, media, software::scaling};

    ffmpeg::init().ok()?;
    let mut input = format::input(path).ok()?;
    let stream = input.streams().best(media::Type::Video)?;
    let index = stream.index();
    let mut dec = codec::context::Context::from_parameters(stream.parameters())
        .ok()?
        .decoder()
        .video()
        .ok()?;
    for (s, packet) in input.packets() {
        if s.index() != index {
            continue;
        }
        if dec.send_packet(&packet).is_err() {
            continue;
        }
        let mut raw = frame::Video::empty();
        if dec.receive_frame(&mut raw).is_err() {
            continue;
        }
        let (w, h) = (raw.width(), raw.height());
        let mut scaler = scaling::Context::get(
            raw.format(),
            w,
            h,
            Pixel::RGB24,
            w,
            h,
            scaling::Flags::BILINEAR,
        )
        .ok()?;
        let mut rgb = frame::Video::empty();
        scaler.run(&raw, &mut rgb).ok()?;
        let stride = rgb.stride(0);
        let mut buf = Vec::with_capacity((w * h * 3) as usize);
        for row in 0..h as usize {
            buf.extend_from_slice(&rgb.data(0)[row * stride..row * stride + w as usize * 3]);
        }
        let img = image::RgbImage::from_raw(w, h, buf)?;
        return encode_thumbnail(&DynamicImage::ImageRgb8(img));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noisy(w: u32, h: u32) -> DynamicImage {
        let mut x = 0x1234_5678u32;
        DynamicImage::ImageRgb8(image::RgbImage::from_fn(w, h, |_, _| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            image::Rgb([x as u8, (x >> 8) as u8, (x >> 16) as u8])
        }))
    }

    #[test]
    fn thumbnails_are_small_keep_aspect_and_stay_under_the_limit() {
        // Rauschen laesst sich schlecht komprimieren: der haerteste Fall.
        let t = encode_thumbnail(&noisy(4000, 1000)).unwrap();
        assert!(t.len() <= MAX_THUMB_BYTES, "{} Bytes", t.len());
        let back = image::load_from_memory(&t).unwrap();
        assert_eq!((back.width(), back.height()), (320, 80));
        let small = image::load_from_memory(&encode_thumbnail(&noisy(50, 40)).unwrap()).unwrap();
        assert!(small.width() <= THUMB_EDGE && small.height() <= THUMB_EDGE);
    }

    #[test]
    fn decompression_bombs_are_refused_and_big_images_shrink() {
        // Einfarbig: als PNG winzig, dekodiert ein Vielfaches (hier 6000 x 6000).
        let bomb = DynamicImage::ImageLuma8(image::GrayImage::new(6000, 6000));
        let mut png = Vec::new();
        bomb.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        assert!(png.len() < 256 * 1024, "{}", png.len());
        assert!(decode_rgba_limited(&png, 16_384, 16 << 20, 4096).is_none());
        assert!(decode_rgba_limited(&png, 4096, 512 << 20, 4096).is_none());
        // Innerhalb der Grenzen: wird auf die Zielgroesse verkleinert.
        let mut jpg = Vec::new();
        noisy(3000, 1500)
            .write_to(
                &mut std::io::Cursor::new(&mut jpg),
                image::ImageFormat::Jpeg,
            )
            .unwrap();
        let (w, h, px) = decode_rgba_limited(&jpg, 4096, 64 << 20, 1000).unwrap();
        assert_eq!((w, h), (1000, 500));
        assert_eq!(px.len(), 1000 * 500 * 4);
        // Zu viel Speicher fuer die Pixel: abgelehnt, auch wenn die Kanten passen.
        assert!(decode_rgba_limited(&jpg, 4096, 1 << 20, 1000).is_none());
        assert!(decode_rgba_limited(b"kein bild", 4096, 64 << 20, 1000).is_none());
    }

    #[test]
    fn png_file_gives_a_thumbnail_and_garbage_does_not() {
        let dir = std::env::temp_dir().join(format!("campfire-thumb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("a.png");
        noisy(640, 480).save(&png).unwrap();
        let t = image_thumbnail(&png).unwrap();
        assert_eq!(image::load_from_memory(&t).unwrap().width(), 320);
        let bad = dir.join("b.png");
        std::fs::write(&bad, b"das ist kein Bild").unwrap();
        assert!(image_thumbnail(&bad).is_none());
        assert!(image_thumbnail(&dir.join("fehlt.png")).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "ffmpeg")]
    #[test]
    fn first_frame_of_a_video_becomes_a_thumbnail() {
        use crate::encode::{probe, Bgra, Encoder};
        use crate::select::{Codec, Resolution, StreamOptions};
        let Some(info) = probe().iter().find(|e| e.codec == Codec::H264) else {
            return;
        };
        let opts = StreamOptions {
            resolution: Resolution::Native,
            ..StreamOptions::default()
        };
        let (w, h) = (640u32, 360u32);
        let mut img = vec![0u8; (w * h * 4) as usize];
        for px in img.chunks_exact_mut(4) {
            px.copy_from_slice(&[40, 180, 220, 255]); // BGRA: Orange
        }
        let mut e = Encoder::new(*info, opts);
        let mut bytes = Vec::new();
        for n in 0..10i64 {
            let frame = Bgra {
                data: &img,
                width: w,
                height: h,
                pitch: (w * 4) as usize,
            };
            for p in e.encode(&frame, n * 33).unwrap() {
                bytes.extend(p.data);
            }
        }
        for p in e.finish() {
            bytes.extend(p.data);
        }
        let path = std::env::temp_dir().join(format!("campfire-thumb-{}.h264", std::process::id()));
        std::fs::write(&path, &bytes).unwrap();
        let t = video_thumbnail(&path).expect("Vorschau");
        let _ = std::fs::remove_file(&path);
        let rgb = image::load_from_memory(&t).unwrap().to_rgb8();
        assert_eq!(rgb.width(), 320);
        let p = rgb.get_pixel(rgb.width() / 2, rgb.height() / 2);
        assert!(p[0] > 180 && p[2] < 110, "keine Orange-Farbe: {p:?}");
    }
}
