//! Encoding detection and strict, streaming conversion to and from UTF-8.
//!
//! `file_type` identifies the format and `chardetng` estimates a legacy encoding. Byte-order
//! marks take priority, followed by conservative heuristics for BOM-less UTF-16 and UTF-32.
//! A sampled guess is validated while decoding the complete file.

use super::{
    buffer::{Snapshot, TextBuffer},
    file_io::Progress,
};
use encoding_rs::{DecoderResult, EncoderResult};
use std::io::{self, Write};

/// A text encoding the editor can read and write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    /// UTF-8.
    Utf8,
    /// UTF-16, little-endian.
    Utf16Le,
    /// UTF-16, big-endian.
    Utf16Be,
    /// UTF-32, little-endian.
    Utf32Le,
    /// UTF-32, big-endian.
    Utf32Be,
    /// Each byte maps to the same Unicode scalar value. All 256 byte values
    /// round-trip, including NUL and undefined legacy code-page positions.
    Bytes,
    /// A single- or multi-byte legacy encoding.
    Legacy(&'static encoding_rs::Encoding),
}
impl Encoding {
    /// The name shown to the user.
    pub fn name(self) -> &'static str {
        match self {
            Self::Utf8 => "UTF-8",
            Self::Utf16Le => "UTF-16 LE",
            Self::Utf16Be => "UTF-16 BE",
            Self::Utf32Le => "UTF-32 LE",
            Self::Utf32Be => "UTF-32 BE",
            Self::Bytes => "Bytes / ISO-8859-1",
            Self::Legacy(e) => e.name(),
        }
    }
    /// Every encoding the user can choose.
    pub fn choices() -> Vec<Self> {
        let mut choices = vec![
            Self::Utf8,
            Self::Utf16Le,
            Self::Utf16Be,
            Self::Utf32Le,
            Self::Utf32Be,
            Self::Bytes,
        ];
        for label in [
            "windows-1250",
            "windows-1251",
            "windows-1252",
            "windows-1253",
            "windows-1254",
            "windows-1255",
            "windows-1256",
            "windows-1257",
            "windows-1258",
            "windows-874",
            "shift_jis",
            "euc-jp",
            "iso-2022-jp",
            "gb18030",
            "gbk",
            "big5",
            "euc-kr",
            "koi8-r",
            "koi8-u",
            "ibm866",
            "iso-8859-2",
            "iso-8859-3",
            "iso-8859-4",
            "iso-8859-5",
            "iso-8859-6",
            "iso-8859-7",
            "iso-8859-8",
            "iso-8859-8-i",
            "iso-8859-10",
            "iso-8859-13",
            "iso-8859-14",
            "iso-8859-15",
            "iso-8859-16",
            "macintosh",
            "x-mac-cyrillic",
        ] {
            if let Some(e) = encoding_rs::Encoding::for_label(label.as_bytes()) {
                choices.push(Self::Legacy(e));
            }
        }
        choices
    }
    /// The byte-order mark for this encoding; empty when it has none.
    pub fn bom(self) -> &'static [u8] {
        match self {
            Self::Utf8 => b"\xef\xbb\xbf",
            Self::Utf16Le => b"\xff\xfe",
            Self::Utf16Be => b"\xfe\xff",
            Self::Utf32Le => b"\xff\xfe\0\0",
            Self::Utf32Be => b"\0\0\xfe\xff",
            _ => b"",
        }
    }
    /// Whether converting a document to this encoding adds a byte-order mark.
    pub fn default_bom(self) -> bool {
        matches!(
            self,
            Self::Utf16Le | Self::Utf16Be | Self::Utf32Le | Self::Utf32Be
        )
    }
}

