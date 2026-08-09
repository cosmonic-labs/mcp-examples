//! After Effects MCP server — pure-compute helpers for motion-graphics work.
//!
//! These tools do the fiddly arithmetic and code generation an AE artist
//! reaches for constantly: frame/timecode conversion, keyframe easing values,
//! expression snippets, and color conversion into AE's 0..1 float space. No
//! host application is involved — they answer from a request alone, which is
//! why they stay useful even when the bridge panel is closed.
//!
//! The tools that drive a *live* After Effects instance live in
//! [`crate::live`]; both routers are merged in [`AfterEffectsServer::new`].

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// After Effects MCP server. Stateless — one instance per request.
#[derive(Clone)]
pub struct AfterEffectsServer {
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FramesToTimecodeParams {
    /// Frame number (0-based), must be >= 0.
    pub frames: u64,
    /// Frames per second, e.g. 24, 25, 30, 60. Must be > 0.
    pub fps: f64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TimecodeToFramesParams {
    /// Timecode `HH:MM:SS:FF` (non-drop; `:` or `;` separators accepted).
    pub timecode: String,
    /// Frames per second the timecode is expressed in. Must be > 0.
    pub fps: f64,
}

/// Easing curve for [`AfterEffectsServer::interpolate`].
///
/// The cubic and back variants match the constants used in the reference
/// After Effects animation toolkit (easeOutCubic, easeInOutCubic, and the
/// standard 1.70158 easeOutBack overshoot).
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Easing {
    /// Constant velocity.
    Linear,
    /// Slow start (quadratic).
    EaseIn,
    /// Slow end (quadratic).
    EaseOut,
    /// Slow start and end (cubic smoothstep) — AE's "Easy Ease".
    EaseInOut,
    /// Slow end (cubic): `1 - (1 - t)^3`.
    EaseOutCubic,
    /// Slow start and end (cubic): `t < .5 ? 4t^3 : 1 - (-2t+2)^3/2`.
    EaseInOutCubic,
    /// Overshoot-and-settle (easeOutBack, overshoot constant 1.70158).
    EaseOutBack,
}

/// A predefined After Effects effect template. Names and defaults mirror the
/// reference toolkit; `match_name` is the internal ADBE identifier (more
/// reliable to apply than the display name).
///
/// `Serialize` matters: [`crate::live`] sends these names straight to the
/// panel's `applyEffectTemplate`, which expects the same kebab-case spelling.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum EffectTemplate {
    GaussianBlur,
    DirectionalBlur,
    ColorBalance,
    BrightnessContrast,
    Curves,
    Glow,
    DropShadow,
    CinematicLook,
    TextPop,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EffectTemplateParams {
    /// Which template to expand.
    pub template: EffectTemplate,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InterpolateParams {
    /// Value at the first keyframe.
    pub from: f64,
    /// Value at the second keyframe.
    pub to: f64,
    /// Normalized time between the keyframes, clamped to `0.0..=1.0`.
    pub t: f64,
    /// Easing curve to apply.
    pub easing: Easing,
}

/// Which AE expression to generate.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExpressionKind {
    /// `wiggle(freq, amp)` random motion.
    Wiggle,
    /// `loopOut("cycle")` — repeat keyframes after the last one.
    LoopOut,
    /// `time * rate` — constant drive (e.g. continuous rotation).
    TimeDrive,
    /// Bounce settle after the last keyframe.
    Bounce,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExpressionParams {
    /// Which expression to generate.
    pub kind: ExpressionKind,
    /// For `wiggle`: frequency in Hz. For `time_drive`: units per second.
    /// Ignored otherwise.
    #[serde(default)]
    pub rate: Option<f64>,
    /// For `wiggle`: amplitude in layer units. Ignored otherwise.
    #[serde(default)]
    pub amplitude: Option<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct HexToRgbParams {
    /// Hex color, `#RGB`, `#RRGGBB`, or `#RRGGBBAA` (leading `#` optional).
    pub hex: String,
}

#[tool_router]
impl AfterEffectsServer {
    pub fn new() -> Self {
        Self {
            // Pure-compute helpers plus the live-control tools from
            // `crate::live`, which share this struct's router.
            tool_router: Self::tool_router() + Self::live_router(),
        }
    }

    /// Converts an absolute frame number to non-drop `HH:MM:SS:FF` timecode.
    #[tool(
        description = "Convert a frame number to HH:MM:SS:FF timecode (non-drop) at a given fps"
    )]
    #[tracing::instrument(name = "tool.frames_to_timecode", skip(self))]
    async fn frames_to_timecode(
        &self,
        Parameters(params): Parameters<FramesToTimecodeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        // Whole frames per second for the FF field (29.97 -> 30, 59.94 -> 60).
        let (fps, rate) = validate_fps(params.fps)?;
        let total = params.frames;
        let ff = total % rate;
        let total_seconds = total / rate;
        let ss = total_seconds % 60;
        let mm = (total_seconds / 60) % 60;
        let hh = total_seconds / 3600;
        let timecode = format!("{hh:02}:{mm:02}:{ss:02}:{ff:02}");
        Ok(CallToolResult::structured(serde_json::json!({
            "timecode": timecode,
            "frames": total,
            "fps": fps,
        })))
    }

    /// Converts a non-drop `HH:MM:SS:FF` timecode to an absolute frame number.
    #[tool(
        description = "Convert HH:MM:SS:FF timecode (non-drop) to a frame number at a given fps"
    )]
    #[tracing::instrument(name = "tool.timecode_to_frames", skip(self))]
    async fn timecode_to_frames(
        &self,
        Parameters(params): Parameters<TimecodeToFramesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let (fps, rate) = validate_fps(params.fps)?;
        let (hh, mm, ss, ff) = parse_timecode(&params.timecode)?;
        if ff >= rate {
            return Err(ErrorData::invalid_params(
                format!("frame field {ff} is out of range for {rate} fps"),
                None,
            ));
        }
        if ss >= 60 || mm >= 60 {
            return Err(ErrorData::invalid_params(
                "minutes and seconds must be < 60".to_owned(),
                None,
            ));
        }
        // Checked arithmetic: a huge HH field would otherwise wrap silently
        // (release builds) and return a wrong frame count.
        let frames = hh
            .checked_mul(3600)
            .and_then(|v| v.checked_add(mm * 60 + ss))
            .and_then(|v| v.checked_mul(rate))
            .and_then(|v| v.checked_add(ff))
            .ok_or_else(|| {
                ErrorData::invalid_params("timecode is too large to convert".to_owned(), None)
            })?;
        Ok(CallToolResult::structured(serde_json::json!({
            "frames": frames,
            "timecode": params.timecode,
            "fps": fps,
        })))
    }

