//! Chapters of MP4 / M4B files, which `symphonia` does not read: the Nero `chpl` list in
//! `moov/udta` and the `QuickTime` chapter track (a text track referenced by `tref/chap`).
#![expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "atom types are byte-string literals"
)]

use std::io::{Read, Seek, SeekFrom};

use anyhow::{Context, bail};

use super::Chapter;

/// `moov` larger than this is not a real audiobook header.
const MAX_MOOV: u64 = 64 << 20;

/// Chapters of the file; empty when it has none.
pub(crate) fn chapters<R: Read + Seek>(file: &mut R) -> anyhow::Result<Vec<Chapter>> {
    let Some(moov) = read_moov(file)? else {
        return Ok(Vec::new());
    };
    let quicktime = quicktime_chapters(file, &moov)?;
    if !quicktime.is_empty() {
        return Ok(quicktime);
    }
    Ok(child(&moov, b"udta")
        .and_then(|udta| child(udta, b"chpl"))
        .map(nero_chapters)
        .transpose()?
        .unwrap_or_default())
}

fn read_moov<R: Read + Seek>(file: &mut R) -> anyhow::Result<Option<Vec<u8>>> {
    let len = file.seek(SeekFrom::End(0))?;
    let mut pos = 0;
    while pos + 8 <= len {
        file.seek(SeekFrom::Start(pos))?;
        let mut header = [0; 8];
        file.read_exact(&mut header)?;
        let mut size = u64::from(u32::from_be_bytes(header[..4].try_into()?));
        let mut header_len = 8;
        if size == 1 {
            let mut large = [0; 8];
            file.read_exact(&mut large)?;
            size = u64::from_be_bytes(large);
            header_len = 16;
        } else if size == 0 {
            size = len - pos;
        }
        if size < header_len {
            bail!("corrupt atom at {pos}");
        }
        if &header[4..] == b"moov" {
            let body = size - header_len;
            anyhow::ensure!(body <= MAX_MOOV, "moov is too large");
            let mut moov = vec![0; usize::try_from(body)?];
            file.read_exact(&mut moov)?;
            return Ok(Some(moov));
        }
        pos += size;
    }
    Ok(None)
}

/// Child atoms of a container body: (type, body).
fn atoms(data: &[u8]) -> impl Iterator<Item = (&[u8], &[u8])> {
    let mut rest = data;
    std::iter::from_fn(move || {
        if rest.len() < 8 {
            return None;
        }
        let size = u32::from_be_bytes(rest[..4].try_into().ok()?) as usize;
        let (size, header) = match size {
            0 => (rest.len(), 8),
            1 => {
                let large = u64::from_be_bytes(rest.get(8..16)?.try_into().ok()?);
                (usize::try_from(large).ok()?, 16)
            }
            n => (n, 8),
        };
        if size < header || size > rest.len() {
            return None;
        }
        let kind = &rest[4..8];
        let body = &rest[header..size];
        rest = &rest[size..];
        Some((kind, body))
    })
}

fn child<'a>(data: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    atoms(data).find(|(k, _)| k == kind).map(|(_, body)| body)
}

fn u32_at(data: &[u8], at: usize) -> anyhow::Result<u32> {
    Ok(u32::from_be_bytes(
        data.get(at..at + 4).context("truncated atom")?.try_into()?,
    ))
}

fn u64_at(data: &[u8], at: usize) -> anyhow::Result<u64> {
    Ok(u64::from_be_bytes(
        data.get(at..at + 8).context("truncated atom")?.try_into()?,
    ))
}

/// Nero chapters: start times in 100 ns units, titles; the end is the next start.
fn nero_chapters(chpl: &[u8]) -> anyhow::Result<Vec<Chapter>> {
    let version = *chpl.first().context("empty chpl")?;
    let mut at = 4 + if version > 0 { 4 } else { 0 };
    let count = *chpl.get(at).context("truncated chpl")?;
    at += 1;
    let mut starts = Vec::with_capacity(count.into());
    for _ in 0..count {
        let start = u64_at(chpl, at)? / 10_000;
        let len = usize::from(*chpl.get(at + 8).context("truncated chpl")?);
        let title = chpl.get(at + 9..at + 9 + len).context("truncated chpl")?;
        starts.push((start, String::from_utf8_lossy(title).trim().to_owned()));
        at += 9 + len;
    }
    Ok(close(starts))
}

/// Chapters from (start ms, title): each ends where the next starts; the last is open.
fn close(starts: Vec<(u64, String)>) -> Vec<Chapter> {
    let ends: Vec<Option<u64>> = starts
        .iter()
        .skip(1)
        .map(|(start, _)| Some(*start))
        .chain([None])
        .collect();
    starts
        .into_iter()
        .zip(ends)
        .map(|((start_ms, title), end_ms)| Chapter {
            title,
            start_ms,
            end_ms,
        })
        .collect()
}