/// Guess the encoding of a file from its first bytes.
pub fn detect(sample: &[u8]) -> Encoding {
    if sample.is_empty() {
        return Encoding::Utf8;
    }
    let kind = file_type::FileType::from_bytes(sample);
    let format = kind
        .media_types()
        .first()
        .copied()
        .unwrap_or("application/octet-stream");
    for encoding in [
        Encoding::Utf32Le,
        Encoding::Utf32Be,
        Encoding::Utf8,
        Encoding::Utf16Le,
        Encoding::Utf16Be,
    ] {
        if sample.starts_with(encoding.bom()) {
            return encoding;
        }
    }
    // BOM-less Unicode: require strong positional NUL evidence, not just NULs.
    let groups = sample.len() / 4;
    if groups >= 4 {
        let zeroes: [usize; 4] = std::array::from_fn(|i| {
            sample
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|g| g[i] == 0)
                .count()
        });
        let high = |i| zeroes[i] * 10 > groups * 8;
        let low = |i| zeroes[i] * 10 < groups * 2;
        if low(0) && high(1) && high(2) && high(3) {
            return Encoding::Utf32Le;
        }
        if high(0) && high(1) && high(2) && low(3) {
            return Encoding::Utf32Be;
        }
        if low(0) && high(1) && low(2) && high(3) {
            return Encoding::Utf16Le;
        }
        if high(0) && low(1) && high(2) && low(3) {
            return Encoding::Utf16Be;
        }
    }
    let binary_signature = format != "application/octet-stream"
        && !format.starts_with("text/")
        && !format.contains("json")
        && !format.contains("xml")
        && !format.contains("javascript")
        && !format.contains("toml")
        && !format.contains("yaml");
    let controls = sample
        .iter()
        .filter(|&&b| b < 32 && !matches!(b, b'\t' | b'\n' | b'\r' | 12 | 27))
        .count();
    if binary_signature || sample.contains(&0) || controls * 100 > sample.len().max(1) {
        return Encoding::Bytes;
    }
    let mut detector = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Allow);
    detector.feed(sample, false);
    let encoding = detector.guess(None, chardetng::Utf8Detection::Allow);
    if encoding == encoding_rs::UTF_8 {
        Encoding::Utf8
    } else {
        Encoding::Legacy(encoding)
    }
}

pub(crate) fn decode(
    source: &Snapshot,
    skip: usize,
    encoding: Encoding,
    out: &mut dyn Write,
    progress: &Progress,
) -> io::Result<()> {
    let length = source.len() - skip;
    let mut destination = [0u8; 64 * 1024];
    match encoding {
        Encoding::Utf32Le | Encoding::Utf32Be => {
            if !length.is_multiple_of(4) {
                return Err(invalid(encoding, length));
            }
            source.read_chunks(skip, |offset, input| {
                progress.advance(offset as u64)?;
                let mut used = 0;
                for raw in input.as_chunks::<4>().0 {
                    let value = if encoding == Encoding::Utf32Le {
                        u32::from_le_bytes(*raw)
                    } else {
                        u32::from_be_bytes(*raw)
                    };
                    used += char::from_u32(value)
                        .ok_or_else(|| invalid(encoding, offset))?
                        .encode_utf8(&mut destination[used..])
                        .len();
                }
                out.write_all(&destination[..used])
            })?;
        }
        Encoding::Bytes => source.read_chunks(skip, |offset, input| {
            progress.advance(offset as u64)?;
            let mut used = 0;
            for &byte in input {
                used += char::from(byte).encode_utf8(&mut destination[used..]).len();
            }
            out.write_all(&destination[..used])
        })?,
        _ => {
            let codec = match encoding {
                Encoding::Utf8 => encoding_rs::UTF_8,
                Encoding::Utf16Le => encoding_rs::UTF_16LE,
                Encoding::Utf16Be => encoding_rs::UTF_16BE,
                Encoding::Legacy(e) => e,
                _ => unreachable!(),
            };
            let mut decoder = codec.new_decoder_without_bom_handling();
            source.read_chunks(skip, |offset, input| {
                progress.advance(offset as u64)?;
                let mut consumed = 0;
                loop {
                    let (result, read, written) = decoder.decode_to_utf8_without_replacement(
                        &input[consumed..],
                        &mut destination,
                        false,
                    );
                    out.write_all(&destination[..written])?;
                    consumed += read;
                    match result {
                        DecoderResult::Malformed(_, _) => {
                            return Err(invalid(encoding, offset + consumed));
                        }
                        DecoderResult::InputEmpty => break,
                        DecoderResult::OutputFull => {}
                    }
                }
                Ok(())
            })?;
            loop {
                let (result, _, written) =
                    decoder.decode_to_utf8_without_replacement(&[], &mut destination, true);
                out.write_all(&destination[..written])?;
                match result {
                    DecoderResult::Malformed(_, _) => return Err(invalid(encoding, length)),
                    DecoderResult::InputEmpty => break,
                    DecoderResult::OutputFull => {}
                }
            }
        }
    }
    progress.advance(length as u64)
}

