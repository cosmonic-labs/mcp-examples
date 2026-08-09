//! Live After Effects control.
//!
//! After Effects has no remote API, so this drives it the only way a sandboxed
//! component can: a ScriptUI panel (`bridge/mcp-bridge-auto.jsx`) runs *inside*
//! After Effects and polls this component every ~2 seconds. A tool call queues
//! one command in the shared store ([`crate::state`]), the panel claims it,
//! executes it against the ExtendScript DOM, and POSTs the result back.
//!
//! Two consequences shape every tool here:
//!
//! - **Every call costs at least one poll cycle.** Building a scene one tool
//!   call at a time is dominated by that latency, which is what [`run_batch`]
//!   exists to avoid — it also wraps the whole batch in a single undo group.
//! - **The panel may not be there.** Not-connected is reported as a tool error
//!   with the specific fix, because the command stays queued and nothing will
//!   happen until a human opens the panel. That is more useful to a caller
//!   than a success that silently did nothing.
//!
//! [`run_batch`]: AfterEffectsServer::run_batch

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router, ErrorData};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::server::AfterEffectsServer;
use crate::state;

/// How long a tool call waits for the panel before telling the caller to poll
/// [`get_results`]. The panel checks in every ~2 seconds.
///
/// [`get_results`]: AfterEffectsServer::get_results
const RESULT_WAIT_MS: u64 = 12_000;

/// Batches and frame renders do real work in After Effects and legitimately
/// outrun the single-command budget. Large batches get slower as a project
/// accumulates expression-driven layers, so this budget is generous.
const SLOW_RESULT_WAIT_MS: u64 = 240_000;

/// Commands that get [`SLOW_RESULT_WAIT_MS`] instead of [`RESULT_WAIT_MS`].
const SLOW_COMMANDS: &[&str] = &["runBatch", "saveFramePng", "saveProject"];

/// A panel that has not polled for this long is treated as gone.
const PANEL_SILENT_MS: u64 = 30_000;

/// A panel that polled within this window counts as connected for
/// [`bridge_status`](AfterEffectsServer::bridge_status).
const PANEL_FRESH_MS: u64 = 10_000;

/// Shown when a panel is polling but we refuse to serve it. Without this the
/// symptom is identical to no panel running, and the obvious fix (open the
/// panel) is the one thing that won't help.
const STALE_PANEL_HINT: &str = "An outdated bridge panel is polling and cannot be served \
     commands. Run ./install-bridge.sh, then close and reopen \
     Window > mcp-bridge-auto.jsx in After Effects.";

/// Every command the bridge panel knows how to execute.
///
/// Each variant has a dedicated tool; this enum exists so [`run_script`] and
/// [`run_batch`] can name commands with the same validation the tools get,
/// and so a new panel script is reachable before it has a tool of its own.
///
/// [`run_script`]: AfterEffectsServer::run_script
/// [`run_batch`]: AfterEffectsServer::run_batch
#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PanelScript {
    GetProjectInfo,
    ListCompositions,
    GetLayerInfo,
    CreateComposition,
    CreateTextLayer,
    CreateShapeLayer,
    CreateSolidLayer,
    SetLayerProperties,
    SetLayerKeyframe,
    SetLayerExpression,
    ApplyEffect,
    ApplyEffectTemplate,
    BridgeTestEffects,
    CreateCamera,
    BatchSetLayerProperties,
    SetCompositionProperties,
    DuplicateLayer,
    DeleteLayer,
    SetLayerMask,
    RunBatch,
    AddImageLayer,
    SaveFramePng,
    SaveProject,
    DeleteComposition,
}

impl PanelScript {
    /// The name the panel's dispatch table switches on. Kept in sync with the
    /// `camelCase` serde renaming above, which is what goes over the wire when
    /// a script is named inside `run_batch`.
    fn as_command(self) -> &'static str {
        match self {
            Self::GetProjectInfo => "getProjectInfo",
            Self::ListCompositions => "listCompositions",
            Self::GetLayerInfo => "getLayerInfo",
            Self::CreateComposition => "createComposition",
            Self::CreateTextLayer => "createTextLayer",
            Self::CreateShapeLayer => "createShapeLayer",
            Self::CreateSolidLayer => "createSolidLayer",
            Self::SetLayerProperties => "setLayerProperties",
            Self::SetLayerKeyframe => "setLayerKeyframe",
            Self::SetLayerExpression => "setLayerExpression",
            Self::ApplyEffect => "applyEffect",
            Self::ApplyEffectTemplate => "applyEffectTemplate",
            Self::BridgeTestEffects => "bridgeTestEffects",
            Self::CreateCamera => "createCamera",
            Self::BatchSetLayerProperties => "batchSetLayerProperties",
            Self::SetCompositionProperties => "setCompositionProperties",
            Self::DuplicateLayer => "duplicateLayer",
            Self::DeleteLayer => "deleteLayer",
            Self::SetLayerMask => "setLayerMask",
            Self::RunBatch => "runBatch",
            Self::AddImageLayer => "addImageLayer",
            Self::SaveFramePng => "saveFramePng",
            Self::SaveProject => "saveProject",
            Self::DeleteComposition => "deleteComposition",
        }
    }
}