/// A track's sample table: what the chapter track needs.
struct TextTrack<'a> {
    timescale: u32,
    stbl: &'a [u8],
}

fn quicktime_chapters<R: Read + Seek>(file: &mut R, moov: &[u8]) -> anyhow::Result<Vec<Chapter>> {
    let traks: Vec<&[u8]> = atoms(moov)
        .filter(|(k, _)| *k == b"trak")
        .map(|(_, body)| body)
        .collect();
    // The track the audio track points at with tref/chap.
    let chapter_ids: Vec<u32> = traks
        .iter()
        .filter_map(|trak| child(trak, b"tref").and_then(|tref| child(tref, b"chap")))
        .flat_map(|chap| {
            chap.as_chunks::<4>()
                .0
                .iter()
                .map(|id| u32::from_be_bytes(*id))
        })
        .collect();
    let Some(text) = traks.iter().find_map(|trak| {
        let id = track_id(trak).ok()?;
        chapter_ids
            .contains(&id)
            .then(|| text_track(trak))
            .flatten()
    }) else {
        return Ok(Vec::new());
    };
    read_text_samples(file, &text)
}

fn track_id(trak: &[u8]) -> anyhow::Result<u32> {
    let tkhd = child(trak, b"tkhd").context("no tkhd")?;
    // version 1 has 64-bit times: id follows creation and modification times.
    u32_at(tkhd, if tkhd.first() == Some(&1) { 20 } else { 12 })
}

fn text_track(trak: &[u8]) -> Option<TextTrack<'_>> {
    let mdia = child(trak, b"mdia")?;
    let mdhd = child(mdia, b"mdhd")?;
    let timescale = u32_at(mdhd, if mdhd.first() == Some(&1) { 20 } else { 12 }).ok()?;
    let stbl = child(child(mdia, b"minf")?, b"stbl")?;
    Some(TextTrack { timescale, stbl })
}

fn read_text_samples<R: Read + Seek>(
    file: &mut R,
    track: &TextTrack<'_>,
) -> anyhow::Result<Vec<Chapter>> {
    anyhow::ensure!(track.timescale > 0, "chapter track without timescale");
    let stbl = track.stbl;
    // Sample durations.
    let stts = child(stbl, b"stts").context("no stts")?;
    let mut durations = Vec::new();
    for i in 0..u32_at(stts, 4)? as usize {
        let count = u32_at(stts, 8 + i * 8)?;
        let delta = u32_at(stts, 12 + i * 8)?;
        durations.extend(std::iter::repeat_n(u64::from(delta), count as usize));
    }
    // Sample sizes.
    let size_atom = child(stbl, b"stsz").context("no stsz")?;
    let fixed = u32_at(size_atom, 4)?;
    let count = u32_at(size_atom, 8)? as usize;
    let sizes: Vec<u32> = if fixed == 0 {
        (0..count)
            .map(|i| u32_at(size_atom, 12 + i * 4))
            .collect::<anyhow::Result<_>>()?
    } else {
        vec![fixed; count]
    };
    // Chunk offsets and samples per chunk.
    let offsets: Vec<u64> = if let Some(stco) = child(stbl, b"stco") {
        (0..u32_at(stco, 4)? as usize)
            .map(|i| u32_at(stco, 8 + i * 4).map(u64::from))
            .collect::<anyhow::Result<_>>()?
    } else {
        let co64 = child(stbl, b"co64").context("no chunk offsets")?;
        (0..u32_at(co64, 4)? as usize)
            .map(|i| u64_at(co64, 8 + i * 8))
            .collect::<anyhow::Result<_>>()?
    };
    let runs_atom = child(stbl, b"stsc").context("no stsc")?;
    let runs: Vec<(u32, u32)> = (0..u32_at(runs_atom, 4)? as usize)
        .map(|i| {
            Ok((
                u32_at(runs_atom, 8 + i * 12)?,
                u32_at(runs_atom, 12 + i * 12)?,
            ))
        })
        .collect::<anyhow::Result<_>>()?;

    let mut positions = Vec::with_capacity(count);
    for (chunk, offset) in offsets.iter().enumerate() {
        let chunk_no = u32::try_from(chunk + 1)?;
        let per_chunk = runs
            .iter()
            .rev()
            .find(|(first, _)| *first <= chunk_no)
            .map_or(1, |(_, n)| *n);
        let mut at = *offset;
        for _ in 0..per_chunk {
            let Some(size) = sizes.get(positions.len()) else {
                break;
            };
            positions.push((at, *size));
            at += u64::from(*size);
        }
    }

    let mut starts = Vec::with_capacity(positions.len());
    let mut time = 0;
    for (i, (offset, size)) in positions.into_iter().enumerate() {
        file.seek(SeekFrom::Start(offset))?;
        let mut sample = vec![0; size as usize];
        file.read_exact(&mut sample)?;
        let len = sample
            .get(..2)
            .map_or(0, |b| usize::from(u16::from_be_bytes([b[0], b[1]])));
        let text = sample.get(2..2 + len).unwrap_or_default();
        starts.push((time * 1000 / u64::from(track.timescale), decode_text(text)));
        time += durations.get(i).copied().unwrap_or_default();
    }
    let total = time * 1000 / u64::from(track.timescale);
    let mut chapters = close(starts);
    if let Some(last) = chapters.last_mut() {
        last.end_ms = Some(total);
    }
    Ok(chapters)
}