/// Stream `text` into `out` in `encoding`, failing on any unrepresentable character.
pub fn encode(
    text: &TextBuffer,
    encoding: Encoding,
    out: &mut dyn Write,
    progress: &Progress,
) -> io::Result<()> {
    let mut destination = [0u8; 64 * 1024];
    let mut encoder = if let Encoding::Legacy(e) = encoding {
        Some(e.new_encoder())
    } else {
        None
    };
    text.read_chunks(0..text.len(), |offset, chunk| {
        progress.advance(offset as u64)?;
        if encoding == Encoding::Utf8 {
            out.write_all(chunk.as_bytes())?;
            return Ok(());
        }
        if let Some(encoder) = &mut encoder {
            let mut remaining = chunk;
            loop {
                let (result, read, written) = encoder.encode_from_utf8_without_replacement(
                    remaining,
                    &mut destination,
                    false,
                );
                out.write_all(&destination[..written])?;
                remaining = &remaining[read..];
                match result {
                    EncoderResult::Unmappable(ch) => return Err(unmappable(encoding, ch)),
                    EncoderResult::InputEmpty => break,
                    EncoderResult::OutputFull => {}
                }
            }
        } else {
            let mut used = 0;
            for ch in chunk.chars() {
                if used + 4 > destination.len() {
                    out.write_all(&destination[..used])?;
                    used = 0;
                }
                match encoding {
                    Encoding::Bytes => {
                        destination[used] =
                            u8::try_from(ch as u32).map_err(|_| unmappable(encoding, ch))?;
                        used += 1;
                    }
                    Encoding::Utf16Le | Encoding::Utf16Be => {
                        for &mut unit in ch.encode_utf16(&mut [0; 2]) {
                            let bytes = if encoding == Encoding::Utf16Le {
                                unit.to_le_bytes()
                            } else {
                                unit.to_be_bytes()
                            };
                            destination[used..used + 2].copy_from_slice(&bytes);
                            used += 2;
                        }
                    }
                    Encoding::Utf32Le | Encoding::Utf32Be => {
                        let bytes = if encoding == Encoding::Utf32Le {
                            (ch as u32).to_le_bytes()
                        } else {
                            (ch as u32).to_be_bytes()
                        };
                        destination[used..used + 4].copy_from_slice(&bytes);
                        used += 4;
                    }
                    _ => unreachable!(),
                }
            }
            out.write_all(&destination[..used])?;
        }
        Ok(())
    })?;
    if let Some(mut encoder) = encoder {
        loop {
            let (result, _, written) =
                encoder.encode_from_utf8_without_replacement("", &mut destination, true);
            out.write_all(&destination[..written])?;
            match result {
                EncoderResult::InputEmpty => break,
                EncoderResult::Unmappable(ch) => return Err(unmappable(encoding, ch)),
                EncoderResult::OutputFull => {}
            }
        }
    }
    progress.advance(text.len() as u64)
}
fn invalid(encoding: Encoding, offset: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "Invalid {} near byte {offset}; choose another encoding or Bytes",
            encoding.name()
        ),
    )
}
fn unmappable(encoding: Encoding, ch: char) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "{} cannot represent U+{:04X}; choose a different save encoding",
            encoding.name(),
            ch as u32
        ),
    )
}
