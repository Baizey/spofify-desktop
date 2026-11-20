//! Read-only media projections for the exact authenticated origin. Do not persist these.
//!
//! API integration helpers (implemented in `api.rs`):
//! ```ignore
//! impl Api {
//!     // Validate API/session ownership and liveness; return canonical origin and
//!     // the immutable authenticated account ID (not mutable presentation fields).
//!     pub(crate) fn authenticated_account<'a, 's>(&'a self, session: &'s Session)
//!         -> Result<(&'a str, &'s str), ApiError>;
//!     // Authenticated GET of one encoded track ID and a fixed resource suffix.
//!     // Same-origin only; no redirects, retries, proxies, query, or absolute URL.
//!     // Require 200 and exactly one Content-Type, normalized to MIME essence;
//!     // enforce the allowed MIME list and max_bytes (including chunked bodies),
//!     // check the session before/after I/O, and preserve 401 revocation/status.
//!     pub(crate) async fn track_resource(
//!         &self, session: &Session, track_id: &str, resource: crate::media::TrackResource,
//!     ) -> Result<(String, Vec<u8>), ApiError>;
//! }
//! ```

use std::{io::Cursor, time::Duration};

use image::{ImageDecoder, ImageFormat, ImageReader, Limits};
use serde::Deserialize;
use url::Url;

use crate::api::{Api, ApiError, Session};

const INVALID: ApiError = ApiError {
    message: "This track’s media details are unavailable.",
    expired: false,
    status: 0,
};
const ALGORITHM: &str = "peak-u8-two-pass-v1-half-up";
const MAX_PIXELS: u32 = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TrackResource {
    Lyrics,
    Waveform,
    Artwork,
}

impl TrackResource {
    pub(crate) fn suffix(self) -> &'static str {
        match self {
            Self::Lyrics => "lyrics",
            Self::Waveform => "waveform",
            Self::Artwork => "artwork",
        }
    }

    pub(crate) fn accept(self) -> &'static str {
        match self {
            Self::Lyrics | Self::Waveform => "application/json",
            Self::Artwork => "image/png, image/jpeg",
        }
    }

    pub(crate) fn accepts(self, mime: &str) -> bool {
        match self {
            Self::Lyrics | Self::Waveform => mime == "application/json",
            Self::Artwork => matches!(mime, "image/png" | "image/jpeg"),
        }
    }

    pub(crate) fn max_bytes(self) -> usize {
        match self {
            Self::Lyrics => 260 * 1024,
            Self::Waveform => 16 * 1024,
            Self::Artwork => 2 * 1024 * 1024,
        }
    }
}

/// Caller must apply the current session, track and request-occurrence fence
/// before accepting a response. No fetch loop, disk cache or external requests.
pub async fn lyrics(api: &Api, session: &Session, track_id: &str) -> Result<Lyrics, ApiError> {
    let (_, body) = api
        .track_resource(session, track_id, TrackResource::Lyrics)
        .await?;
    let result = parse_lyrics(&body, track_id)?;
    api.authenticated_account(session)?;
    Ok(result)
}

pub async fn waveform(api: &Api, session: &Session, track_id: &str) -> Result<Waveform, ApiError> {
    let (_, body) = api
        .track_resource(session, track_id, TrackResource::Waveform)
        .await?;
    let result = parse_waveform(&body, track_id)?;
    api.authenticated_account(session)?;
    Ok(result)
}

