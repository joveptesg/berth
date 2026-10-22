//! Audio file inspection with `symphonia` (no decoding): tags, duration, chapters and the
//! embedded cover. MP4 chapters come from [`mp4`], which `symphonia` does not read.

#[doc(hidden)]
pub mod fixtures;
mod mp4;

use std::{fs::File, path::Path};

use anyhow::Context;
use symphonia::core::{
    formats::{FormatOptions, FormatReader, TrackType, probe::Hint},
    io::{MediaSourceStream, MediaSourceStreamOptions},
    meta::{
        ChapterGroup, ChapterGroupItem, MetadataOptions, StandardTag, StandardVisualKey, Tag,
        Visual,
    },
};

/// A chapter inside one file, milliseconds from the start of that file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chapter {
    /// Title; may be empty.
    pub title: String,
    /// Start.
    pub start_ms: u64,
    /// End; `None` for the last chapter when the container does not say.
    pub end_ms: Option<u64>,
}

/// An embedded picture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picture {
    /// Media type such as `image/jpeg`.
    pub mime: String,
    /// Image bytes.
    pub data: Vec<u8>,
}

/// What a file tells about itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AudioInfo {
    /// Playing time.
    pub duration_ms: u64,
    /// Track title.
    pub title: Option<String>,
    /// Album: the book title of a multi-file book.
    pub album: Option<String>,
    /// Track artist.
    pub artist: Option<String>,
    /// Album artist: usually the author.
    pub album_artist: Option<String>,
    /// Narrator (`Narrator` tag).
    pub narrator: Option<String>,
    /// Composer: some taggers put the narrator here, Russian releases often the translator.
    pub composer: Option<String>,
    /// Track number.
    pub track: Option<u64>,
    /// Embedded chapters.
    pub chapters: Vec<Chapter>,
    /// Front cover (or the first picture).
    pub cover: Option<Picture>,
}

/// Inspects an audio file; `ext` (`mp3`, `m4b`, ...) helps the probe.
///
/// # Errors
/// Unreadable or unsupported files.
pub fn inspect(path: &Path, ext: &str) -> anyhow::Result<AudioInfo> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let stream = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());
    let mut hint = Hint::new();
    hint.with_extension(ext);
    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            stream,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .context("unsupported audio format")?;

    let mut info = AudioInfo::default();
    let mut tags: Vec<Tag> = Vec::new();
    let mut visuals: Vec<Visual> = Vec::new();
    {
        let mut metadata = format.metadata();
        while let Some(revision) = metadata.pop() {
            tags.extend(revision.media.tags);
            visuals.extend(revision.media.visuals);
            for track in revision.per_track {
                tags.extend(track.metadata.tags);
                visuals.extend(track.metadata.visuals);
            }
        }
        if let Some(revision) = metadata.current() {
            tags.extend(revision.media.tags.iter().cloned());
            visuals.extend(revision.media.visuals.iter().cloned());
        }
    }
    apply_tags(&mut info, &tags);
    info.cover = pick_cover(visuals);
    if let Some(group) = format.chapters() {
        flatten(group, &mut info.chapters);
    }
    info.duration_ms = duration_ms(format.as_mut())?;
    if info.chapters.is_empty() && matches!(ext, "m4b" | "m4a" | "mp4" | "aac") {
        let mut file = File::open(path)?;
        info.chapters = mp4::chapters(&mut file).unwrap_or_else(|err| {
            tracing::debug!(error = %err, "no mp4 chapters");
            Vec::new()
        });
    }
    Ok(info)
}

/// Russian taggers often write Windows-1251 bytes into Latin-1 ID3 frames: "Ïðèíöèïû" is
/// "Принципы". Text made only of Latin-1 characters, mostly from the upper half where
/// Windows-1251 keeps the Cyrillic alphabet, is read again as Windows-1251.
fn fix_cp1251(text: &str) -> String {
    let high = text
        .chars()
        .filter(|c| ('\u{c0}'..='\u{ff}').contains(c))
        .count();
    let letters = text.chars().filter(|c| c.is_alphabetic()).count();
    if text.chars().any(|c| u32::from(c) > 0xff) || high == 0 || high * 2 < letters {
        return text.to_owned();
    }
    text.chars()
        .map(|c| match u32::from(c) {
            n @ 0xc0..=0xff => char::from_u32(n - 0xc0 + 0x410).unwrap_or(c),
            0xa8 => 'Ё',
            0xb8 => 'ё',
            0xb9 => '№',
            0xab => '«',
            0xbb => '»',
            0x96 => '–',
            0x97 => '—',
            _ => c,
        })
        .collect()
}

fn apply_tags(info: &mut AudioInfo, tags: &[Tag]) {
    let text = |value: &str| {
        let value = value.trim().trim_matches('\0').trim();
        (!value.is_empty()).then(|| fix_cp1251(value))
    };
    let mut composer: Option<String> = None;
    for tag in tags {
        match &tag.std {
            Some(StandardTag::TrackTitle(v)) => info.title = info.title.take().or_else(|| text(v)),
            Some(StandardTag::Album(v)) => info.album = info.album.take().or_else(|| text(v)),
            Some(StandardTag::Artist(v)) => info.artist = info.artist.take().or_else(|| text(v)),
            Some(StandardTag::AlbumArtist(v)) => {
                info.album_artist = info.album_artist.take().or_else(|| text(v));
            }
            Some(StandardTag::Narrator(v)) => {
                info.narrator = info.narrator.take().or_else(|| text(v));
            }
            Some(StandardTag::Composer(v)) => composer = composer.or_else(|| text(v)),
            Some(StandardTag::TrackNumber(n)) => info.track = info.track.or(Some(*n)),
            _ => {}
        }
    }
    info.composer = composer;
}

