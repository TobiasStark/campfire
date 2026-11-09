//! ffmpeg-Decoder fuer empfangene Streams (M3-06): d3d11va mit Rueckfall auf Software.
//! Eingabe sind die kodierten Frames aus dem Transport (Annex B, Parameter im Keyframe),
//! Ausgabe sind NV12-Bilder.

extern crate ffmpeg_next as ffmpeg;

use crate::frame::YuvFrame;
use crate::select::Codec;
use ffmpeg::{codec, decoder, format::Pixel, frame, Packet};

fn codec_id(c: Codec) -> codec::Id {
    match c {
        Codec::H264 => codec::Id::H264,
        Codec::Hevc => codec::Id::HEVC,
        Codec::Av1 => codec::Id::AV1,
    }
}

/// Welche Codecs sich auf diesem Rechner dekodieren lassen (H.264 immer angenommen).
pub fn supported() -> Vec<Codec> {
    let _ = ffmpeg::init();
    Codec::ALL
        .into_iter()
        .filter(|c| *c == Codec::H264 || decoder::find(codec_id(*c)).is_some())
        .collect()
}

unsafe extern "C" fn get_hw_format(
    _ctx: *mut ffmpeg::ffi::AVCodecContext,
    fmts: *const ffmpeg::ffi::AVPixelFormat,
) -> ffmpeg::ffi::AVPixelFormat {
    use ffmpeg::ffi::AVPixelFormat::*;
    let mut p = fmts;
    // SAFETY: ffmpeg uebergibt eine mit AV_PIX_FMT_NONE terminierte Liste.
    unsafe {
        while *p != AV_PIX_FMT_NONE {
            if *p == AV_PIX_FMT_D3D11 {
                return *p;
            }
            p = p.add(1);
        }
    }
    AV_PIX_FMT_NONE
}

fn open(codec: Codec, hw: bool) -> Result<decoder::Video, String> {
    ffmpeg::init().map_err(|e| e.to_string())?;
    let c = decoder::find(codec_id(codec)).ok_or("Decoder nicht im Build")?;
    let mut ctx = codec::context::Context::new_with_codec(c);
    ctx.set_flags(codec::Flags::LOW_DELAY);
    if hw {
        // SAFETY: Vor dem Oeffnen wird der Hardware-Kontext gesetzt; die Referenz gehoert
        // danach dem Decoder-Kontext.
        unsafe {
            use ffmpeg::ffi::*;
            let mut dev: *mut AVBufferRef = std::ptr::null_mut();
            let r = av_hwdevice_ctx_create(
                &mut dev,
                AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA,
                std::ptr::null(),
                std::ptr::null_mut(),
                0,
            );
            if r < 0 {
                return Err(format!("d3d11va nicht verfuegbar ({r})"));
            }
            let p = ctx.as_mut_ptr();
            (*p).hw_device_ctx = av_buffer_ref(dev);
            (*p).get_format = Some(get_hw_format);
            av_buffer_unref(&mut dev);
        }
    }
    ctx.decoder().video().map_err(|e| e.to_string())
}

pub struct Decoder {
    codec: Codec,
    dec: decoder::Video,
    hw: bool,
    raw: frame::Video,
    sw: frame::Video,
    next_id: u64,
}

// SAFETY: Die ffmpeg-Objekte werden nur von einem Thread zur Zeit benutzt (`&mut self`).
unsafe impl Send for Decoder {}

impl Decoder {
    /// `prefer_hw`: zuerst d3d11va versuchen, bei Fehler Software.
    pub fn new(codec: Codec, prefer_hw: bool) -> Result<Self, String> {
        let (dec, hw) = if prefer_hw {
            match open(codec, true) {
                Ok(d) => (d, true),
                Err(_) => (open(codec, false)?, false),
            }
        } else {
            (open(codec, false)?, false)
        };
        Ok(Self {
            codec,
            dec,
            hw,
            raw: frame::Video::empty(),
            sw: frame::Video::empty(),
            next_id: 0,
        })
    }

    pub fn codec(&self) -> Codec {
        self.codec
    }

    pub fn is_hardware(&self) -> bool {
        self.hw
    }