/// A 404 is an ordinary no-cover result. Use the application's static fallback.
pub async fn artwork(
    api: &Api,
    session: &Session,
    track_id: &str,
) -> Result<Option<Artwork>, ApiError> {
    let network = session.network_changes();
    let (mime, body) = match api
        .track_resource(session, track_id, TrackResource::Artwork)
        .await
    {
        Ok(value) => value,
        Err(err) if err.status == 404 => {
            api.authenticated_account(session)?;
            return Ok(None);
        }
        Err(err) => return Err(err),
    };
    let result = tokio::task::spawn_blocking(move || decode_artwork(&mime, &body))
        .await
        .map_err(|_| INVALID)??;
    api.authenticated_account(session)?;
    if network.has_changed().unwrap_or(true) || session.is_offline() {
        return Err(INVALID);
    }
    Ok(Some(result))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LyricsState {
    Pending,
    Plain,
    Timed,
    Instrumental,
    NotFound,
    Unavailable,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LyricsReason {
    SourceNotConfigured,
    LocalInvalid,
    SourceFailed,
    NoMatch,
    SourceDeferred,
    SourceInvalid,
    RecordingUnavailable,
    AwaitingPlayback,
    AwaitingProcessing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LyricsProvenance {
    LocalLrc,
    Embedded,
    Provider,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttributionOwnership {
    Library,
    Provider,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Attribution {
    pub name: String,
    pub ownership: AttributionOwnership,
    /// Display data only, never a fetch target.
    pub url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LyricLine {
    pub start_ms: u64,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lyrics {
    pub track_id: String,
    pub recording_version: String,
    pub source_generation: String,
    pub state: LyricsState,
    pub plain_text: Option<String>,
    pub lines: Vec<LyricLine>,
    pub reason: Option<LyricsReason>,
    pub provenance: Option<LyricsProvenance>,
    pub attribution: Option<Attribution>,
}

impl Lyrics {
    /// Last authored line at or before the observed native playback position.
    /// Suppress sync if actual playback duration is unknown or the text extends
    /// outside it. Plain lyrics and unverified audio never gain an active line.
    #[cfg(test)]
    pub fn active_line(
        &self,
        position: Duration,
        actual_duration: Option<Duration>,
    ) -> Option<usize> {
        let duration = actual_duration?;
        if self.state != LyricsState::Timed
            || duration.is_zero()
            || position > duration
            || Duration::from_millis(self.lines.last()?.start_ms) > duration
        {
            return None;
        }
        self.lines
            .partition_point(|line| Duration::from_millis(line.start_ms) <= position)
            .checked_sub(1)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LyricsWire {
    version: u8,
    track_id: String,
    recording_version: String,
    source_generation: String,
    state: LyricsState,
    plain_text: Option<String>,
    lines: Vec<LyricLine>,
    reason: Option<LyricsReason>,
    provenance: Option<LyricsProvenance>,
    attribution: Option<Attribution>,
    offline_transfer_allowed: bool,
}

fn parse_lyrics(body: &[u8], id: &str) -> Result<Lyrics, ApiError> {
    if body.len() > TrackResource::Lyrics.max_bytes() {
        return Err(INVALID);
    }
    let mut wire: LyricsWire = serde_json::from_slice(body).map_err(|_| INVALID)?;
    if wire.version != 1
        || wire.track_id != id
        || !identity(&wire.recording_version)
        || wire.source_generation != "local-v1"
        || wire.offline_transfer_allowed
        || wire.lines.len() > 2048
    {
        return Err(INVALID);
    }
    let text = wire.plain_text.as_deref().unwrap_or_default();
    if !lyric_text(text, 64 * 1024) {
        return Err(INVALID);
    }
    match wire.state {
        LyricsState::Plain
            if !text.trim().is_empty()
                && wire.lines.is_empty()
                && text.split('\n').count() <= 2048
                && text.split('\n').all(|line| line.len() <= 4096) =>
        {
            ()
        }
        LyricsState::Timed
            if text.is_empty()
                && wire.lines.iter().map(|line| line.text.len()).sum::<usize>() <= 64 * 1024
                && wire
                    .lines
                    .windows(2)
                    .all(|lines| lines[0].start_ms <= lines[1].start_ms)
                && wire
                    .lines
                    .iter()
                    .all(|line| line.start_ms <= 86_400_000 && lyric_text(&line.text, 4096))
                && wire.lines.iter().any(|line| !line.text.trim().is_empty()) =>
        {
            ()
        }
        LyricsState::Pending
        | LyricsState::Instrumental
        | LyricsState::NotFound
        | LyricsState::Unavailable
        | LyricsState::Error
            if text.is_empty() && wire.lines.is_empty() =>
        {
            ()
        }
        _ => return Err(INVALID),
    }
    if let Some(attribution) = &mut wire.attribution {
        if attribution.name.trim().is_empty() || !lyric_text(&attribution.name, 256) {
            return Err(INVALID);
        }
        if attribution
            .url
            .as_deref()
            .is_some_and(|url| !safe_attribution_url(url))
        {
            attribution.url = None;
        }
    } else if matches!(
        wire.state,
        LyricsState::Plain | LyricsState::Timed | LyricsState::Instrumental
    ) {
        return Err(INVALID);
    }
    Ok(Lyrics {
        track_id: wire.track_id,
        recording_version: wire.recording_version,
        source_generation: wire.source_generation,
        state: wire.state,
        plain_text: wire.plain_text.filter(|text| !text.is_empty()),
        lines: wire.lines,
        reason: wire.reason,
        provenance: wire.provenance,
        attribution: wire.attribution,
    })
}

fn lyric_text(text: &str, max: usize) -> bool {
    text.len() <= max
        && !text
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
}

fn safe_attribution_url(raw: &str) -> bool {
    raw.len() <= 2048
        && raw.starts_with("https://")
        && !raw.contains(['?', '#', '\\'])
        && raw.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
        && !raw[8..].split('/').next().unwrap_or("").contains('@')
        && Url::parse(raw).is_ok_and(|url| {
            url.has_host() && url.username().is_empty() && url.password().is_none()
        })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaveformState {
    Ready,
    Unavailable,
    Queued,
    Running,
    Stale,
    Unsupported,
    Missing,
    Error,
    Timeout,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Waveform {
    pub track_id: String,
    pub state: WaveformState,
    pub source_generation: String,
    pub product_version: Option<String>,
    pub envelope: Option<Envelope>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Envelope {
    pub frames: u64,
    pub sample_rate: u32,
    pub channels: u8,
    pub duration: Duration,
    pub bins: Vec<u8>,
}

impl Envelope {
    /// Discard only the visual decoration on a mismatch; normal seek remains available.
    pub fn matches_duration(&self, actual: Duration) -> bool {
        !actual.is_zero()
            && !self.bins.is_empty()
            && self.duration.abs_diff(actual)
                <= Duration::from_millis(250).max(self.duration / self.bins.len() as u32)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WaveformWire {
    version: u8,
    track_id: String,
    state: WaveformState,
    source_generation: String,
    algorithm_version: String,
    product_version: Option<String>,
    frames: Option<u64>,
    sample_rate: Option<u32>,
    channels: Option<u8>,
    duration_seconds: Option<f64>,
    bins: Option<Vec<u8>>,
}

fn parse_waveform(body: &[u8], id: &str) -> Result<Waveform, ApiError> {
    if body.len() > TrackResource::Waveform.max_bytes() {
        return Err(INVALID);
    }
    let wire: WaveformWire = serde_json::from_slice(body).map_err(|_| INVALID)?;
    if wire.version != 1
        || wire.track_id != id
        || wire.algorithm_version != ALGORITHM
        || (!wire.source_generation.is_empty() && !identity(&wire.source_generation))
        || wire
            .product_version
            .as_deref()
            .is_some_and(|value| !identity(value))
    {
        return Err(INVALID);
    }
    let envelope = match (
        wire.state,
        wire.frames,
        wire.sample_rate,
        wire.channels,
        wire.duration_seconds,
        wire.bins,
    ) {
        (
            WaveformState::Ready,
            Some(frames),
            Some(sample_rate @ 1..=192_000),
            Some(channels @ 1..=2),
            Some(seconds),
            Some(bins),
        ) => {
            if wire.source_generation.is_empty()
                || wire.product_version.is_none()
                || frames == 0
                || frames > u64::from(sample_rate) * 7200
                || bins.len() as u64 != frames.min(2048)
            {
                return Err(INVALID);
            }
            let duration = frames as f64 / f64::from(sample_rate);
            if !seconds.is_finite() || seconds <= 0.0 || (duration - seconds).abs() > 1e-8 {
                return Err(INVALID);
            }
            Some(Envelope {
                frames,
                sample_rate,
                channels,
                duration: Duration::from_secs_f64(duration),
                bins,
            })
        }
        (state, None, None, None, None, None) if state != WaveformState::Ready => None,
        _ => return Err(INVALID),
    };
    Ok(Waveform {
        track_id: wire.track_id,
        state: wire.state,
        source_generation: wire.source_generation,
        product_version: wire.product_version,
        envelope,
    })
}

fn identity(s: &str) -> bool {
    !s.is_empty() && s.len() <= 256 && !s.chars().any(|c| c.is_control() || c.is_whitespace())
}

pub struct Artwork {
    pub width: u32,
    pub height: u32,
    /// Row-major, tightly packed, straight-alpha RGBA8; no encoded metadata/URLs.
    pub rgba: Vec<u8>,
}

fn decode_artwork(mime: &str, body: &[u8]) -> Result<Artwork, ApiError> {
    if body.is_empty() || body.len() > TrackResource::Artwork.max_bytes() {
        return Err(INVALID);
    }
    let format = match mime {
        "image/png" => ImageFormat::Png,
        "image/jpeg" => ImageFormat::Jpeg,
        _ => return Err(INVALID),
    };
    if image::guess_format(body).ok() != Some(format) {
        return Err(INVALID);
    }
    let mut reader = ImageReader::with_format(Cursor::new(body), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let decoder = reader.into_decoder().map_err(|_| INVALID)?;
    let (width, height) = decoder.dimensions();
    if width == 0 || height == 0 || width > 4096 || height > 4096 || width * height > MAX_PIXELS {
        return Err(INVALID);
    }
    let rgba = image::DynamicImage::from_decoder(decoder)
        .map_err(|_| INVALID)?
        .into_rgba8()
        .into_raw();
    Ok(Artwork {
        width,
        height,
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{
        ExtendedColorType, ImageEncoder,
        codecs::{jpeg::JpegEncoder, png::PngEncoder},
    };
    use serde_json::{Value, json};

    fn timed() -> Value {
        json!({"version":1,"trackId":"track","recordingVersion":"recording","sourceGeneration":"local-v1",
            "state":"timed","lines":[{"startMs":250,"text":"first"},{"startMs":250,"text":"second"},
            {"startMs":900,"text":""}],"offlineTransferAllowed":false,"provenance":"local_lrc",
            "attribution":{"name":"Fixture","ownership":"library","url":"https://credit.invalid/"}})
    }

    #[test]
    fn timed_lyrics_use_actual_playback_and_keep_equal_timestamps() {
        let data = timed();
        let body = serde_json::to_vec(&data).unwrap();
        let model = parse_lyrics(&body, "track").unwrap();
        let ms = Duration::from_millis;
        assert_eq!(model.active_line(ms(249), Some(ms(1000))), None);
        assert_eq!(model.active_line(ms(250), Some(ms(1000))), Some(1));
        assert_eq!(model.active_line(ms(900), Some(ms(1000))), Some(2));
        assert_eq!(model.active_line(ms(250), None), None);
        assert_eq!(model.active_line(ms(250), Some(ms(899))), None);
        assert!(parse_lyrics(&body, "other").is_err());
    }

    #[test]
    fn plain_lyrics_do_not_invent_timing_and_reject_mixed_payloads() {
        fn parse(data: &Value) -> Result<Lyrics, ApiError> {
            parse_lyrics(&serde_json::to_vec(data).unwrap(), "track")
        }
        let mut data = timed();
        data["state"] = "plain".into();
        data["plainText"] = "Literal lyrics\nsecond line".into();
        assert!(parse(&data).is_err());
        data["lines"] = json!([]);
        data["attribution"]["url"] = "https://credit.invalid/?".into();
        let model = parse(&data).unwrap();
        assert!(model.lines.is_empty());
        assert_eq!(
            model.active_line(Duration::ZERO, Some(Duration::from_secs(1))),
            None
        );
        assert_eq!(model.attribution.unwrap().url, None);
        data["offlineTransferAllowed"] = true.into();
        assert!(parse(&data).is_err());
    }

    #[test]
    fn waveform_distinguishes_real_silence_from_missing_and_validates_shape() {
        fn parse(data: &Value) -> Result<Waveform, ApiError> {
            parse_waveform(&serde_json::to_vec(data).unwrap(), "track")
        }
        let mut data = json!({"version":1,"trackId":"track","state":"ready","sourceGeneration":"source",
            "algorithmVersion":ALGORITHM,"productVersion":"product","frames":3,"sampleRate":48_000,
            "channels":2,"durationSeconds":0.0000625,"bins":[0,0,0]});
        let model = parse(&data).unwrap().envelope.unwrap();
        assert_eq!(model.bins, [0, 0, 0]);
        assert!(model.matches_duration(Duration::from_millis(1)));
        assert!(!model.matches_duration(Duration::from_millis(251)));
        data["bins"] = json!([0, 256, 0]);
        assert!(parse(&data).is_err());
        data["bins"] = json!([0, 0]);
        assert!(parse(&data).is_err());
        data["bins"] = json!([0, 0, 0]);
        data["durationSeconds"] = 1.into();
        assert!(parse(&data).is_err());
        data["state"] = "missing".into();
        assert!(parse(&data).is_err());
        let missing = json!({"version":1,"trackId":"track","state":"missing","sourceGeneration":"","algorithmVersion":ALGORITHM});
        assert!(parse(&missing).unwrap().envelope.is_none());
    }

    #[test]
    fn artwork_decodes_png_jpeg_and_rejects_wrong_format_or_limits() {
        let mut png = Vec::new();
        PngEncoder::new(&mut png)
            .write_image(&[8, 16, 32, 128], 1, 1, ExtendedColorType::Rgba8)
            .unwrap();
        let image = decode_artwork("image/png", &png).unwrap();
        assert_eq!((image.width, image.height), (1, 1));
        assert_eq!(image.rgba, [8, 16, 32, 128]);
        assert!(decode_artwork("image/jpeg", &png).is_err());
        assert!(decode_artwork("image/png", &png[..28]).is_err());
        let mut jpeg = Vec::new();
        JpegEncoder::new(&mut jpeg)
            .encode(&[80, 80, 80], 1, 1, ExtendedColorType::Rgb8)
            .unwrap();
        let image = decode_artwork("image/jpeg", &jpeg).unwrap();
        assert_eq!((image.width, image.height, image.rgba[3]), (1, 1, 255));
        assert!(
            decode_artwork(
                "image/png",
                &vec![0; TrackResource::Artwork.max_bytes() + 1]
            )
            .is_err()
        );
    }
}
