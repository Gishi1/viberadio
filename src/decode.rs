//! Decoder boundary for stream playback.
//!
//! The engine worker owns the decoder and only needs three things from it:
//! `f32` samples, a channel count and a sample rate. Keeping that behind a trait
//! means a second backend (for example symphonia for AAC-LC) can be added
//! without touching the worker, the sample ring or the audio callback.
//!
//! The current implementation is rodio, which is the proven path for
//! MP3/Vorbis/FLAC/WAV. A future backend can be tried first or used as a
//! fallback; the worker only sees [`StreamDecoder`].

use std::io::{Read, Seek};
use std::time::Duration;

use rodio::{Decoder, Source};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{Decoder as SymphoniaCodec, DecoderOptions};
use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// A thread-safe byte source for the decoder probe.
///
/// `Sync` is required by symphonia's `MediaSource`; `is_seekable` lets a future
/// backend tell a live network window (not seekable) from a file.
pub trait ReadSeekSend: Read + Seek + Send + Sync {
    fn is_seekable(&self) -> bool {
        false
    }

    /// HTTP content type when known, so AAC can skip rodio's probing (which
    /// consumes the ADTS sync bytes before the fallback sees them).
    fn content_type(&self) -> Option<String> {
        None
    }
}

impl ReadSeekSend for std::io::Cursor<Vec<u8>> {
    fn is_seekable(&self) -> bool {
        true
    }
}

impl ReadSeekSend for std::fs::File {
    fn is_seekable(&self) -> bool {
        true
    }
}

// Intentionally no blanket impl: a backend needs to state whether its source is
// seekable (a live window is not, a local file is), and a blanket impl would make
// that impossible to override.

/// Decoded audio: interleaved `f32` samples plus their format.
pub trait StreamDecoder: Iterator<Item = f32> + Send {
    fn channels(&self) -> u16;
    fn sample_rate(&self) -> u32;
}

/// rodio-backed decoder. The concrete iterator type is boxed so callers never
/// depend on rodio's converter types.
struct RodioStreamDecoder {
    samples: Box<dyn Iterator<Item = f32> + Send>,
    channels: u16,
    sample_rate: u32,
}

impl Iterator for RodioStreamDecoder {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        self.samples.next()
    }
}

impl StreamDecoder for RodioStreamDecoder {
    fn channels(&self) -> u16 {
        self.channels
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
}

/// Serves a `ReadSeekSend` to symphonia's probe without changing the trait.
struct ProbeSource(Box<dyn ReadSeekSend>);

impl Read for ProbeSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

impl Seek for ProbeSource {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        self.0.seek(pos)
    }
}

impl MediaSource for ProbeSource {
    fn is_seekable(&self) -> bool {
        self.0.is_seekable()
    }

    fn byte_len(&self) -> Option<u64> {
        None
    }
}

struct SymphoniaStreamDecoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn SymphoniaCodec>,
    track_id: u32,
    channels: u16,
    sample_rate: u32,
    samples: Vec<f32>,
    index: usize,
}

impl Iterator for SymphoniaStreamDecoder {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        loop {
            if self.index < self.samples.len() {
                let sample = self.samples[self.index];
                self.index += 1;
                return Some(sample);
            }
            let packet = match self.format.next_packet() {
                Ok(packet) => packet,
                Err(symphonia::core::errors::Error::IoError(error))
                    if error.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    return None;
                }
                Err(_) => return None,
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            let buffer = match self.decoder.decode(&packet) {
                Ok(buffer) => buffer,
                Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
                Err(symphonia::core::errors::Error::IoError(error))
                    if error.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    return None;
                }
                Err(_) => return None,
            };
            let spec = *buffer.spec();
            self.sample_rate = spec.rate.max(1);
            self.channels = spec.channels.count().max(1) as u16;
            let mut samples = SampleBuffer::<f32>::new(buffer.frames() as u64, spec);
            samples.copy_interleaved_ref(buffer);
            self.samples.clear();
            self.samples.extend_from_slice(samples.samples());
            self.index = 0;
        }
    }
}