    /// Dekodiert einen Frame. Leer, wenn der Decoder noch puffert.
    pub fn decode(&mut self, data: &[u8]) -> Result<Vec<YuvFrame>, String> {
        let pkt = Packet::copy(data);
        self.dec.send_packet(&pkt).map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        while self.dec.receive_frame(&mut self.raw).is_ok() {
            let src: &frame::Video = if self.raw.format() == Pixel::D3D11 {
                // SAFETY: Beide Frames sind gueltig; kopiert vom GPU- in den CPU-Frame.
                let r = unsafe {
                    ffmpeg::ffi::av_hwframe_transfer_data(
                        self.sw.as_mut_ptr(),
                        self.raw.as_ptr(),
                        0,
                    )
                };
                if r < 0 {
                    return Err(format!("hwframe_transfer: {r}"));
                }
                &self.sw
            } else {
                &self.raw
            };
            self.next_id += 1;
            out.push(to_nv12(src, self.next_id)?);
        }
        Ok(out)
    }
}

fn to_nv12(src: &frame::Video, id: u64) -> Result<YuvFrame, String> {
    let (w, h) = (src.width(), src.height());
    let (wu, hu) = (w as usize, h as usize);
    let mut y = Vec::with_capacity(wu * hu);
    let mut uv = Vec::with_capacity(wu * hu / 2);
    let ys = src.stride(0);
    for row in 0..hu {
        y.extend_from_slice(&src.data(0)[row * ys..row * ys + wu]);
    }
    let uvs = src.stride(1);
    match src.format() {
        Pixel::NV12 => {
            for row in 0..hu / 2 {
                uv.extend_from_slice(&src.data(1)[row * uvs..row * uvs + wu]);
            }
        }
        Pixel::YUV420P | Pixel::YUVJ420P => {
            let vs = src.stride(2);
            for row in 0..hu / 2 {
                let u = &src.data(1)[row * uvs..row * uvs + wu / 2];
                let v = &src.data(2)[row * vs..row * vs + wu / 2];
                for (a, b) in u.iter().zip(v) {
                    uv.push(*a);
                    uv.push(*b);
                }
            }
        }
        other => return Err(format!("Pixelformat {other:?} wird nicht unterstuetzt")),
    }
    Ok(YuvFrame {
        id,
        ts: 0,
        w,
        h,
        y,
        uv,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::{probe, Bgra, Encoder};
    use crate::select::{Resolution, StreamOptions};

    #[test]
    fn roundtrip_through_every_available_h264_encoder_and_both_decoders() {
        assert!(supported().contains(&Codec::H264));
        let found = probe();
        let opts = StreamOptions {
            resolution: Resolution::Native,
            ..StreamOptions::default()
        };
        let (w, h) = (640u32, 360u32);
        let mut img = vec![0u8; (w * h * 4) as usize];
        for px in img.chunks_exact_mut(4) {
            px.copy_from_slice(&[40, 180, 220, 255]); // BGRA: helles Orange
        }
        for info in found.iter().filter(|e| e.codec == Codec::H264) {
            let mut e = Encoder::new(*info, opts);
            let mut packets = Vec::new();
            for n in 0..30i64 {
                packets.extend(
                    e.encode(
                        &Bgra {
                            data: &img,
                            width: w,
                            height: h,
                            pitch: (w * 4) as usize,
                        },
                        n * 17,
                    )
                    .unwrap(),
                );
            }
            packets.extend(e.finish());
            assert!(packets[0].key, "{}", info.name);
            for hw in [false, true] {
                let mut d = Decoder::new(Codec::H264, hw).unwrap();
                let mut frames = Vec::new();
                for p in &packets {
                    frames.extend(d.decode(&p.data).unwrap());
                }
                assert!(
                    !frames.is_empty(),
                    "{} hw={hw}: nichts dekodiert",
                    info.name
                );
                let f = &frames[frames.len() - 1];
                assert_eq!((f.w, f.h), (w, h), "{} hw={hw}", info.name);
                assert_eq!(f.y.len(), (w * h) as usize);
                assert_eq!(f.uv.len(), (w * h / 2) as usize);
                // Farbe ueberlebt grob: orange hat hohes V (Rot), niedriges U (Blau).
                let mid = (f.uv.len() / 2) & !1;
                let (u, v) = (f.uv[mid], f.uv[mid + 1]);
                assert!(v > 150 && u < 110, "{} hw={hw}: U={u} V={v}", info.name);
                println!(
                    "{} -> {}: {} Bilder",
                    info.name,
                    if d.is_hardware() {
                        "d3d11va"
                    } else {
                        "Software"
                    },
                    frames.len()
                );
            }
        }
    }

    #[test]
    fn garbage_does_not_crash() {
        let mut d = Decoder::new(Codec::H264, false).unwrap();
        let _ = d.decode(&[0, 0, 0, 1, 9, 9, 9, 9, 9]);
        let _ = d.decode(&[1, 2, 3]);
    }
}