// ---------------------------------------------------------------------------
// Parameter types
//
// Field names are camelCase because that is what the ExtendScript panel reads
// off the wire; the structs are serialized straight into the queued command.
// `Option` + `skip_serializing_if` matters: the panel distinguishes "absent"
// (leave alone) from any concrete value, so a `null` is not the same as an
// omitted field.
// ---------------------------------------------------------------------------

/// RGB 0-255, the panel's composition background convention.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct Rgb255 {
    pub r: u16,
    pub g: u16,
    pub b: u16,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunScriptParams {
    /// Panel script to run.
    pub script: PanelScript,
    /// Arguments for the script, shaped as that script's dedicated tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Value>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GetLayerInfoParams {
    /// Composition name. Defaults to the active composition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comp_name: Option<String>,
    /// Include transform values, effects, and text details per layer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<bool>,
    /// Include keyframe times and values for animated properties.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyframes: Option<bool>,
    /// Restrict the listing to these 1-based layer indices.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indices: Option<Vec<i64>>,
    /// Restrict the listing to these layer names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub names: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateCompositionParams {
    /// Composition name.
    pub name: String,
    /// Width in pixels (default 1920).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<i64>,
    /// Height in pixels (default 1080).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<i64>,
    /// Pixel aspect ratio (default 1.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pixel_aspect: Option<f64>,
    /// Duration in seconds (default 10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    /// Frames per second (default 30).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_rate: Option<f64>,
    /// Background color, RGB 0-255.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_color: Option<Rgb255>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetCompositionPropertiesParams {
    /// Composition name. Defaults to the active composition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comp_name: Option<String>,
    /// Duration in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    /// Frames per second.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_rate: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<i64>,
    /// Preview backdrop, RGB 0-255. It never renders into the alpha channel,
    /// so a comp with no full-bleed layer is already transparent when rendered
    /// with alpha — use black for comps meant to be composited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_color: Option<Rgb255>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeleteCompositionParams {
    /// Name of the composition(s) to delete.
    pub comp_name: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateTextLayerParams {
    /// Composition name. Defaults to the active composition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comp_name: Option<String>,
    /// The text content.
    pub text: String,
    /// `[x, y]` position (default `[960, 540]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<Vec<f64>>,
    /// Font size (default 72).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_size: Option<f64>,
    /// `[r, g, b]`, each 0..1 (default white). `hex_to_rgb` converts hex here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<Vec<f64>>,
    /// Font family (default Arial).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_family: Option<String>,
    /// Paragraph alignment: `left`, `center`, or `right`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alignment: Option<TextAlignment>,
    /// Layer start time in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time: Option<f64>,
    /// Layer duration in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TextAlignment {
    Left,
    Center,
    Right,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ShapeType {
    Rectangle,
    Ellipse,
    Polygon,
    Star,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateShapeLayerParams {
    /// Composition name. Defaults to the active composition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comp_name: Option<String>,
    /// Shape to draw.
    pub shape_type: ShapeType,
    /// `[x, y]` position.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<Vec<f64>>,
    /// `[width, height]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<Vec<f64>>,
    /// `[r, g, b]`, each 0..1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill_color: Option<Vec<f64>>,
    /// `[r, g, b]`, each 0..1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stroke_color: Option<Vec<f64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stroke_width: Option<f64>,
    /// Stroke opacity 0-100 (default 100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stroke_opacity: Option<f64>,
    /// `[dashLength, gapLength]` for a dashed or dotted outline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dash: Option<Vec<f64>>,
    /// Corner radius for rectangles, in pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roundness: Option<f64>,
    /// Fill opacity 0-100 (default 100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill_opacity: Option<f64>,
    /// Omit the fill entirely (outline-only shape).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill_none: Option<bool>,
    /// Point count for polygon/star (default 5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub points: Option<i64>,
    /// Layer name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateSolidLayerParams {
    /// Composition name. Defaults to the active composition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comp_name: Option<String>,
    /// `[r, g, b]`, each 0..1. Ignored for adjustment layers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<Vec<f64>>,
    /// Layer name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `[width, height]`. Defaults to the composition size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<Vec<f64>>,
    /// `[x, y]` position (default `[960, 540]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<Vec<f64>>,
    /// Create as an adjustment layer (affects the layers beneath it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_adjustment: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AddImageLayerParams {
    /// Composition name. Defaults to the active composition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comp_name: Option<String>,
    /// Absolute path to the image file. PNG/JPEG/TIFF — After Effects cannot
    /// read WebP.
    pub path: String,
    /// Layer name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `[x, y]` centre position.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<Vec<f64>>,
    /// Target height in composition pixels (keeps aspect).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<f64>,
    /// Target width in composition pixels (keeps aspect).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<f64>,
    /// Explicit `[x, y]` scale percentages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<Vec<f64>>,
    /// Opacity 0-100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetLayerPropertiesParams {
    /// Composition name. Defaults to the active composition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comp_name: Option<String>,
    /// Layer name (alternative to `layerIndex`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer_name: Option<String>,
    /// 1-based layer index.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer_index: Option<i64>,
    /// `[x, y]` position. Refused if the property is animated — see the tool
    /// description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<Vec<f64>>,
    /// `[x, y]` scale percentages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<Vec<f64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotation: Option<f64>,
    /// Opacity 0-100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    /// Layer visibility (the eyeball).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Blending mode, e.g. `normal`, `add`, `screen`, `multiply`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blend_mode: Option<String>,
    /// New text content (text layers only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Font family (text layers only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_family: Option<String>,
    /// Font size (text layers only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_size: Option<f64>,
    /// `[r, g, b]` text fill, each 0..1 (text layers only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill_color: Option<Vec<f64>>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetLayerKeyframeParams {
    /// Composition name. Preferred over `compIndex`: project item indices
    /// shift whenever footage is imported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comp_name: Option<String>,
    /// 1-based index of the composition in the project panel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comp_index: Option<i64>,
    /// 1-based layer index within the composition.
    pub layer_index: i64,
    /// Property to keyframe, e.g. `Position`, `Scale`, `Rotation`, `Opacity`.
    pub property_name: String,
    /// Keyframe time in seconds.
    pub time_in_seconds: f64,
    /// Keyframe value: `[x, y]` for Position, `[w, h]` for Scale, a number for
    /// Rotation and Opacity.
    pub value: Value,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetLayerExpressionParams {
    /// Composition name. Preferred over `compIndex`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comp_name: Option<String>,
    /// 1-based index of the composition in the project panel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comp_index: Option<i64>,
    /// 1-based layer index within the composition.
    pub layer_index: i64,
    /// Property to target, e.g. `Position`, `Opacity`.
    pub property_name: String,
    /// The expression. An empty string removes it. The `expression` tool
    /// generates the common ones.
    pub expression_string: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApplyEffectParams {
    /// Composition name. Preferred over `compIndex`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comp_name: Option<String>,
    /// 1-based index of the composition in the project panel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comp_index: Option<i64>,
    /// 1-based layer index within the composition.
    pub layer_index: i64,
    /// Display name, e.g. `Gaussian Blur`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect_name: Option<String>,
    /// Internal match name, e.g. `ADBE Gaussian Blur 2`. More reliable than
    /// the display name, which is localized. `effect_template` lists them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect_match_name: Option<String>,
    /// Effect parameters, e.g. `{ "Blurriness": 25 }`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect_settings: Option<Value>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApplyEffectTemplateParams {
    /// Composition name. Preferred over `compIndex`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comp_name: Option<String>,
    /// 1-based index of the composition in the project panel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comp_index: Option<i64>,
    /// 1-based layer index within the composition.
    pub layer_index: i64,
    /// Template to apply. `effect_template` shows what each expands to.
    pub template_name: crate::server::EffectTemplate,
    /// Overrides for the template defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_settings: Option<Value>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateCameraParams {
    /// Composition name. Defaults to the active composition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comp_name: Option<String>,
    /// Layer name (default `Camera`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Zoom in pixels (default 1777.78, roughly a 50mm equivalent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zoom: Option<f64>,
    /// `[x, y, z]` camera position.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<Vec<f64>>,
    /// `[x, y, z]` point of interest. Ignored when `oneNode` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub point_of_interest: Option<Vec<f64>>,
    /// Create a one-node camera (free-orienting, no point of interest).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub one_node: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DuplicateLayerParams {
    /// Composition name. Defaults to the active composition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comp_name: Option<String>,
    /// 1-based layer index. Takes precedence over `layerName`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer_index: Option<i64>,
    /// Layer name (alternative to `layerIndex`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer_name: Option<String>,
    /// Name for the copy. Defaults to After Effects' own naming.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_name: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeleteLayerParams {
    /// Composition name. Defaults to the active composition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comp_name: Option<String>,
    /// 1-based layer index. Takes precedence over `layerName`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer_index: Option<i64>,
    /// Layer name (alternative to `layerIndex`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer_name: Option<String>,
}

/// How a mask combines with the ones above it.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum MaskMode {
    None,
    Add,
    Subtract,
    Intersect,
    Lighten,
    Darken,
    Difference,
}

/// Rectangle shorthand for a mask shape, in layer coordinates.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct MaskRect {
    pub top: f64,
    pub left: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetLayerMaskParams {
    /// Composition name. Defaults to the active composition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comp_name: Option<String>,
    /// 1-based layer index. Takes precedence over `layerName`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer_index: Option<i64>,
    /// Layer name (alternative to `layerIndex`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer_name: Option<String>,
    /// 1-based index of an existing mask to modify. Omit to add a new mask.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mask_index: Option<i64>,
    /// Mask outline as `[[x, y], ...]`, at least three points. The shape is
    /// always closed. Supply this or `maskRect`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mask_path: Option<Vec<Vec<f64>>>,
    /// Rectangular shorthand for `maskPath`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mask_rect: Option<MaskRect>,
    /// Combination mode (default `add`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mask_mode: Option<MaskMode>,
    /// `[x, y]` feather in pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mask_feather: Option<Vec<f64>>,
    /// Mask opacity 0-100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mask_opacity: Option<f64>,
    /// Mask expansion in pixels; negative contracts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mask_expansion: Option<f64>,
    /// Rename the mask.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mask_name: Option<String>,
}

/// One layer's worth of edits for
/// [`batch_set_layer_properties`](AfterEffectsServer::batch_set_layer_properties).
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LayerPropertyOperation {
    /// 1-based layer index. Takes precedence over `layerName`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer_index: Option<i64>,
    /// Layer name (alternative to `layerIndex`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer_name: Option<String>,
    /// Toggle the layer's 3D switch. Set this before `rotation` in the same
    /// operation: a 3D layer takes Z Rotation, a 2D layer takes Rotation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub three_d_layer: Option<bool>,
    /// `[x, y]` (or `[x, y, z]` for 3D). **Existing Position keyframes are
    /// deleted** so the static value can be applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<Vec<f64>>,
    /// `[x, y]` scale percentages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<Vec<f64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotation: Option<f64>,
    /// Opacity 0-100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity: Option<f64>,
    /// One of `normal`, `add`, `multiply`, `screen`, `overlay`, `softLight`,
    /// `hardLight`, `darken`, `lighten`, `difference`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blend_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out_point: Option<f64>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BatchSetLayerPropertiesParams {
    /// Composition name. Defaults to the active composition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comp_name: Option<String>,
    /// One entry per layer. A failing entry is reported and the rest continue.
    pub operations: Vec<LayerPropertyOperation>,
}

/// One entry in a [`run_batch`](AfterEffectsServer::run_batch) command list.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BatchCommand {
    /// Panel script to run.
    pub command: PanelScript,
    /// Arguments, shaped as that script's dedicated tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunBatchParams {
    /// Commands to run in order. Keep batches to roughly 100.
    pub commands: Vec<BatchCommand>,
    /// Undo group label shown in After Effects (default `MCP batch`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undo_group: Option<String>,
    /// Keep going after a failing command (default false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continue_on_error: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SaveFramePngParams {
    /// Composition name. Defaults to the active composition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comp_name: Option<String>,
    /// Time in seconds to render (default 0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<f64>,
    /// Absolute output path ending in `.png`.
    pub path: String,
    /// Allow replacing an existing file (default false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overwrite: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SaveProjectParams {
    /// Absolute `.aep` path. Omit to save in place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Allow replacing an existing file (default false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overwrite: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BridgeTestEffectsParams {
    /// 1-based composition index (default 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comp_index: Option<i64>,
    /// 1-based layer index (default 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer_index: Option<i64>,
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

#[tool_router(router = live_router, vis = "pub(crate)")]
impl AfterEffectsServer {
    // --- bridge plumbing ---------------------------------------------------

    /// Reports whether the panel is polling, and what it is doing.
    #[tool(
        description = "Check whether the After Effects bridge panel is connected and polling. \
                       Call this first when a command seems to hang."
    )]
    #[tracing::instrument(name = "tool.bridge_status", skip(self))]
    async fn bridge_status(&self) -> Result<CallToolResult, ErrorData> {
        let age = state::last_poll_age_ms().map_err(store_error)?;
        let connected = age.is_some_and(|age| age < PANEL_FRESH_MS);
        let stale_age = state::last_stale_poll_age_ms().map_err(store_error)?;
        let stale_polling = !connected && stale_age.is_some_and(|age| age < PANEL_FRESH_MS);
        Ok(CallToolResult::structured(json!({
            "panelConnected": connected,
            "lastPollAgeMs": age,
            "stalePanelPolling": stale_polling,
            "lastStalePollAgeMs": stale_age,
            "currentCommand": state::current_command().map_err(store_error)?,
            "hint": if connected {
                "Bridge panel is polling normally."
            } else if stale_polling {
                STALE_PANEL_HINT
            } else {
                "Panel not connected. In After Effects: Window > mcp-bridge-auto.jsx \
                 (run ./install-bridge.sh first if it is not listed)."
            },
        })))
    }

    /// Returns whatever the panel reported last.
    #[tool(
        description = "Fetch the result of the most recently executed After Effects command — \
                       use this after a tool call reports that it timed out waiting."
    )]
    #[tracing::instrument(name = "tool.get_results", skip(self))]
    async fn get_results(&self) -> Result<CallToolResult, ErrorData> {
        match state::latest_result().map_err(store_error)? {
            Some(result) => Ok(CallToolResult::structured(result)),
            None => Ok(CallToolResult::structured(json!({
                "status": "no-results",
                "message": "No results yet. Queue a command first, and make sure the \
                            MCP Bridge Auto panel is open in After Effects.",
            }))),
        }
    }

    /// Setup and reference material for the whole integration.
    #[tool(
        description = "How to set up and use the After Effects bridge, including effect match \
                       names, templates, and the batching guidance that makes scene-building fast."
    )]
    #[tracing::instrument(name = "tool.get_help", skip(self))]
    async fn get_help(&self) -> Result<CallToolResult, ErrorData> {
        Ok(CallToolResult::structured(json!({ "help": HELP_TEXT })))
    }

    /// Escape hatch onto the panel's dispatch table.
    #[tool(
        description = "Run a panel script by name with raw arguments. Every script also has a \
                       dedicated, schema-checked tool — prefer those. This exists for a panel \
                       script that is newer than this component, and for forwarding arguments \
                       built elsewhere."
    )]
    #[tracing::instrument(name = "tool.run_script", skip(self))]
    async fn run_script(
        &self,
        Parameters(params): Parameters<RunScriptParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let args = params.parameters.unwrap_or_else(|| json!({}));
        queue_and_wait(params.script.as_command(), args).await
    }

    // --- reading the project ----------------------------------------------

    #[tool(
        description = "Information about the current After Effects project: item list and active \
                       composition."
    )]
    #[tracing::instrument(name = "tool.get_project_info", skip(self))]
    async fn get_project_info(&self) -> Result<CallToolResult, ErrorData> {
        queue_and_wait("getProjectInfo", json!({})).await
    }

    #[tool(description = "List all compositions in the current After Effects project.")]
    #[tracing::instrument(name = "tool.list_compositions", skip(self))]
    async fn list_compositions(&self) -> Result<CallToolResult, ErrorData> {
        queue_and_wait("listCompositions", json!({})).await
    }

    #[tool(
        description = "List the layers of a composition. Ask for `detail` to get transform values \
                       and effects, `keyframes` for animation data, and narrow with `indices` or \
                       `names` — a full detailed listing of a large comp is a lot of output."
    )]
    #[tracing::instrument(name = "tool.get_layer_info", skip(self))]
    async fn get_layer_info(
        &self,
        Parameters(params): Parameters<GetLayerInfoParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("getLayerInfo", &params).await
    }

    // --- creating -----------------------------------------------------------

    #[tool(description = "Create a new composition.")]
    #[tracing::instrument(name = "tool.create_composition", skip(self))]
    async fn create_composition(
        &self,
        Parameters(params): Parameters<CreateCompositionParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("createComposition", &params).await
    }

    #[tool(description = "Create a text layer in a composition.")]
    #[tracing::instrument(name = "tool.create_text_layer", skip(self))]
    async fn create_text_layer(
        &self,
        Parameters(params): Parameters<CreateTextLayerParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("createTextLayer", &params).await
    }

    #[tool(description = "Create a shape layer: rectangle, ellipse, polygon, or star.")]
    #[tracing::instrument(name = "tool.create_shape_layer", skip(self))]
    async fn create_shape_layer(
        &self,
        Parameters(params): Parameters<CreateShapeLayerParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("createShapeLayer", &params).await
    }

    #[tool(
        description = "Create a solid or adjustment layer. Adjustment layers apply their effects \
                       to everything beneath them in the stack."
    )]
    #[tracing::instrument(name = "tool.create_solid_layer", skip(self))]
    async fn create_solid_layer(
        &self,
        Parameters(params): Parameters<CreateSolidLayerParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("createSolidLayer", &params).await
    }

    #[tool(
        description = "Import an image file (PNG/JPEG/TIFF — not WebP, which After Effects cannot \
                       read) and add it as a layer. Size it with an explicit scale or by target \
                       height/width in composition pixels. Re-imports of the same path are reused."
    )]
    #[tracing::instrument(name = "tool.add_image_layer", skip(self))]
    async fn add_image_layer(
        &self,
        Parameters(params): Parameters<AddImageLayerParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("addImageLayer", &params).await
    }

    #[tool(
        description = "Add a camera layer. Two-node cameras aim at a point of interest; one-node \
                       cameras orient freely. Only 3D layers are affected by a camera."
    )]
    #[tracing::instrument(name = "tool.create_camera", skip(self))]
    async fn create_camera(
        &self,
        Parameters(params): Parameters<CreateCameraParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("createCamera", &params).await
    }

    // --- modifying ----------------------------------------------------------

    #[tool(
        description = "Set transform, timing, visibility, blend mode, and text properties on one \
                       layer. A transform that is already animated is NOT overwritten — the \
                       result lists it under `skippedProperties` rather than silently deleting \
                       the animation. Use set_layer_keyframe to animate instead."
    )]
    #[tracing::instrument(name = "tool.set_layer_properties", skip(self))]
    async fn set_layer_properties(
        &self,
        Parameters(params): Parameters<SetLayerPropertiesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("setLayerProperties", &params).await
    }

    #[tool(
        description = "Apply transform, timing, 3D, and blend-mode changes to many layers of one \
                       composition in a single round trip. Unlike set_layer_properties this \
                       REPLACES existing Position keyframes with the static value."
    )]
    #[tracing::instrument(name = "tool.batch_set_layer_properties", skip(self))]
    async fn batch_set_layer_properties(
        &self,
        Parameters(params): Parameters<BatchSetLayerPropertiesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("batchSetLayerProperties", &params).await
    }

    #[tool(description = "Set a keyframe on a layer property at a given time.")]
    #[tracing::instrument(name = "tool.set_layer_keyframe", skip(self))]
    async fn set_layer_keyframe(
        &self,
        Parameters(params): Parameters<SetLayerKeyframeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("setLayerKeyframe", &params).await
    }

    #[tool(
        description = "Set or remove an expression on a layer property. An empty string removes \
                       it. The `expression` tool generates wiggle/loopOut/bounce source."
    )]
    #[tracing::instrument(name = "tool.set_layer_expression", skip(self))]
    async fn set_layer_expression(
        &self,
        Parameters(params): Parameters<SetLayerExpressionParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("setLayerExpression", &params).await
    }

    #[tool(
        description = "Create or modify a mask on a layer. Give a rectangle with `maskRect` or an \
                       arbitrary closed outline with `maskPath` (three points minimum); pass \
                       `maskIndex` to edit an existing mask instead of adding one."
    )]
    #[tracing::instrument(name = "tool.set_layer_mask", skip(self))]
    async fn set_layer_mask(
        &self,
        Parameters(params): Parameters<SetLayerMaskParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("setLayerMask", &params).await
    }

    #[tool(description = "Apply an effect to a layer, by display name or (preferably) match name.")]
    #[tracing::instrument(name = "tool.apply_effect", skip(self))]
    async fn apply_effect(
        &self,
        Parameters(params): Parameters<ApplyEffectParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("applyEffect", &params).await
    }

    #[tool(
        description = "Apply a predefined effect template (gaussian-blur, glow, drop-shadow, \
                       cinematic-look, text-pop, …) to a layer."
    )]
    #[tracing::instrument(name = "tool.apply_effect_template", skip(self))]
    async fn apply_effect_template(
        &self,
        Parameters(params): Parameters<ApplyEffectTemplateParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("applyEffectTemplate", &params).await
    }

    #[tool(
        description = "Change a composition's duration, frame rate, dimensions, or background \
                       colour. The background colour is a preview backdrop only — it never \
                       renders into the alpha channel."
    )]
    #[tracing::instrument(name = "tool.set_composition_properties", skip(self))]
    async fn set_composition_properties(
        &self,
        Parameters(params): Parameters<SetCompositionPropertiesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("setCompositionProperties", &params).await
    }

    #[tool(description = "Duplicate a layer, optionally renaming the copy.")]
    #[tracing::instrument(name = "tool.duplicate_layer", skip(self))]
    async fn duplicate_layer(
        &self,
        Parameters(params): Parameters<DuplicateLayerParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("duplicateLayer", &params).await
    }

    #[tool(
        description = "Delete a layer. Layer indices of the layers below it shift up by one \
                       afterwards, so re-read get_layer_info before deleting again by index."
    )]
    #[tracing::instrument(name = "tool.delete_layer", skip(self))]
    async fn delete_layer(
        &self,
        Parameters(params): Parameters<DeleteLayerParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("deleteLayer", &params).await
    }

    #[tool(
        description = "Delete every composition with the given name — useful to rebuild a scene \
                       from scratch."
    )]
    #[tracing::instrument(name = "tool.delete_composition", skip(self))]
    async fn delete_composition(
        &self,
        Parameters(params): Parameters<DeleteCompositionParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("deleteComposition", &params).await
    }

    // --- batching and output ------------------------------------------------

    #[tool(
        description = "Run many commands in one round trip, inside a single undo group. Strongly \
                       preferred when building a scene: every other tool costs a ~2s poll cycle, \
                       so 200 individual calls take minutes while one batch takes seconds. Keep \
                       batches to roughly 100 commands. Stops at the first error unless \
                       continueOnError is set."
    )]
    #[tracing::instrument(name = "tool.run_batch", skip(self))]
    async fn run_batch(
        &self,
        Parameters(params): Parameters<RunBatchParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("runBatch", &params).await
    }

    #[tool(
        description = "Render a single frame of a composition to a PNG file — the way to visually \
                       verify work. Refuses to overwrite unless `overwrite` is set."
    )]
    #[tracing::instrument(name = "tool.save_frame_png", skip(self))]
    async fn save_frame_png(
        &self,
        Parameters(params): Parameters<SaveFramePngParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("saveFramePng", &params).await
    }

    #[tool(description = "Save the After Effects project. With no path, saves in place.")]
    #[tracing::instrument(name = "tool.save_project", skip(self))]
    async fn save_project(
        &self,
        Parameters(params): Parameters<SaveProjectParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("saveProject", &params).await
    }

    #[tool(
        description = "Smoke-test the bridge by applying a blur and a drop shadow to one layer. \
                       Diagnostic only — use apply_effect for real work."
    )]
    #[tracing::instrument(name = "tool.bridge_test_effects", skip(self))]
    async fn bridge_test_effects(
        &self,
        Parameters(params): Parameters<BridgeTestEffectsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("bridgeTestEffects", &params).await
    }
}

