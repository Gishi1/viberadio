//! Minimal MPEG-TS demuxer for HLS segments.
//!
//! HLS radio streams often carry ADTS AAC or MP3 inside MPEG-TS packets. The
//! decoder wants the elementary stream, not the container, so this module walks
//! the PAT/PMT to find the audio PID and copies each PES payload into a byte
//! stream the decoder can read.
//!
//! Only what HLS needs is implemented: no PCR handling, no continuity checks,
//! and no video. A segment that does not look like TS is reported as such and
//! the caller passes it through unchanged.

/// MPEG-TS packet layout: a 4-byte timestamp prefix and 192-byte packets is
/// used by some broadcast captures; HLS uses plain 188-byte packets.
const TS_PACKET: usize = 188;
const M2TS_PACKET: usize = 192;
const M2TS_PREFIX: usize = 4;

/// Stream types that carry audio this player can decode.
fn is_audio_stream(stream_type: u8) -> bool {
    matches!(stream_type, 0x03 | 0x04 | 0x0f | 0x11)
}

#[derive(Debug, Default)]
pub struct TsDemuxer {
    pmt_pid: Option<u16>,
    audio_pid: Option<u16>,
    layout: Option<(usize, usize)>,
    pes_remaining: Option<usize>,
}

impl TsDemuxer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Extract the elementary audio stream from one segment.
    ///
    /// Returns `false` when the bytes are not MPEG-TS, in which case the caller
    /// should treat them as a raw audio segment.
    pub fn demux(&mut self, bytes: &[u8], out: &mut Vec<u8>) -> bool {
        let layout = match self.layout.or_else(|| detect_layout(bytes)) {
            Some(layout) => layout,
            None => return false,
        };
        self.layout = Some(layout);

        let (offset, stride) = layout;
        let mut index = offset;
        while index + stride <= bytes.len() {
            self.packet(&bytes[index..index + stride], out);
            index += stride;
        }
        true
    }
}

/// Find the packet offset and stride, tolerating a leading partial packet.
fn detect_layout(bytes: &[u8]) -> Option<(usize, usize)> {
    if bytes.len() >= TS_PACKET && bytes[0] == 0x47 {
        return Some((0, TS_PACKET));
    }
    if bytes.len() >= M2TS_PACKET && bytes[M2TS_PREFIX] == 0x47 {
        return Some((M2TS_PREFIX, M2TS_PACKET));
    }
    // A segment can start mid-packet. Look for two sync bytes one packet apart.
    for offset in 0..TS_PACKET.min(bytes.len()) {
        if bytes[offset] == 0x47 && bytes.get(offset + TS_PACKET) == Some(&0x47) {
            return Some((offset, TS_PACKET));
        }
    }
    None
}

impl TsDemuxer {
    fn packet(&mut self, packet: &[u8], out: &mut Vec<u8>) {
        if packet.len() < 4 || packet[0] != 0x47 {
            return;
        }
        let pid = (u16::from(packet[1] & 0x1f) << 8) | u16::from(packet[2]);
        let payload_start = packet[1] & 0x40 != 0;
        let adaptation = (packet[3] >> 4) & 0x3;
        if adaptation & 0x1 == 0 {
            return;
        }
        let mut start = 4;
        if adaptation & 0x2 != 0 {
            start = 5 + usize::from(packet[4]);
            if start >= packet.len() {
                return;
            }
        }
        let payload = &packet[start..];

        if pid == 0 {
            self.parse_pat(payload);
        } else if Some(pid) == self.pmt_pid {
            self.parse_pmt(payload);
        } else if Some(pid) == self.audio_pid {
            self.parse_pes(payload, payload_start, out);
        }
    }

    fn parse_pat(&mut self, payload: &[u8]) {
        let Some(section) = psi_section(payload) else {
            return;
        };
        if section.first() != Some(&0x00) || section.len() < 12 {
            return;
        }
        let end = section.len().saturating_sub(4);
        let mut index = 8;
        while index + 4 <= end {
            let program = u16::from_be_bytes([section[index], section[index + 1]]);
            let pid = (u16::from(section[index + 2] & 0x1f) << 8) | u16::from(section[index + 3]);
            if program != 0 {
                self.pmt_pid = Some(pid);
                return;
            }
            index += 4;
        }
    }

    fn parse_pmt(&mut self, payload: &[u8]) {
        let Some(section) = psi_section(payload) else {
            return;
        };
        if section.first() != Some(&0x02) || section.len() < 12 {
            return;
        }
        let program_info = (usize::from(section[10] & 0x0f) << 8) | usize::from(section[11]);
        let end = section.len().saturating_sub(4);
        let mut index = 12 + program_info;
        while index + 5 <= end {
            let stream_type = section[index];
            let pid = (u16::from(section[index + 1] & 0x1f) << 8) | u16::from(section[index + 2]);
            let es_info = (usize::from(section[index + 3] & 0x0f) << 8) | usize::from(section[index + 4]);
            if is_audio_stream(stream_type) {
                self.audio_pid = Some(pid);
                return;
            }
            index += 5 + es_info;
        }
    }