fn pick_cover(visuals: Vec<Visual>) -> Option<Picture> {
    let front = visuals
        .iter()
        .position(|v| v.usage == Some(StandardVisualKey::FrontCover))
        .unwrap_or(0);
    let visual = visuals.into_iter().nth(front)?;
    let mime = visual
        .media_type
        .filter(|m| m.starts_with("image/"))
        .or_else(|| image_type(&visual.data).map(str::to_owned))?;
    Some(Picture {
        mime,
        data: visual.data.into_vec(),
    })
}

/// Media type from the first bytes of an image.
#[must_use]
pub fn image_type(data: &[u8]) -> Option<&'static str> {
    match data {
        [0xff, 0xd8, 0xff, ..] => Some("image/jpeg"),
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => Some("image/webp"),
        _ => None,
    }
}

fn flatten(group: &ChapterGroup, out: &mut Vec<Chapter>) {
    for item in &group.items {
        match item {
            ChapterGroupItem::Group(inner) => flatten(inner, out),
            ChapterGroupItem::Chapter(chapter) => {
                let title = chapter
                    .tags
                    .iter()
                    .find_map(|t| match &t.std {
                        Some(StandardTag::ChapterTitle(v) | StandardTag::TrackTitle(v)) => {
                            Some(v.trim().to_owned())
                        }
                        _ => None,
                    })
                    .unwrap_or_default();
                let ms =
                    |t: symphonia::core::units::Time| u64::try_from(t.as_millis()).unwrap_or(0);
                out.push(Chapter {
                    title,
                    start_ms: ms(chapter.start_time),
                    end_ms: chapter.end_time.map(ms),
                });
            }
        }
    }
}

/// From the container's frame count, else by walking the packets (no decoding).
fn duration_ms(format: &mut dyn FormatReader) -> anyhow::Result<u64> {
    let track = format
        .default_track(TrackType::Audio)
        .context("no audio track")?;
    let id = track.id;
    let time_base = track.time_base.context("no time base")?;
    if let Some(frames) = track
        .num_frames
        .or(track.duration.map(symphonia::core::units::Duration::get))
    {
        let time =
            time_base.calc_duration_saturating(symphonia::core::units::Duration::new(frames));
        return Ok(u64::try_from(time.as_millis()).unwrap_or(0));
    }
    let mut ticks = 0u64;
    while let Ok(Some(packet)) = format.next_packet() {
        if packet.track_id == id {
            ticks += packet.dur.get();
        }
    }
    let time = time_base.calc_duration_saturating(symphonia::core::units::Duration::new(ticks));
    Ok(u64::try_from(time.as_millis()).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mp3_tags_duration_chapters_and_cover() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("01.mp3");
        let mp3 = fixtures::mp3(&fixtures::Mp3 {
            seconds: 3,
            title: Some("Глава 1"),
            album: Some("Книга"),
            artist: Some("Автор"),
            narrator: Some("Чтец"),
            track: Some(1),
            chapters: &[("Начало", 0, 1000), ("Дальше", 1000, 3000)],
            cover: true,
        });
        std::fs::write(&path, mp3).unwrap();
        let info = inspect(&path, "mp3").unwrap();
        assert!(
            (2950..=3050).contains(&info.duration_ms),
            "{}",
            info.duration_ms
        );
        assert_eq!(info.title.as_deref(), Some("Глава 1"));
        assert_eq!(info.album.as_deref(), Some("Книга"));
        assert_eq!(info.artist.as_deref(), Some("Автор"));
        assert_eq!(info.composer.as_deref(), Some("Чтец"));
        assert_eq!(info.track, Some(1));
        assert_eq!(
            info.chapters
                .iter()
                .map(|c| (c.title.as_str(), c.start_ms))
                .collect::<Vec<_>>(),
            [("Начало", 0), ("Дальше", 1000)]
        );
        assert_eq!(info.cover.unwrap().mime, "image/png");
    }

    #[test]
    fn reads_windows_1251_in_latin_1() {
        assert_eq!(fix_cp1251("Ïðèíöèïû óñïåõà"), "Принципы успеха");
        assert_eq!(fix_cp1251("Ðýé Äàëèî"), "Рэй Далио");
        assert_eq!(fix_cp1251("Café au lait"), "Café au lait");
        assert_eq!(fix_cp1251("Когда упало небо"), "Когда упало небо");
        assert_eq!(fix_cp1251("01"), "01");
    }

    #[test]
    fn m4b_duration_chapters_and_tags() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("book.m4b");
        std::fs::write(
            &path,
            fixtures::m4b(&fixtures::M4b {
                seconds: 120,
                title: "Книга",
                artist: "Автор",
                chapters: &[(0, "Пролог"), (30_000, "Глава 1"), (90_000, "Глава 2")],
            }),
        )
        .unwrap();
        let info = inspect(&path, "m4b").unwrap();
        assert_eq!(info.duration_ms, 120_000);
        assert_eq!(info.chapters.len(), 3);
        assert_eq!(info.chapters[2].title, "Глава 2");
        assert_eq!(info.title.as_deref(), Some("Книга"));
        assert_eq!(info.artist.as_deref(), Some("Автор"));
    }
}
