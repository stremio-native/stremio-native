use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RankingMode {
    #[default]
    Smart,
    Quality,
    Smallest,
    Seeders,
    Original,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DebridAvailability {
    Cached,
    Uncached,
    Unknown,
    ProviderUnavailable,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RankInput {
    pub id: String,
    pub name: String,
    pub description: String,
    pub addon: String,
    pub original_index: usize,
    pub size_bytes: Option<u64>,
    pub seeders: Option<u32>,
    pub debrid: Option<DebridAvailability>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReasonKind {
    Positive,
    Negative,
    Neutral,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ScoreReason {
    pub kind: ReasonKind,
    pub label: String,
    pub points: i32,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ParsedStream {
    pub quality_height: Option<u16>,
    pub source: Option<String>,
    pub codec: Option<String>,
    pub hdr: bool,
    pub dolby_vision: bool,
    pub audio: Option<String>,
    pub languages: Vec<String>,
    pub release_group: Option<String>,
    pub suspicious: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RankedStream {
    pub input: RankInput,
    pub parsed: ParsedStream,
    pub score: i32,
    pub reasons: Vec<ScoreReason>,
    pub filtered: bool,
}

pub fn rank_streams(
    inputs: impl IntoIterator<Item = RankInput>,
    mode: RankingMode,
    show_filtered: bool,
) -> Vec<RankedStream> {
    let mut streams = inputs
        .into_iter()
        .map(rank_one)
        .filter(|stream| show_filtered || !stream.filtered)
        .collect::<Vec<_>>();

    match mode {
        RankingMode::Original => streams.sort_by_key(|stream| stream.input.original_index),
        RankingMode::Quality => streams.sort_by(|left, right| {
            right
                .parsed
                .quality_height
                .cmp(&left.parsed.quality_height)
                .then_with(|| left.input.original_index.cmp(&right.input.original_index))
        }),
        RankingMode::Smallest => streams.sort_by(|left, right| {
            left.input
                .size_bytes
                .unwrap_or(u64::MAX)
                .cmp(&right.input.size_bytes.unwrap_or(u64::MAX))
                .then_with(|| left.input.original_index.cmp(&right.input.original_index))
        }),
        RankingMode::Seeders => streams.sort_by(|left, right| {
            right
                .input
                .seeders
                .unwrap_or_default()
                .cmp(&left.input.seeders.unwrap_or_default())
                .then_with(|| left.input.original_index.cmp(&right.input.original_index))
        }),
        RankingMode::Smart => streams.sort_by(|left, right| {
            right
                .score
                .cmp(&left.score)
                .then_with(|| compare_quality(right, left))
                .then_with(|| left.input.original_index.cmp(&right.input.original_index))
        }),
    }
    streams
}

fn compare_quality(left: &RankedStream, right: &RankedStream) -> Ordering {
    left.parsed.quality_height.cmp(&right.parsed.quality_height)
}

fn rank_one(input: RankInput) -> RankedStream {
    let parsed = parse_stream(&input);
    let mut score = 0;
    let mut reasons = Vec::new();
    let mut reason = |kind, label: String, points| {
        score += points;
        reasons.push(ScoreReason {
            kind,
            label,
            points,
        });
    };

    if let Some(height) = parsed.quality_height {
        let points = match height {
            2160.. => 50,
            1440..=2159 => 42,
            1080..=1439 => 34,
            720..=1079 => 22,
            480..=719 => 10,
            _ => 2,
        };
        reason(ReasonKind::Positive, format!("{height}p video"), points);
    }
    if let Some(source) = parsed.source.as_deref() {
        let points = match source {
            "BluRay" | "REMUX" => 18,
            "WEB-DL" => 13,
            "WEBRip" => 8,
            "HDTV" => 4,
            "CAM" | "TS" => -35,
            _ => 0,
        };
        reason(
            if points >= 0 {
                ReasonKind::Positive
            } else {
                ReasonKind::Negative
            },
            format!("{source} source"),
            points,
        );
    }
    if let Some(codec) = parsed.codec.as_deref() {
        let points = match codec {
            "AV1" => 10,
            "HEVC" => 8,
            "H.264" => 4,
            _ => 0,
        };
        reason(ReasonKind::Positive, format!("{codec} codec"), points);
    }
    if parsed.dolby_vision {
        reason(ReasonKind::Positive, "Dolby Vision".to_owned(), 8);
    } else if parsed.hdr {
        reason(ReasonKind::Positive, "HDR video".to_owned(), 6);
    }
    if let Some(audio) = parsed.audio.as_deref() {
        let points = if audio.contains("Atmos") || audio.contains("TrueHD") {
            8
        } else if audio.contains("DTS") {
            6
        } else {
            2
        };
        reason(ReasonKind::Positive, format!("{audio} audio"), points);
    }
    if let Some(seeders) = input.seeders {
        let points = match seeders {
            500.. => 18,
            100..=499 => 14,
            25..=99 => 9,
            5..=24 => 4,
            1..=4 => 0,
            0 => -18,
        };
        reason(
            if points >= 0 {
                ReasonKind::Positive
            } else {
                ReasonKind::Negative
            },
            format!("{seeders} seeders"),
            points,
        );
    }
    if let Some(size) = input.size_bytes {
        let gib = size as f64 / 1_073_741_824.0;
        let points = if gib <= 0.15 {
            -10
        } else if gib <= 8.0 {
            6
        } else if gib >= 80.0 {
            -12
        } else {
            0
        };
        reason(
            if points >= 0 {
                ReasonKind::Positive
            } else {
                ReasonKind::Negative
            },
            format!("{gib:.1} GiB"),
            points,
        );
    }
    match input.debrid {
        Some(DebridAvailability::Cached) => reason(
            ReasonKind::Positive,
            "Instant on debrid provider".to_owned(),
            32,
        ),
        Some(DebridAvailability::Uncached) => reason(
            ReasonKind::Neutral,
            "Not cached on debrid provider".to_owned(),
            0,
        ),
        Some(DebridAvailability::Unknown | DebridAvailability::ProviderUnavailable) | None => {
            reason(
                ReasonKind::Neutral,
                "Debrid availability unknown".to_owned(),
                0,
            )
        }
    }
    if parsed.suspicious {
        reason(
            ReasonKind::Negative,
            "Potential fake, password, or scam result".to_owned(),
            -1_000,
        );
    }

    RankedStream {
        input,
        parsed: parsed.clone(),
        score,
        reasons,
        filtered: parsed.suspicious,
    }
}

pub fn parse_stream(input: &RankInput) -> ParsedStream {
    let combined = format!("{} {}", input.name, input.description);
    let upper = combined.to_ascii_uppercase();
    let quality_height = [4320_u16, 2160, 1440, 1080, 720, 576, 540, 480, 360]
        .into_iter()
        .find(|height| {
            upper.contains(&format!("{height}P"))
                || (*height == 2160 && upper.contains("4K"))
                || (*height == 4320 && upper.contains("8K"))
        });
    let source = [
        ("REMUX", "REMUX"),
        ("BLURAY", "BluRay"),
        ("BLU-RAY", "BluRay"),
        ("WEB-DL", "WEB-DL"),
        ("WEBDL", "WEB-DL"),
        ("WEBRIP", "WEBRip"),
        ("HDTV", "HDTV"),
        ("CAM", "CAM"),
        ("TELESYNC", "TS"),
    ]
    .into_iter()
    .find_map(|(marker, normalized)| upper.contains(marker).then(|| normalized.to_owned()));
    let codec = [
        (&["AV1", "AV01"][..], "AV1"),
        (&["HEVC", "H265", "H.265", "X265"][..], "HEVC"),
        (&["H264", "H.264", "X264", "AVC"][..], "H.264"),
    ]
    .into_iter()
    .find_map(|(markers, normalized)| {
        markers
            .iter()
            .any(|marker| upper.contains(marker))
            .then(|| normalized.to_owned())
    });
    let dolby_vision = ["DOLBY VISION", "DOVI", " DV "]
        .iter()
        .any(|marker| upper.contains(marker));
    let hdr = dolby_vision
        || ["HDR10+", "HDR10", " HDR ", "HLG"]
            .iter()
            .any(|marker| upper.contains(marker));
    let audio = [
        "TRUEHD ATMOS",
        "ATMOS",
        "TRUEHD",
        "DTS-HD",
        "DTS",
        "EAC3",
        "DDP",
        "AAC",
    ]
    .into_iter()
    .find(|marker| upper.contains(marker))
    .map(ToOwned::to_owned);
    let languages = [
        ("ENGLISH", "English"),
        ("HINDI", "Hindi"),
        ("PORTUGUESE", "Portuguese"),
        ("ARABIC", "Arabic"),
        ("SPANISH", "Spanish"),
        ("FRENCH", "French"),
        ("JAPANESE", "Japanese"),
        ("MULTI", "Multi"),
    ]
    .into_iter()
    .filter(|(marker, _)| upper.contains(marker))
    .map(|(_, language)| language.to_owned())
    .collect();
    let release_group = combined
        .split_whitespace()
        .rev()
        .find_map(|token| token.strip_prefix('-'))
        .filter(|group| !group.is_empty() && group.len() <= 24)
        .map(ToOwned::to_owned);
    let suspicious = [
        "PASSWORD",
        "PASSW0RD",
        "CLICK HERE",
        "FREE BITCOIN",
        "INSTALL CODEC",
        "EXE INCLUDED",
        "SCAM",
        "FAKE",
    ]
    .iter()
    .any(|marker| upper.contains(marker));

    ParsedStream {
        quality_height,
        source,
        codec,
        hdr,
        dolby_vision,
        audio,
        languages,
        release_group,
        suspicious,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(index: usize, name: &str, size: u64, seeders: u32) -> RankInput {
        RankInput {
            id: index.to_string(),
            name: name.to_owned(),
            description: String::new(),
            addon: "Fixture".to_owned(),
            original_index: index,
            size_bytes: Some(size),
            seeders: Some(seeders),
            debrid: None,
        }
    }

    #[test]
    fn original_mode_exactly_preserves_addon_order() {
        let ranked = rank_streams(
            [
                input(2, "2160p REMUX", 50, 10),
                input(0, "720p WEBRip", 1, 100),
                input(1, "1080p WEB-DL", 2, 50),
            ],
            RankingMode::Original,
            false,
        );
        assert_eq!(
            ranked
                .iter()
                .map(|stream| stream.input.original_index)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn smart_ranking_is_deterministic_and_uses_original_order_as_final_tie_breaker() {
        let fixture = [
            input(1, "1080p WEB-DL x265", 2_000_000_000, 50),
            input(0, "1080p WEB-DL x265", 2_000_000_000, 50),
        ];
        let first = rank_streams(fixture.clone(), RankingMode::Smart, false);
        let second = rank_streams(fixture, RankingMode::Smart, false);
        assert_eq!(first, second);
        assert_eq!(first[0].input.original_index, 0);
    }

    #[test]
    fn suspicious_streams_are_hidden_but_recoverable() {
        let fake = input(0, "2160p FAKE install codec", 1, 9999);
        assert!(rank_streams([fake.clone()], RankingMode::Smart, false).is_empty());
        let visible = rank_streams([fake], RankingMode::Smart, true);
        assert!(visible[0].filtered);
    }

    #[test]
    fn provider_outage_is_neutral() {
        let mut unavailable = input(0, "1080p", 2_000_000_000, 20);
        unavailable.debrid = Some(DebridAvailability::ProviderUnavailable);
        let mut unknown = unavailable.clone();
        unknown.debrid = Some(DebridAvailability::Unknown);
        assert_eq!(
            rank_streams([unavailable], RankingMode::Smart, false)[0].score,
            rank_streams([unknown], RankingMode::Smart, false)[0].score
        );
    }
}