impl StreamDecoder for SymphoniaStreamDecoder {
    fn channels(&self) -> u16 {
        self.channels
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
}

/// Read up to `buf.len()` bytes, stopping early at EOF or an error.
fn peek(reader: &mut dyn ReadSeekSend, buf: &mut [u8]) -> usize {
    let mut read = 0;
    while read < buf.len() {
        match reader.read(&mut buf[read..]) {
            Ok(0) => break,
            Ok(n) => read += n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    read
}

fn open_symphonia(mut reader: Box<dyn ReadSeekSend>) -> Result<Box<dyn StreamDecoder>, String> {
    // ADTS live streams are endless; the generic probe can scan forever looking
    // for a container. Detect the 12-bit ADTS sync ourselves and build the reader
    // directly. Reading two bytes and seeking back is safe: this happens before
    // decoding, while the progressive buffer still holds them.
    let mut sync = [0u8; 2];
    let peeked = peek(&mut *reader, &mut sync);
    let adts = peeked == 2 && sync[0] == 0xFF && (sync[1] & 0xF6) == 0xF0;
    if reader
        .seek(std::io::SeekFrom::Current(-(peeked as i64)))
        .is_err()
    {
        return Err("cannot rewind the ADTS probe bytes".to_string());
    }

    let format: Box<dyn FormatReader> = if adts {
        let stream = MediaSourceStream::new(Box::new(ProbeSource(reader)), Default::default());
        Box::new(
            symphonia_codec_aac::AdtsReader::try_new(stream, &FormatOptions::default())
                .map_err(|e| format!("ADTS reader failed: {e}"))?,
        )
    } else {
        let stream = MediaSourceStream::new(Box::new(ProbeSource(reader)), Default::default());
        let mut hint = Hint::new();
        hint.with_extension("aac");
        let probed = symphonia::default::get_probe()
            .format(
                &hint,
                stream,
                &FormatOptions {
                    enable_gapless: true,
                    ..Default::default()
                },
                &MetadataOptions::default(),
            )
            .map_err(|e| format!("symphonia probe failed: {e}"))?;
        probed.format
    };

    let (track_id, codec_params) = {
        let track = format
            .default_track()
            .ok_or_else(|| "symphonia found no default track".to_string())?;
        (track.id, track.codec_params.clone())
    };
    let sample_rate = codec_params.sample_rate.unwrap_or(44_100).max(1);
    let channels = codec_params
        .channels
        .map(|channels| channels.count())
        .unwrap_or(2)
        .max(1) as u16;
    let decoder = symphonia::default::get_codecs()
        .make(&codec_params, &DecoderOptions::default())
        .map_err(|e| format!("symphonia has no decoder for this stream: {e}"))?;
    Ok(Box::new(SymphoniaStreamDecoder {
        format,
        decoder,
        track_id,
        channels,
        sample_rate,
        samples: Vec::new(),
        index: 0,
    }))
}

/// How long each backend may spend identifying a stream. Without this an
/// unrecognised format keeps reading until the read-stall timeout, leaving the
/// stream in `Buffering` for half a minute.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Open a decoder, asking `make_reader` for a fresh byte source and a probe
/// budget.
///
/// This is a factory rather than a single reader so a second backend can retry a
/// probe without losing the bytes: the worker's sliding window keeps the data
/// until the decoder starts, and each attempt gets its own budget. The rodio
/// path is tried first; a symphonia AAC-LC path can be added in the `Err` branch
/// without changing the worker.
///
/// A stream whose sample rate or channel count changes mid-attempt is treated as
/// ended and reconnected by the worker; the decoder does not renegotiate format.
pub fn open_with<F>(mut make_reader: F) -> Result<Box<dyn StreamDecoder>, String>
where
    F: FnMut(Duration) -> Box<dyn ReadSeekSend>,
{
    let mut reader = make_reader(PROBE_TIMEOUT);
    let seekable = reader.is_seekable();
    let content_type = reader.content_type().unwrap_or_default().to_lowercase();
    if content_type.contains("aac") {
        return open_symphonia(make_reader(PROBE_TIMEOUT));
    }
    // Peek at the first bytes before rodio consumes them. Live ADTS has no
    // container probe and the fallback would otherwise start mid-frame.
    let mut sync = [0u8; 2];
    let peeked = peek(&mut *reader, &mut sync);
    if reader
        .seek(std::io::SeekFrom::Current(-(peeked as i64)))
        .is_err()
    {
        return Err("cannot rewind the codec probe bytes".to_string());
    }
    if peeked == 2 && sync[0] == 0xFF && (sync[1] & 0xF6) == 0xF0 {
        return open_symphonia(make_reader(PROBE_TIMEOUT));
    }
    match Decoder::new(reader) {
        Ok(decoder) => {
            let channels = decoder.channels().max(1);
            let sample_rate = decoder.sample_rate().max(1);
            let samples: Box<dyn Iterator<Item = f32> + Send> =
                Box::new(decoder.convert_samples::<f32>());
            Ok(Box::new(RodioStreamDecoder {
                samples,
                channels,
                sample_rate,
            }))
        }
        Err(rodio_error) => {
            // Nothing arrived, so there is nothing for a second backend to
            // probe; retrying would only wait out another stall.
            if peeked == 0 {
                return Err(format!("rodio: {rodio_error} (seekable: {seekable})"));
            }
            open_symphonia(make_reader(PROBE_TIMEOUT)).map_err(|symphonia_error| {
                format!("rodio: {rodio_error} (seekable: {seekable}); {symphonia_error}")
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::SeekFrom;

    /// A reader that returns at most one byte per call, to prove the ADTS peek
    /// does not depend on a full two-byte read.
    struct Trickle<R>(R);

    impl<R: Read> Read for Trickle<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let len = buf.len().min(1);
            self.0.read(&mut buf[..len])
        }
    }

    impl<R: Seek> Seek for Trickle<R> {
        fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
            self.0.seek(from)
        }
    }

    impl ReadSeekSend for Trickle<File> {
        fn is_seekable(&self) -> bool {
            true
        }
    }

    #[test]
    fn adts_peek_tolerates_one_byte_reads() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tone.aac");
        let decoder = open_with(|_budget| Box::new(Trickle(File::open(path).expect("fixture"))))
            .expect("ADTS must be detected across one-byte reads");
        assert_eq!(decoder.channels(), 1);
    }

    #[test]
    fn decodes_ogg_vorbis() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tone.ogg");
        let mut decoder = open_with(|_budget| Box::new(File::open(path).expect("fixture")))
            .expect("OGG Vorbis must decode");
        assert_eq!(decoder.channels(), 1);
        assert_eq!(decoder.sample_rate(), 44_100);
        let samples: Vec<f32> = decoder.by_ref().take(20_000).collect();
        assert!(
            samples.len() > 10_000,
            "OGG decoder produced too few samples: {}",
            samples.len()
        );
        assert!(samples.iter().all(|sample| sample.is_finite()));
    }

    #[test]
    fn decodes_flac() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tone.flac");
        let mut decoder = open_with(|_budget| Box::new(File::open(path).expect("fixture")))
            .expect("FLAC must decode");
        assert_eq!(decoder.channels(), 1);
        assert_eq!(decoder.sample_rate(), 44_100);
        let samples: Vec<f32> = decoder.by_ref().take(20_000).collect();
        assert!(
            samples.len() > 10_000,
            "FLAC decoder produced too few samples: {}",
            samples.len()
        );
        assert!(samples.iter().all(|sample| sample.is_finite()));
    }

    #[test]
    fn symphonia_fallback_decodes_aac_lc() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tone.aac");
        let mut decoder = open_with(|_budget| Box::new(File::open(path).expect("fixture")))
            .expect("AAC-LC must decode through the symphonia fallback when rodio cannot");
        assert_eq!(decoder.channels(), 1);
        assert_eq!(decoder.sample_rate(), 44_100);
        let samples: Vec<f32> = decoder.by_ref().take(20_000).collect();
        assert!(
            samples.len() > 10_000,
            "AAC decoder produced too few samples: {}",
            samples.len()
        );
        assert!(samples.iter().all(|sample| sample.is_finite()));
    }
}
