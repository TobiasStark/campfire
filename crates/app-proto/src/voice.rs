//! Voice-Paket-Header: `[flags u8][stream_id u8][seq u32][ts u32][opus...]`.
//! Feste Big-Endian-Struktur, bewusst nicht postcard (Datagramm-Hotpath).

pub const HEADER_LEN: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoiceHeader {
    pub flags: u8,
    pub stream_id: u8,
    pub seq: u32,
    pub ts: u32,
}

impl VoiceHeader {
    pub fn write(&self, payload: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(HEADER_LEN + payload.len());
        v.push(self.flags);
        v.push(self.stream_id);
        v.extend_from_slice(&self.seq.to_be_bytes());
        v.extend_from_slice(&self.ts.to_be_bytes());
        v.extend_from_slice(payload);
        v
    }

    /// Liefert Header und Nutzdaten, oder `None` bei zu kurzem Paket.
    pub fn parse(data: &[u8]) -> Option<(Self, &[u8])> {
        if data.len() < HEADER_LEN {
            return None;
        }
        Some((
            Self {
                flags: data[0],
                stream_id: data[1],
                seq: u32::from_be_bytes(data[2..6].try_into().ok()?),
                ts: u32::from_be_bytes(data[6..10].try_into().ok()?),
            },
            &data[HEADER_LEN..],
        ))
    }
}