    fn parse_pes(&mut self, payload: &[u8], start: bool, out: &mut Vec<u8>) {
        if !start {
            let take = match self.pes_remaining {
                Some(remaining) => remaining.min(payload.len()),
                None => payload.len(),
            };
            out.extend_from_slice(&payload[..take]);
            if let Some(remaining) = &mut self.pes_remaining {
                *remaining -= take;
            }
            return;
        }
        if payload.len() < 9 || payload[0..3] != [0, 0, 1] {
            return;
        }
        let packet_length = usize::from(u16::from_be_bytes([payload[4], payload[5]]));
        let header_length = usize::from(payload[8]);
        let body = 9 + header_length;
        let Some(rest) = payload.get(body..) else {
            return;
        };
        // PES_packet_length counts the flags, the header length byte, the
        // optional header and the payload; a zero length means unbounded.
        let remaining = if packet_length == 0 {
            None
        } else {
            Some(packet_length.saturating_sub(3 + header_length))
        };
        let take = match remaining {
            Some(remaining) => remaining.min(rest.len()),
            None => rest.len(),
        };
        out.extend_from_slice(&rest[..take]);
        self.pes_remaining = remaining.map(|remaining| remaining - take);
    }
}

/// The PSI section after the pointer field, trimmed to its declared length.
fn psi_section(payload: &[u8]) -> Option<&[u8]> {
    let pointer = usize::from(*payload.first()?);
    let rest = payload.get(1 + pointer..)?;
    if rest.len() < 3 {
        return None;
    }
    let section_length = (usize::from(rest[1] & 0x0f) << 8) | usize::from(rest[2]);
    rest.get(..3 + section_length)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(pid: u16, start: bool, payload: &[u8]) -> Vec<u8> {
        let mut packet = vec![0x47];
        packet.push((if start { 0x40 } else { 0x00 }) | ((pid >> 8) as u8 & 0x1f));
        packet.push((pid & 0xff) as u8);
        packet.push(0x10);
        packet.extend_from_slice(payload);
        assert!(packet.len() <= TS_PACKET, "test packet overflows");
        packet.resize(TS_PACKET, 0xff);
        packet
    }

    fn pat() -> Vec<u8> {
        let mut section = vec![0x00, 0xb0, 0x0d, 0x00, 0x01, 0xc1, 0x00, 0x00];
        section.extend_from_slice(&[0x00, 0x01, 0xf0, 0x00]);
        section.extend_from_slice(&[0, 0, 0, 0]);
        let mut payload = vec![0x00];
        payload.extend_from_slice(&section);
        packet(0, true, &payload)
    }

    fn pmt() -> Vec<u8> {
        let mut section = vec![0x02, 0xb0, 0x12, 0x00, 0x01, 0xc1, 0x00, 0x00];
        section.extend_from_slice(&[0xe1, 0xff, 0xf0, 0x00]);
        section.extend_from_slice(&[0x0f, 0xe1, 0x01, 0xf0, 0x00]);
        section.extend_from_slice(&[0, 0, 0, 0]);
        let mut payload = vec![0x00];
        payload.extend_from_slice(&section);
        packet(0x1000, true, &payload)
    }

    fn pes(payload: &[u8]) -> Vec<u8> {
        let mut data = vec![0x00, 0x00, 0x01, 0xc0];
        let length = (3 + payload.len()) as u16;
        data.extend_from_slice(&length.to_be_bytes());
        data.extend_from_slice(&[0x80, 0x00, 0x00]);
        data.extend_from_slice(payload);
        packet(0x0101, true, &data)
    }

    #[test]
    fn demuxes_an_adts_audio_pes() {
        let audio = b"adts-audio-payload";
        let mut segment = Vec::new();
        segment.extend_from_slice(&pat());
        segment.extend_from_slice(&pmt());
        segment.extend_from_slice(&pes(audio));

        let mut demuxer = TsDemuxer::new();
        let mut out = Vec::new();
        assert!(demuxer.demux(&segment, &mut out));
        assert_eq!(out, audio);
    }

    #[test]
    fn ignores_non_ts_bytes() {
        let mut demuxer = TsDemuxer::new();
        let mut out = Vec::new();
        assert!(!demuxer.demux(b"ID3\x04\x00\x00\x00\x00\x00\x00", &mut out));
        assert!(out.is_empty());
    }

    #[test]
    fn keeps_the_audio_pid_across_segments() {
        let audio = b"second-segment";
        let mut first = Vec::new();
        first.extend_from_slice(&pat());
        first.extend_from_slice(&pmt());
        first.extend_from_slice(&pes(b"first"));

        let mut second = Vec::new();
        second.extend_from_slice(&pes(audio));

        let mut demuxer = TsDemuxer::new();
        let mut out = Vec::new();
        assert!(demuxer.demux(&first, &mut out));
        assert!(demuxer.demux(&second, &mut out));
        assert_eq!(out, b"firstsecond-segment");
    }

    #[test]
    fn trims_pes_stuffing() {
        let audio = b"short";
        let mut data = vec![0x00, 0x00, 0x01, 0xc0];
        let length = (3 + audio.len()) as u16;
        data.extend_from_slice(&length.to_be_bytes());
        data.extend_from_slice(&[0x80, 0x00, 0x00]);
        data.extend_from_slice(audio);
        data.extend_from_slice(&[0xff; 32]);
        let mut segment = Vec::new();
        segment.extend_from_slice(&pat());
        segment.extend_from_slice(&pmt());
        segment.extend_from_slice(&packet(0x0101, true, &data));

        let mut demuxer = TsDemuxer::new();
        let mut out = Vec::new();
        assert!(demuxer.demux(&segment, &mut out));
        assert_eq!(out, audio);
    }
}
