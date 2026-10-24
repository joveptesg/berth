//! Tiny synthetic audio files for tests: silent MP3 frames with ID3v2.4 tags and an M4B
//! header with Nero chapters. Real audiobooks cannot go into a public repository.
#![expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "atom and frame ids are byte-string literals"
)]

/// An MP3 to build.
pub struct Mp3<'a> {
    /// Length in whole seconds of silence.
    pub seconds: u32,
    /// `TIT2`.
    pub title: Option<&'a str>,
    /// `TALB`.
    pub album: Option<&'a str>,
    /// `TPE1`.
    pub artist: Option<&'a str>,
    /// `TCOM`, read as the narrator.
    pub narrator: Option<&'a str>,
    /// `TRCK`.
    pub track: Option<u32>,
    /// `TCON`.
    pub genre: Option<&'a str>,
    /// `TLAN`.
    pub language: Option<&'a str>,
    /// `CHAP` frames: (title, start ms, end ms).
    pub chapters: &'a [(&'a str, u32, u32)],
    /// An `APIC` front cover (a 1×1 PNG).
    pub cover: bool,
}

/// A 1×1 PNG.
pub const PNG: &[u8] = &[
    0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0x0d, b'I', b'H', b'D', b'R', 0, 0, 0,
    1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1f, 0x15, 0xc4, 0x89, 0, 0, 0, 0x0a, b'I', b'D', b'A', b'T',
    0x78, 0x9c, 0x63, 0, 1, 0, 0, 5, 0, 1, 0x0d, 0x0a, 0x2d, 0xb4, 0, 0, 0, 0, b'I', b'E', b'N',
    b'D', 0xae, 0x42, 0x60, 0x82,
];

fn syncsafe(n: usize) -> [u8; 4] {
    let n = u32::try_from(n).expect("small tag");
    [
        ((n >> 21) & 0x7f) as u8,
        ((n >> 14) & 0x7f) as u8,
        ((n >> 7) & 0x7f) as u8,
        (n & 0x7f) as u8,
    ]
}

fn frame(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = id.to_vec();
    out.extend_from_slice(&syncsafe(body.len()));
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(body);
    out
}

fn text_frame(id: &[u8; 4], text: &str) -> Vec<u8> {
    let mut body = vec![3]; // UTF-8
    body.extend_from_slice(text.as_bytes());
    frame(id, &body)
}

/// Builds the MP3: ID3v2.4 tag, then 128 kbit/s 44.1 kHz mono frames of silence.
#[must_use]
pub fn mp3(spec: &Mp3<'_>) -> Vec<u8> {
    let mut frames = Vec::new();
    for (id, value) in [
        (b"TIT2", spec.title),
        (b"TALB", spec.album),
        (b"TPE1", spec.artist),
        (b"TCOM", spec.narrator),
        (b"TCON", spec.genre),
        (b"TLAN", spec.language),
    ] {
        if let Some(value) = value {
            frames.extend(text_frame(id, value));
        }
    }
    if let Some(track) = spec.track {
        frames.extend(text_frame(b"TRCK", &track.to_string()));
    }
    for (i, (title, start, end)) in spec.chapters.iter().enumerate() {
        let mut body = format!("ch{i}\0").into_bytes();
        body.extend_from_slice(&start.to_be_bytes());
        body.extend_from_slice(&end.to_be_bytes());
        body.extend_from_slice(&[0xff; 8]);
        body.extend(text_frame(b"TIT2", title));
        frames.extend(frame(b"CHAP", &body));
    }
    if spec.cover {
        let mut body = vec![3];
        body.extend_from_slice(b"image/png\0");
        body.push(3); // front cover
        body.push(0); // empty description
        body.extend_from_slice(PNG);
        frames.extend(frame(b"APIC", &body));
    }
    let mut out = b"ID3\x04\x00\x00".to_vec();
    out.extend_from_slice(&syncsafe(frames.len()));
    out.extend(frames);

    // 1152 samples per frame at 44.1 kHz; 417 bytes per frame at 128 kbit/s.
    let count = (u64::from(spec.seconds) * 44_100).div_ceil(1152);
    let mut silent = vec![0u8; 417];
    silent[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0xc0]);
    for _ in 0..count {
        out.extend_from_slice(&silent);
    }
    out
}