/// `QuickTime` text: UTF-16 with a BOM, otherwise UTF-8.
fn decode_text(text: &[u8]) -> String {
    let utf16 = |bytes: &[u8], be: bool| {
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| {
                if be {
                    u16::from_be_bytes(*c)
                } else {
                    u16::from_le_bytes(*c)
                }
            })
            .collect();
        String::from_utf16_lossy(&units)
    };
    let decoded = match text {
        [0xfe, 0xff, rest @ ..] => utf16(rest, true),
        [0xff, 0xfe, rest @ ..] => utf16(rest, false),
        _ => String::from_utf8_lossy(text).into_owned(),
    };
    decoded.trim().to_owned()
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::Cursor;

    use super::*;

    pub(crate) fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = u32::try_from(body.len() + 8)
            .unwrap()
            .to_be_bytes()
            .to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    pub(crate) fn chpl(chapters: &[(u64, &str)]) -> Vec<u8> {
        let mut body = vec![
            1,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            u8::try_from(chapters.len()).unwrap(),
        ];
        for (start_ms, title) in chapters {
            body.extend_from_slice(&(start_ms * 10_000).to_be_bytes());
            body.push(u8::try_from(title.len()).unwrap());
            body.extend_from_slice(title.as_bytes());
        }
        atom(b"chpl", &body)
    }

    #[test]
    fn reads_nero_chapters() {
        let moov = atom(
            b"moov",
            &atom(b"udta", &chpl(&[(0, "Intro"), (90_000, "Глава 1")])),
        );
        let mut file = atom(b"ftyp", b"M4B \0\0\0\0");
        file.extend(moov);
        let chapters = chapters(&mut Cursor::new(file)).unwrap();
        assert_eq!(chapters.len(), 2);
        assert_eq!(chapters[1].title, "Глава 1");
        assert_eq!(
            (chapters[0].start_ms, chapters[0].end_ms),
            (0, Some(90_000))
        );
        assert_eq!(chapters[1].end_ms, None);
    }

    #[test]
    fn reads_quicktime_chapter_track() {
        // Two text samples of 1000 and 2500 ticks at timescale 1000, stored at the start.
        let samples: Vec<u8> = [&b"\0\x05First"[..], &b"\0\x06Second"[..]].concat();
        let mut file = samples.clone();
        let tkhd = |id: u32| {
            let mut body = vec![0; 12];
            body.extend_from_slice(&id.to_be_bytes());
            body.extend_from_slice(&[0; 64]);
            atom(b"tkhd", &body)
        };
        let mut mdhd = vec![0; 12];
        mdhd.extend_from_slice(&1000u32.to_be_bytes());
        mdhd.extend_from_slice(&[0; 8]);
        let table = |entries: &[u32]| {
            let mut body = vec![0; 4];
            for e in entries {
                body.extend_from_slice(&e.to_be_bytes());
            }
            body
        };
        let stbl = [
            atom(b"stts", &table(&[2, 1, 1000, 1, 2500])),
            atom(b"stsz", &table(&[0, 2, 7, 8])),
            atom(b"stsc", &table(&[1, 1, 2, 1])),
            atom(b"stco", &table(&[1, 0])),
        ]
        .concat();
        let text_trak = atom(
            b"trak",
            &[
                tkhd(2),
                atom(
                    b"mdia",
                    &[atom(b"mdhd", &mdhd), atom(b"minf", &atom(b"stbl", &stbl))].concat(),
                ),
            ]
            .concat(),
        );
        let audio_trak = atom(
            b"trak",
            &[tkhd(1), atom(b"tref", &atom(b"chap", &2u32.to_be_bytes()))].concat(),
        );
        // The samples sit in an mdat-like prefix; the moov follows them.
        file = atom(b"mdat", &file);
        let shift: u32 = 8;
        let fixed: Vec<u8> = text_trak
            .windows(4)
            .position(|w| w == b"stco")
            .map(|at| {
                let mut t = text_trak.clone();
                t[at + 12..at + 16].copy_from_slice(&shift.to_be_bytes());
                t
            })
            .unwrap();
        file.extend(atom(b"moov", &[audio_trak, fixed].concat()));
        let chapters = chapters(&mut Cursor::new(file)).unwrap();
        let got: Vec<_> = chapters
            .iter()
            .map(|c| (c.title.as_str(), c.start_ms, c.end_ms))
            .collect();
        assert_eq!(
            got,
            [("First", 0, Some(1000)), ("Second", 1000, Some(3500))]
        );
        let _ = samples;
    }
}