    /// Evaluates an eased interpolation between two keyframe values.
    #[tool(
        description = "Interpolate between two keyframe values with an easing curve at time t (0..1)"
    )]
    #[tracing::instrument(name = "tool.interpolate", skip(self))]
    async fn interpolate(
        &self,
        Parameters(params): Parameters<InterpolateParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if !params.t.is_finite() || !params.from.is_finite() || !params.to.is_finite() {
            return Err(ErrorData::invalid_params(
                "from, to, and t must be finite numbers".to_owned(),
                None,
            ));
        }
        let t = params.t.clamp(0.0, 1.0);
        let eased = match params.easing {
            Easing::Linear => t,
            Easing::EaseIn => t * t,
            Easing::EaseOut => 1.0 - (1.0 - t) * (1.0 - t),
            Easing::EaseInOut => t * t * (3.0 - 2.0 * t),
            Easing::EaseOutCubic => 1.0 - (1.0 - t).powi(3),
            Easing::EaseInOutCubic => {
                if t < 0.5 {
                    4.0 * t * t * t
                } else {
                    1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
                }
            }
            Easing::EaseOutBack => {
                let c1 = 1.70158;
                let c3 = c1 + 1.0;
                1.0 + c3 * (t - 1.0).powi(3) + c1 * (t - 1.0).powi(2)
            }
        };
        let value = params.from + (params.to - params.from) * eased;
        if !value.is_finite() {
            // e.g. from=-1e308, to=1e308 overflows to ±inf (which serde would
            // emit as JSON null). Report it instead of returning a null value.
            return Err(ErrorData::invalid_params(
                "interpolated value overflowed to a non-finite number".to_owned(),
                None,
            ));
        }
        Ok(CallToolResult::structured(serde_json::json!({
            "value": value,
            "eased_t": eased,
        })))
    }

    /// Generates a ready-to-paste After Effects expression.
    #[tool(
        description = "Generate a common After Effects expression (wiggle, loopOut, time drive, bounce)"
    )]
    #[tracing::instrument(name = "tool.expression", skip(self))]
    async fn expression(
        &self,
        Parameters(params): Parameters<ExpressionParams>,
    ) -> Result<CallToolResult, ErrorData> {
        // Non-finite numbers would format as `NaN`/`inf` — invalid AE
        // expression text — so fall back to the default for those.
        let finite_or = |v: Option<f64>, default: f64| match v {
            Some(x) if x.is_finite() => x,
            _ => default,
        };
        let code = match params.kind {
            ExpressionKind::Wiggle => {
                let freq = finite_or(params.rate, 2.0);
                let amp = finite_or(params.amplitude, 30.0);
                format!("wiggle({freq}, {amp});")
            }
            ExpressionKind::LoopOut => "loopOut(\"cycle\");".to_owned(),
            ExpressionKind::TimeDrive => {
                let rate = finite_or(params.rate, 360.0);
                format!("time * {rate};")
            }
            ExpressionKind::Bounce => "// Bounce settle after last keyframe\n\
                 amp = 0.1; freq = 2.0; decay = 5.0;\n\
                 n = numKeys > 0 ? nearestKey(time).index : 0;\n\
                 if (n > 0 && key(n).time > time) n--;\n\
                 if (n == 0) { value; } else {\n\
                 t = time - key(n).time;\n\
                 v = velocityAtTime(key(n).time - 0.001);\n\
                 value + v * amp * Math.sin(freq * t * 2 * Math.PI) / Math.exp(decay * t);\n\
                 }"
            .to_owned(),
        };
        Ok(CallToolResult::structured(serde_json::json!({
            "expression": code,
        })))
    }

    /// Expands a predefined effect template to its ADBE match name and default
    /// parameters — paste-ready for `applyEffect` in a live AE bridge, or as a
    /// reference for building effects by hand.
    #[tool(
        description = "Expand a predefined effect template (gaussian-blur, glow, drop-shadow, \
                          cinematic-look, text-pop, …) to its ADBE match name(s) and defaults"
    )]
    #[tracing::instrument(name = "tool.effect_template", skip(self))]
    async fn effect_template(
        &self,
        Parameters(params): Parameters<EffectTemplateParams>,
    ) -> Result<CallToolResult, ErrorData> {
        // Match names and defaults are the reference toolkit's, verbatim.
        let effects = match params.template {
            EffectTemplate::GaussianBlur => serde_json::json!([
                {"match_name": "ADBE Gaussian Blur 2", "settings": {"Blurriness": 20}}
            ]),
            EffectTemplate::DirectionalBlur => serde_json::json!([
                {"match_name": "ADBE Directional Blur", "settings": {"Direction": 0, "Blur Length": 10}}
            ]),
            EffectTemplate::ColorBalance => serde_json::json!([
                {"match_name": "ADBE Color Balance (HLS)", "settings": {"Hue": 0, "Lightness": 0, "Saturation": 0}}
            ]),
            EffectTemplate::BrightnessContrast => serde_json::json!([
                {"match_name": "ADBE Brightness & Contrast 2", "settings": {"Brightness": 0, "Contrast": 0, "Use Legacy": false}}
            ]),
            EffectTemplate::Curves => serde_json::json!([
                {"match_name": "ADBE CurvesCustom", "settings": {}}
            ]),
            EffectTemplate::Glow => serde_json::json!([
                {"match_name": "ADBE Glow", "settings": {"Glow Threshold": 50, "Glow Radius": 15, "Glow Intensity": 1}}
            ]),
            EffectTemplate::DropShadow => serde_json::json!([
                {"match_name": "ADBE Drop Shadow", "settings": {"Shadow Color": [0,0,0,1], "Opacity": 50, "Direction": 135, "Distance": 10, "Softness": 10}}
            ]),
            EffectTemplate::CinematicLook => serde_json::json!([
                {"match_name": "ADBE CurvesCustom", "settings": {}},
                {"match_name": "ADBE Vibrance", "settings": {"Vibrance": 15, "Saturation": -5}}
            ]),
            EffectTemplate::TextPop => serde_json::json!([
                {"match_name": "ADBE Drop Shadow", "settings": {"Shadow Color": [0,0,0,1], "Opacity": 75, "Distance": 5, "Softness": 10}},
                {"match_name": "ADBE Glow", "settings": {"Glow Threshold": 50, "Glow Radius": 10, "Glow Intensity": 1.5}}
            ]),
        };
        Ok(CallToolResult::structured(
            serde_json::json!({ "effects": effects }),
        ))
    }

    /// Converts a hex color to After Effects' 0..1 RGBA float array plus 0..255.
    #[tool(
        description = "Convert a hex color (#RGB/#RRGGBB/#RRGGBBAA) to AE 0..1 RGBA and 0..255 values"
    )]
    #[tracing::instrument(name = "tool.hex_to_rgb", skip(self))]
    async fn hex_to_rgb(
        &self,
        Parameters(params): Parameters<HexToRgbParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let (r, g, b, a) = parse_hex(&params.hex)?;
        let norm = |v: u8| (v as f64) / 255.0;
        Ok(CallToolResult::structured(serde_json::json!({
            "rgba_255": [r, g, b, a],
            "rgba_float": [norm(r), norm(g), norm(b), norm(a)],
            "ae_array": format!("[{}, {}, {}, {}]", norm(r), norm(g), norm(b), norm(a)),
        })))
    }
}