/// An M4B to build.
pub struct M4b<'a> {
    /// Length.
    pub seconds: u32,
    /// `©nam`.
    pub title: &'a str,
    /// `©ART`.
    pub artist: &'a str,
    /// `©gen`.
    pub genre: Option<&'a str>,
    /// `----:com.apple.iTunes:LANGUAGE`, as Mp3tag writes it.
    pub language: Option<&'a str>,
    /// ISO 639-2 language of the audio track in `mdhd`; `None` for `und`.
    pub track_language: Option<&'a str>,
    /// Nero chapters: (start ms, title).
    pub chapters: &'a [(u64, &'a str)],
}

fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = u32::try_from(body.len() + 8)
        .expect("small atom")
        .to_be_bytes()
        .to_vec();
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out
}

fn full(version_flags: u32, body: &[u8]) -> Vec<u8> {
    let mut out = version_flags.to_be_bytes().to_vec();
    out.extend_from_slice(body);
    out
}

fn ilst_text(kind: &[u8; 4], text: &str) -> Vec<u8> {
    let mut data = 1u32.to_be_bytes().to_vec(); // UTF-8
    data.extend_from_slice(&[0; 4]);
    data.extend_from_slice(text.as_bytes());
    atom(kind, &atom(b"data", &data))
}

/// Builds the M4B: one AAC track at timescale 1000 with two samples, metadata and chapters.
#[must_use]
#[expect(clippy::too_many_lines, reason = "one MP4 layout, atom by atom")]
pub fn m4b(spec: &M4b<'_>) -> Vec<u8> {
    let ms = spec.seconds * 1000;
    let half = ms / 2;
    let samples = [0u8; 16];

    let mut mvhd = vec![0; 12];
    mvhd.extend_from_slice(&1000u32.to_be_bytes());
    mvhd.extend_from_slice(&ms.to_be_bytes());
    mvhd.extend_from_slice(&0x0001_0000u32.to_be_bytes());
    mvhd.extend_from_slice(&[1, 0]);
    mvhd.extend_from_slice(&[0; 10]);
    for v in [0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
        mvhd.extend_from_slice(&v.to_be_bytes());
    }
    mvhd.extend_from_slice(&[0; 24]);
    mvhd.extend_from_slice(&2u32.to_be_bytes());

    let mut tkhd = vec![0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 0];
    tkhd.extend_from_slice(&1u32.to_be_bytes());
    tkhd.extend_from_slice(&[0; 4]);
    tkhd.extend_from_slice(&ms.to_be_bytes());
    tkhd.extend_from_slice(&[0; 8]);
    tkhd.extend_from_slice(&[0, 0, 0, 0, 1, 0, 0, 0]);
    for v in [0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
        tkhd.extend_from_slice(&v.to_be_bytes());
    }
    tkhd.extend_from_slice(&[0; 8]);

    let mut media_header = vec![0; 12];
    media_header.extend_from_slice(&1000u32.to_be_bytes());
    media_header.extend_from_slice(&ms.to_be_bytes());
    let packed = spec.track_language.map_or(0x55c4, |code| {
        code.bytes()
            .fold(0u16, |acc, b| (acc << 5) | u16::from(b - 0x60))
    });
    media_header.extend_from_slice(&packed.to_be_bytes());
    media_header.extend_from_slice(&[0, 0]);

    let mut hdlr = vec![0; 8];
    hdlr.extend_from_slice(b"soun");
    hdlr.extend_from_slice(&[0; 13]);

    // AAC-LC, 44.1 kHz, mono.
    let esds = full(
        0,
        &[
            0x03, 0x19, 0, 1, 0, // ES descriptor
            0x04, 0x11, 0x40, 0x15, 0, 0, 0, 0, 1, 0xf4, 0, 0, 1, 0xf4, 0, // decoder config
            0x05, 0x02, 0x12, 0x08, // AudioSpecificConfig
            0x06, 0x01, 0x02,
        ],
    );
    let mut mp4a = vec![0; 6];
    mp4a.extend_from_slice(&[0, 1]);
    mp4a.extend_from_slice(&[0; 8]);
    mp4a.extend_from_slice(&[0, 1, 0, 16, 0, 0, 0, 0]);
    mp4a.extend_from_slice(&(44_100u32 << 16).to_be_bytes());
    mp4a.extend(atom(b"esds", &esds));
    let stsd = full(
        0,
        &[&1u32.to_be_bytes()[..], &atom(b"mp4a", &mp4a)].concat(),
    );

    let table = |entries: &[u32]| {
        let mut out = vec![0; 4];
        for e in entries {
            out.extend_from_slice(&e.to_be_bytes());
        }
        out
    };
    let stbl_body = |mdat_offset: u32| {
        [
            atom(b"stsd", &stsd),
            atom(b"stts", &table(&[1, 2, half])),
            atom(b"stsc", &table(&[1, 1, 2, 1])),
            atom(b"stsz", &table(&[0, 2, 8, 8])),
            atom(b"stco", &table(&[1, mdat_offset])),
        ]
        .concat()
    };
    let moov = |mdat_offset: u32| {
        let minf = [
            atom(b"smhd", &[0; 8]),
            atom(
                b"dinf",
                &atom(
                    b"dref",
                    &full(
                        0,
                        &[0, 0, 0, 1, 0, 0, 0, 12, b'u', b'r', b'l', b' ', 0, 0, 0, 1],
                    ),
                ),
            ),
            atom(b"stbl", &stbl_body(mdat_offset)),
        ]
        .concat();
        let mdia = [
            atom(b"mdhd", &media_header),
            atom(b"hdlr", &hdlr),
            atom(b"minf", &minf),
        ]
        .concat();
        let trak = [atom(b"tkhd", &tkhd), atom(b"mdia", &mdia)].concat();

        let mut chpl = vec![
            1,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            u8::try_from(spec.chapters.len()).expect("few chapters"),
        ];
        for (start, title) in spec.chapters {
            chpl.extend_from_slice(&(start * 10_000).to_be_bytes());
            chpl.push(u8::try_from(title.len()).expect("short title"));
            chpl.extend_from_slice(title.as_bytes());
        }
        let mut meta_hdlr = vec![0; 8];
        meta_hdlr.extend_from_slice(b"mdir");
        meta_hdlr.extend_from_slice(b"appl");
        meta_hdlr.extend_from_slice(&[0; 9]);
        let mut ilst = [
            ilst_text(b"\xa9nam", spec.title),
            ilst_text(b"\xa9ART", spec.artist),
        ]
        .concat();
        if let Some(genre) = spec.genre {
            ilst.extend(ilst_text(b"\xa9gen", genre));
        }
        if let Some(language) = spec.language {
            let mut data = 1u32.to_be_bytes().to_vec();
            data.extend_from_slice(&[0; 4]);
            data.extend_from_slice(language.as_bytes());
            ilst.extend(atom(
                b"----",
                &[
                    atom(b"mean", &full(0, b"com.apple.iTunes")),
                    atom(b"name", &full(0, b"LANGUAGE")),
                    atom(b"data", &data),
                ]
                .concat(),
            ));
        }
        let meta = full(
            0,
            &[atom(b"hdlr", &meta_hdlr), atom(b"ilst", &ilst)].concat(),
        );
        let udta = [atom(b"chpl", &chpl), atom(b"meta", &meta)].concat();
        atom(
            b"moov",
            &[
                atom(b"mvhd", &mvhd),
                atom(b"trak", &trak),
                atom(b"udta", &udta),
            ]
            .concat(),
        )
    };
    let ftyp = atom(b"ftyp", b"M4B \0\0\0\0M4B mp42isom");
    // The sample offset depends on the moov size, which does not depend on the offset value.
    let offset = ftyp.len() + moov(0).len() + 8;
    let mut out = ftyp;
    out.extend(moov(u32::try_from(offset).expect("small file")));
    out.extend(atom(b"mdat", &samples));
    out
}