/// Serializes `params` into the panel's argument shape and dispatches it.
async fn dispatch<P: Serialize>(command: &str, params: &P) -> Result<CallToolResult, ErrorData> {
    let args = serde_json::to_value(params).map_err(|err| {
        ErrorData::internal_error(format!("failed to encode arguments: {err}"), None)
    })?;
    queue_and_wait(command, args).await
}

/// Queues one command and waits for the panel to report back.
///
/// The three non-result outcomes are deliberately distinct, because they have
/// different fixes: no panel has ever connected (install and open it), a panel
/// went quiet (open it again), and a stale panel is polling but cannot be
/// served (reinstall — reopening will not help).
async fn queue_and_wait(command: &str, args: Value) -> Result<CallToolResult, ErrorData> {
    // A command already in flight explains the silence: the panel cannot poll
    // while it is executing, and a long batch blocks it for many seconds. Only
    // treat a quiet panel as "gone" when nothing is running.
    let busy = state::current_command()
        .map_err(store_error)?
        .and_then(|command| {
            command
                .get("status")
                .and_then(Value::as_str)
                .map(|status| status == "dispatched")
        })
        .unwrap_or(false);

    // A stale panel polling right now explains the silence better than
    // anything else, and needs different advice.
    let stale_polling = state::last_stale_poll_age_ms()
        .map_err(store_error)?
        .is_some_and(|age| age < PANEL_SILENT_MS);

    let poll_age = state::last_poll_age_ms().map_err(store_error)?;
    let unreachable = match poll_age {
        None => Some(if stale_polling {
            STALE_PANEL_HINT.to_owned()
        } else {
            "The bridge panel has never connected. Run ./install-bridge.sh, then open \
             Window > mcp-bridge-auto.jsx in After Effects."
                .to_owned()
        }),
        Some(age) if age > PANEL_SILENT_MS && !busy => Some(if stale_polling {
            STALE_PANEL_HINT.to_owned()
        } else {
            format!(
                "The bridge panel has not polled for {}s. Open \
                 Window > mcp-bridge-auto.jsx in After Effects.",
                age / 1000
            )
        }),
        Some(_) => None,
    };

    // Queue regardless: the command runs as soon as a panel shows up, so the
    // caller's work is not lost while they go and open it.
    let id = state::queue_command(command, &args).map_err(store_error)?;

    if let Some(hint) = unreachable {
        tracing::warn!(command, id, "queued command with no panel listening");
        // A tool error, not a success: nothing happened in After Effects, and
        // nothing will until a human acts.
        return Ok(CallToolResult::structured_error(json!({
            "status": "queued-not-executed",
            "command": command,
            "commandId": id,
            "message": hint,
            "nextStep": "Once the panel is open, call get_results to fetch the outcome.",
        })));
    }

    let wait_ms = if SLOW_COMMANDS.contains(&command) {
        SLOW_RESULT_WAIT_MS
    } else {
        RESULT_WAIT_MS
    };
    match state::wait_for_result(id, wait_ms)
        .await
        .map_err(store_error)?
    {
        Some(result) => {
            let failed = result.get("status").and_then(Value::as_str) == Some("error")
                || result.get("success").and_then(Value::as_bool) == Some(false);
            let result = match result {
                Value::Object(_) => result,
                other => json!({ "result": other }),
            };
            if failed {
                Ok(CallToolResult::structured_error(result))
            } else {
                Ok(CallToolResult::structured(result))
            }
        }
        None => {
            tracing::warn!(command, id, wait_ms, "timed out waiting for panel result");
            Ok(CallToolResult::structured(json!({
                "status": "pending",
                "command": command,
                "commandId": id,
                "message": format!(
                    "No result within {}s. The panel may still be working on it.",
                    wait_ms / 1000
                ),
                "nextStep": "Call bridge_status to check the connection, then get_results \
                             to fetch the outcome once it lands.",
            })))
        }
    }
}