/// Validates a frame rate and returns `(fps, integer_rate)` where
/// `integer_rate = fps.round()` and is guaranteed `>= 1`.
///
/// The integer rate is the divisor for the frame (`FF`) field; a fractional
/// fps below `0.5` would round to zero and divide-by-zero (a trap on this
/// target), so it is rejected here rather than downstream.
fn validate_fps(fps: f64) -> Result<(f64, u64), ErrorData> {
    if !fps.is_finite() || fps <= 0.0 {
        return Err(ErrorData::invalid_params(
            "fps must be a positive, finite number".to_owned(),
            None,
        ));
    }
    let rate = fps.round();
    if rate < 1.0 {
        return Err(ErrorData::invalid_params(
            "fps must round to at least 1 frame per second".to_owned(),
            None,
        ));
    }
    Ok((fps, rate as u64))
}

/// Parses `HH:MM:SS:FF` (accepting `:` or `;` as the frame separator) into
/// its four numeric fields.
fn parse_timecode(tc: &str) -> Result<(u64, u64, u64, u64), ErrorData> {
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
    Ok((nums[0], nums[1], nums[2], nums[3]))
}

/// Parses `#RGB`, `#RRGGBB`, or `#RRGGBBAA` (leading `#` optional) into RGBA
/// byte components. Alpha defaults to 255 when absent.
fn parse_hex(hex: &str) -> Result<(u8, u8, u8, u8), ErrorData> {
    let s = hex.trim().strip_prefix('#').unwrap_or(hex.trim());
    if !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(ErrorData::invalid_params(
            "hex must contain only 0-9a-f digits".to_owned(),
            None,
        ));
    }
    let byte = |slice: &str| u8::from_str_radix(slice, 16).unwrap_or(0);
    match s.len() {
        3 => {
            // #RGB shorthand: each nibble doubled.
            let expand = |c: char| {
                let v = c.to_digit(16).unwrap_or(0) as u8;
                v * 16 + v
            };
            let chars: Vec<char> = s.chars().collect();
            Ok((expand(chars[0]), expand(chars[1]), expand(chars[2]), 255))
        }
        6 => Ok((byte(&s[0..2]), byte(&s[2..4]), byte(&s[4..6]), 255)),
        8 => Ok((
            byte(&s[0..2]),
            byte(&s[2..4]),
            byte(&s[4..6]),
            byte(&s[6..8]),
        )),
        _ => Err(ErrorData::invalid_params(
            "hex must be 3, 6, or 8 digits".to_owned(),
            None,
        )),
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for AfterEffectsServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Controls a live Adobe After Effects instance through the MCP Bridge \
                 Auto panel (Window > mcp-bridge-auto.jsx), which polls this server \
                 every ~2 seconds. Check `bridge_status` first if commands seem to \
                 hang, and `get_results` to fetch the outcome of one that timed out. \
                 Every call costs a poll cycle, so build scenes with `run_batch` \
                 rather than one tool call per layer, and verify with \
                 `save_frame_png`. `get_help` has the setup steps and effect \
                 match names.\n\n\
                 The helper tools — frames_to_timecode, timecode_to_frames, \
                 interpolate, expression, effect_template, hex_to_rgb — are pure \
                 arithmetic and code generation. They need no panel, and are the \
                 right way to work out keyframe values, easing, and colours before \
                 sending them to After Effects.",
            )
    }
}
