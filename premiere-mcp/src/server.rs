//! Premiere Pro MCP server — pure-compute timecode and sequence math.
//!
//! The focus is SMPTE **drop-frame** timecode (29.97 / 59.94 fps), the classic
//! correctness minefield in non-linear editing: drop-frame *labels* skip two
//! (or four) frame numbers every minute except every tenth minute, so naive
//! `frames = hh*3600*fps + …` math drifts by ~3.6 seconds per hour. These
//! tools implement the standard conversion exactly, plus sequence duration and
//! frame-rate conform. All pure-compute — no host application.

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler};
use schemars::JsonSchema;
use serde::Deserialize;

/// Premiere Pro MCP server. Stateless — one instance per request.
#[derive(Clone)]
pub struct PremiereServer {
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FramesToTimecodeParams {
    /// Absolute frame number (0-based), must be >= 0.
    pub frames: u64,
    /// Frame rate, e.g. 23.976, 24, 25, 29.97, 30, 59.94, 60.
    pub fps: f64,
    /// Emit drop-frame timecode (only valid for 29.97 / 59.94). Default false.
    #[serde(default)]
    pub drop_frame: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TimecodeToFramesParams {
    /// Timecode `HH:MM:SS:FF` (non-drop) or `HH:MM:SS;FF` (drop-frame).
    pub timecode: String,
    /// Frame rate the timecode is expressed in.
    pub fps: f64,
    /// Interpret as drop-frame. If omitted, inferred from a `;` frame
    /// separator in the timecode.
    #[serde(default)]
    pub drop_frame: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct OffsetTimecodeParams {
    /// Starting timecode.
    pub timecode: String,
    /// Frame rate.
    pub fps: f64,
    /// Frames to add (may be negative). The result is clamped at 0.
    pub offset_frames: i64,
    /// Drop-frame. If omitted, inferred from the input separator.
    #[serde(default)]
    pub drop_frame: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SequenceDurationParams {
    /// Per-clip durations in frames.
    pub clip_frames: Vec<u64>,
    /// Frame rate.
    pub fps: f64,
    /// Emit the total as drop-frame timecode. Default false.
    #[serde(default)]
    pub drop_frame: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ConformTimecodeParams {
    /// Source timecode.
    pub timecode: String,
    /// Source frame rate.
    pub from_fps: f64,
    /// Target frame rate.
    pub to_fps: f64,
    /// Source drop-frame. If omitted, inferred from the input separator.
    #[serde(default)]
    pub from_drop_frame: Option<bool>,
    /// Target drop-frame. Default false.
    #[serde(default)]
    pub to_drop_frame: bool,
}

#[tool_router]
impl PremiereServer {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    /// Frame number → SMPTE timecode (drop-frame or non-drop).
    #[tool(
        description = "Convert a frame number to SMPTE timecode; set drop_frame for 29.97/59.94"
    )]
    #[tracing::instrument(name = "tool.frames_to_timecode", skip(self))]
    async fn frames_to_timecode(
        &self,
        Parameters(params): Parameters<FramesToTimecodeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let rate = Rate::new(params.fps, params.drop_frame)?;
        let timecode = rate.frames_to_timecode(params.frames);
        Ok(CallToolResult::structured(serde_json::json!({
            "timecode": timecode,
            "frames": params.frames,
            "fps": params.fps,
            "drop_frame": params.drop_frame,
        })))
    }

    /// SMPTE timecode → frame number (drop-frame or non-drop).
    #[tool(description = "Convert SMPTE timecode to a frame number; drop-frame inferred from ';'")]
    #[tracing::instrument(name = "tool.timecode_to_frames", skip(self))]
    async fn timecode_to_frames(
        &self,
        Parameters(params): Parameters<TimecodeToFramesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let df = params
            .drop_frame
            .unwrap_or_else(|| params.timecode.contains(';'));
        let rate = Rate::new(params.fps, df)?;
        let fields = TimecodeFields::parse(&params.timecode)?;
        let frames = rate.timecode_to_frames(&fields)?;
        Ok(CallToolResult::structured(serde_json::json!({
            "frames": frames,
            "timecode": params.timecode,
            "fps": params.fps,
            "drop_frame": df,
        })))
    }

    /// Offset a timecode by a (possibly negative) number of frames.
    #[tool(description = "Add or subtract frames from a timecode, returning the new timecode")]
    #[tracing::instrument(name = "tool.offset_timecode", skip(self))]
    async fn offset_timecode(
        &self,
        Parameters(params): Parameters<OffsetTimecodeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let df = params
            .drop_frame
            .unwrap_or_else(|| params.timecode.contains(';'));
        let rate = Rate::new(params.fps, df)?;
        let fields = TimecodeFields::parse(&params.timecode)?;
        let start = rate.timecode_to_frames(&fields)?;
        let result = start.saturating_add_signed(params.offset_frames);
        Ok(CallToolResult::structured(serde_json::json!({
            "timecode": rate.frames_to_timecode(result),
            "frames": result,
            "start_frames": start,
            "offset_frames": params.offset_frames,
        })))
    }

    /// Total duration of a sequence of clips (in frames and as timecode).
    #[tool(description = "Sum per-clip frame durations into a total and a timecode")]
    #[tracing::instrument(name = "tool.sequence_duration", skip(self))]
    async fn sequence_duration(
        &self,
        Parameters(params): Parameters<SequenceDurationParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let rate = Rate::new(params.fps, params.drop_frame)?;
        let mut total: u64 = 0;
        for frames in &params.clip_frames {
            total = total.checked_add(*frames).ok_or_else(|| {
                ErrorData::invalid_params("total frame count overflowed".to_owned(), None)
            })?;
        }
        Ok(CallToolResult::structured(serde_json::json!({
            "total_frames": total,
            "timecode": rate.frames_to_timecode(total),
            "clip_count": params.clip_frames.len(),
            "fps": params.fps,
        })))
    }

    /// Conform a timecode from one frame rate to another, preserving real
    /// (wall-clock) time — a clip's on-screen duration, not its frame count.
    #[tool(description = "Retime a timecode from one fps to another, preserving real duration")]
    #[tracing::instrument(name = "tool.conform_timecode", skip(self))]
    async fn conform_timecode(
        &self,
        Parameters(params): Parameters<ConformTimecodeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let from_df = params
            .from_drop_frame
            .unwrap_or_else(|| params.timecode.contains(';'));
        let from_rate = Rate::new(params.from_fps, from_df)?;
        let to_rate = Rate::new(params.to_fps, params.to_drop_frame)?;
        let fields = TimecodeFields::parse(&params.timecode)?;
        let source_frames = from_rate.timecode_to_frames(&fields)?;
        // Real seconds elapsed, using the *actual* (fractional) source rate.
        let seconds = source_frames as f64 / params.from_fps;
        let target_frames = (seconds * params.to_fps).round();
        if !target_frames.is_finite() || target_frames < 0.0 {
            return Err(ErrorData::invalid_params(
                "conform produced an out-of-range frame count".to_owned(),
                None,
            ));
        }
        let target_frames = target_frames as u64;
        Ok(CallToolResult::structured(serde_json::json!({
            "timecode": to_rate.frames_to_timecode(target_frames),
            "frames": target_frames,
            "seconds": seconds,
            "source_frames": source_frames,
        })))
    }
}

/// A validated frame rate: the nominal integer rate (24, 25, 30, 60) plus
/// whether drop-frame counting applies and how many frames are dropped per
/// minute.
struct Rate {
    /// Nominal frames per second, `fps.round()`, guaranteed `>= 1`.
    nominal: u64,
    /// Frames dropped at each non-tenth minute boundary (`0` for non-drop).
    drop_per_min: u64,
}

impl Rate {
    /// Validates `fps` and the drop-frame flag.
    ///
    /// Drop-frame is only meaningful for the NTSC rates whose nominal rate is
    /// a multiple of 30 (29.97 → 30, 59.94 → 60) and which are actually
    /// fractional; requesting it for 24/25/30 exact is an error.
    fn new(fps: f64, drop_frame: bool) -> Result<Self, ErrorData> {
        if !fps.is_finite() || fps <= 0.0 {
            return Err(ErrorData::invalid_params(
                "fps must be a positive, finite number".to_owned(),
                None,
            ));
        }
        let nominal = fps.round();
        if nominal < 1.0 {
            return Err(ErrorData::invalid_params(
                "fps must round to at least 1 frame per second".to_owned(),
                None,
            ));
        }
        let nominal = nominal as u64;
        let drop_per_min = if drop_frame {
            if !nominal.is_multiple_of(30) {
                return Err(ErrorData::invalid_params(
                    "drop-frame is only valid for 29.97 and 59.94 fps".to_owned(),
                    None,
                ));
            }
            if (fps - nominal as f64).abs() < 0.001 {
                return Err(ErrorData::invalid_params(
                    "drop-frame requires a fractional NTSC rate (29.97/59.94), not an exact rate"
                        .to_owned(),
                    None,
                ));
            }
            // 2 frames per minute at 30, 4 at 60.
            nominal / 15
        } else {
            0
        };
        Ok(Self {
            nominal,
            drop_per_min,
        })
    }

    /// Frame number → `HH:MM:SS:FF` (non-drop) or `HH:MM:SS;FF` (drop-frame).
    ///
    /// Drop-frame algorithm per SMPTE / the standard reference implementation.
    /// Saturating arithmetic keeps a pathological near-`u64::MAX` frame number
    /// from wrapping to a wrong (rather than merely huge) timecode.
    fn frames_to_timecode(&self, frame_number: u64) -> String {
        let fps = self.nominal;
        if self.drop_per_min == 0 {
            let ff = frame_number % fps;
            let secs = frame_number / fps;
            let ss = secs % 60;
            let mm = (secs / 60) % 60;
            let hh = secs / 3600;
            return format!("{hh:02}:{mm:02}:{ss:02}:{ff:02}");
        }

        let d = self.drop_per_min;
        let frames_per_10min = fps * 60 * 10 - 9 * d;
        let frames_per_min = fps * 60 - d;
        let tenmin = frame_number / frames_per_10min;
        let rem = frame_number % frames_per_10min;
        // Add back the dropped frame numbers so the label matches the clock.
        let extra = if rem >= d {
            d.saturating_mul(9)
                .saturating_mul(tenmin)
                .saturating_add(d.saturating_mul((rem - d) / frames_per_min))
        } else {
            d.saturating_mul(9).saturating_mul(tenmin)
        };
        let adjusted = frame_number.saturating_add(extra);
        let ff = adjusted % fps;
        let ss = (adjusted / fps) % 60;
        let mm = (adjusted / fps / 60) % 60;
        let hh = adjusted / fps / 3600;
        format!("{hh:02}:{mm:02}:{ss:02};{ff:02}")
    }

    /// `HH:MM:SS:FF` fields → absolute frame number.
    fn timecode_to_frames(&self, f: &TimecodeFields) -> Result<u64, ErrorData> {
        if f.ff >= self.nominal {
            return Err(ErrorData::invalid_params(
                format!(
                    "frame field {} is out of range for {} fps",
                    f.ff, self.nominal
                ),
                None,
            ));
        }
        if f.ss >= 60 || f.mm >= 60 {
            return Err(ErrorData::invalid_params(
                "minutes and seconds must be < 60".to_owned(),
                None,
            ));
        }
        if self.drop_per_min > 0
            && f.ss == 0
            && !f.mm.is_multiple_of(10)
            && f.ff < self.drop_per_min
        {
            return Err(ErrorData::invalid_params(
                format!(
                    "{:02}:{:02}:{:02};{:02} is not a valid drop-frame timecode \
                     (frames 0..{} are dropped at this minute)",
                    f.hh, f.mm, f.ss, f.ff, self.drop_per_min
                ),
                None,
            ));
        }
        let fps = self.nominal;
        let total_minutes =
            f.hh.checked_mul(60)
                .and_then(|v| v.checked_add(f.mm))
                .ok_or_else(overflow)?;
        // Base frame count as if no frames were dropped.
        let base =
            f.hh.checked_mul(3600)
                .and_then(|v| v.checked_add(f.mm * 60))
                .and_then(|v| v.checked_add(f.ss))
                .and_then(|v| v.checked_mul(fps))
                .and_then(|v| v.checked_add(f.ff))
                .ok_or_else(overflow)?;
        // Subtract the frames dropped from all elapsed minutes (except tenths).
        let dropped = self.drop_per_min * (total_minutes - total_minutes / 10);
        base.checked_sub(dropped).ok_or_else(overflow)
    }
}

fn overflow() -> ErrorData {
    ErrorData::invalid_params("timecode is too large to convert".to_owned(), None)
}

/// The four numeric fields of a timecode.
struct TimecodeFields {
    hh: u64,
    mm: u64,
    ss: u64,
    ff: u64,
}

impl TimecodeFields {
    /// Parses `HH:MM:SS:FF` accepting `:` or `;` for the final separator.
    fn parse(tc: &str) -> Result<Self, ErrorData> {
        let parts: Vec<&str> = tc.split([':', ';']).collect();
        if parts.len() != 4 {
            return Err(ErrorData::invalid_params(
                "timecode must have four fields: HH:MM:SS:FF".to_owned(),
                None,
            ));
        }
        let mut nums = [0u64; 4];
        for (slot, part) in nums.iter_mut().zip(parts) {
            *slot = part.parse::<u64>().map_err(|_| {
                ErrorData::invalid_params(format!("invalid timecode field {part:?}"), None)
            })?;
        }
        Ok(Self {
            hh: nums[0],
            mm: nums[1],
            ss: nums[2],
            ff: nums[3],
        })
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for PremiereServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Premiere Pro helper tools: SMPTE timecode conversion with correct \
                 drop-frame (29.97/59.94) handling, timecode offset, sequence duration, \
                 and frame-rate conform. All pure-compute — no host application required.",
            )
    }
}