/// A keyvalue failure is an infrastructure problem, not a bad tool argument,
/// so it becomes a protocol-level error rather than a tool result.
fn store_error(message: String) -> ErrorData {
    tracing::error!(error = %message, "bridge state store failure");
    ErrorData::internal_error(message, None)
}

const HELP_TEXT: &str = r#"# After Effects MCP bridge

## Setup (one-time)
1. Run `./install-bridge.sh` from the project directory (copies
   mcp-bridge-auto.jsx into After Effects' ScriptUI Panels folder).
2. In After Effects: Settings > Scripting & Expressions > enable
   "Allow Scripts to Write Files and Access Network", then restart.
3. Open the panel: Window > mcp-bridge-auto.jsx. Leave it open; it polls this
   server every ~2 seconds.

## How it works
A tool call queues one command; the panel claims it on its next poll, runs it
in After Effects, and posts the result back. Most tools wait up to ~12s and
return the result directly (batches and renders wait up to 4 minutes). If a
call times out, `bridge_status` checks the panel and `get_results` fetches the
outcome.

## Building scenes quickly
Every tool call costs at least one ~2s poll cycle. Use `run_batch` to send a
whole scene in one round trip, inside a single undo group — it is the
difference between minutes and seconds.

## Common effect match names (for apply_effect)
- Gaussian Blur: "ADBE Gaussian Blur 2"
- Directional Blur: "ADBE Directional Blur"
- Brightness & Contrast: "ADBE Brightness & Contrast 2"
- Color Balance: "ADBE Color Balance (HLS)"
- Curves: "ADBE CurvesCustom"
- Hue/Saturation: "ADBE HUE SATURATION"
- Levels: "ADBE Pro Levels2"
- Glow: "ADBE Glow"
- Drop Shadow: "ADBE Drop Shadow"
- Fractal Noise: "ADBE Fractal Noise"

## Effect templates (for apply_effect_template)
gaussian-blur, directional-blur, color-balance, brightness-contrast, curves,
glow, drop-shadow, cinematic-look, text-pop

## Helper tools that need no panel
frames_to_timecode, timecode_to_frames, interpolate, expression,
effect_template, hex_to_rgb — pure arithmetic and code generation, useful for
working out keyframe values and colours before sending them to After Effects.
"#;
