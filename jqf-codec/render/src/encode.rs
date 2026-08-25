//! The render encoder factory and session: one frame per input item.
//!
//! The eight base renderers share one factory. The request's DIALECT selects the renderer; the OPTIONS carry the rest
//! of the composition law (layout, width, header, terminal shape, the shell flattening separator). Each item is
//! rendered to one complete frame whose interior LFs are codec-owned; the facade appends the single final LF after
//! every frame and no BOM. A located item is materialized to an owned value at the semantic boundary before rendering.

use alloc::string::String;
use alloc::vec::Vec;

use jqf_codec_core::{
    ByteSink, CodecError, CodecFailureKind, CodecRunContext, EncodeItem, EncodeRequest, EncoderFactoryImpl,
    EncoderSession, ErasedEncoderFactory, ErasedEncoderSession, PhysicalRouteId, PreservationOutcome,
    PreservationReport, PreservationRequest, RecycledSessionState,
};
use jqf_data::{DataError, MaterializeWorkspace};
use jqf_resource::{ResourceContext, WorkAdmission};

use crate::options::RenderEncodeOptions;
use crate::{hist, plain, shell, spans, table, tree};

/// The stable identity of the render encoder factory.
pub const ENCODE_PHYSICAL_ROUTE_ID: PhysicalRouteId = match PhysicalRouteId::derive(crate::options::FORMAT_ID, 3, 1) {
    Some(id) => id,
    None => panic!("nonzero route identity"),
};

const OFFER_BYTES: usize = 16 * 1024;

/// One of the eight base renderers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Renderer {
    /// `render.plain@1`.
    Plain,
    /// `render.gfm-table@1`.
    GfmTable,
    /// `render.html-table@1`.
    HtmlTable,
    /// `render.grid-table@1`.
    GridTable,
    /// `render.tree@1`.
    Tree,
    /// `render.terminal@1`.
    Terminal,
    /// `render.shell@1`.
    Shell,
    /// `render.hist@1`.
    Hist,
}

impl Renderer {
    /// Maps an output dialect identity to its renderer.
    fn from_dialect(dialect: &str) -> Option<Self> {
        match dialect {
            crate::PLAIN_DIALECT_ID => Some(Self::Plain),
            crate::GFM_TABLE_DIALECT_ID => Some(Self::GfmTable),
            crate::HTML_TABLE_DIALECT_ID => Some(Self::HtmlTable),
            crate::GRID_TABLE_DIALECT_ID => Some(Self::GridTable),
            crate::TREE_DIALECT_ID => Some(Self::Tree),
            crate::TERMINAL_DIALECT_ID => Some(Self::Terminal),
            crate::SHELL_DIALECT_ID => Some(Self::Shell),
            crate::HIST_DIALECT_ID => Some(Self::Hist),
            _ => None,
        }
    }
}

/// Creates the render encoder factory for one normalized renderer + options.
pub(crate) fn create_factory(
    request: EncodeRequest<'_, '_>,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedEncoderFactory, CodecError> {
    if request.format.as_str() != crate::FORMAT_ID {
        return Err(CodecError::new(CodecFailureKind::RequirementMismatch));
    }
    let renderer = Renderer::from_dialect(request.dialect.as_str())
        .ok_or(CodecError::new(CodecFailureKind::RequirementMismatch))?;
    let options = match request.options {
        None => RenderEncodeOptions::default(),
        Some(payload) => *payload
            .downcast_ref::<RenderEncodeOptions>()
            .ok_or(CodecError::new(CodecFailureKind::RequirementMismatch))?,
    };
    ErasedEncoderFactory::try_new_factory(request.preservation, resources, move || {
        Ok(RenderEncoderFactory { renderer, options })
    })
}

struct RenderEncoderFactory {
    renderer: Renderer,
    options: RenderEncodeOptions,
}

impl EncoderFactoryImpl for RenderEncoderFactory {
    fn physical_encoder(&self) -> PhysicalRouteId {
        ENCODE_PHYSICAL_ROUTE_ID
    }

    fn start<'item, 'source>(
        &self,
        item: EncodeItem<'item, 'source>,
        preservation: PreservationRequest,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<ErasedEncoderSession<'item, 'source>, CodecError> {
        let renderer = self.renderer;
        let options = self.options;
        ErasedEncoderSession::try_new(item, preservation, move || {
            Ok(RenderEncoder {
                renderer,
                options,
                bytes: Vec::new(),
                state: EncodeState::Active,
                root_done: false,
                workspace: MaterializeWorkspace::new(),
            })
        })
    }

    fn try_restart(
        &self,
        state: &mut RecycledSessionState<'_>,
        _item: EncodeItem<'_, '_>,
        _preservation: PreservationRequest,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        let Some(encoder) = state.downcast_mut::<RenderEncoder>() else {
            return Ok(false);
        };
        encoder.reset();
        Ok(true)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EncodeState {
    Active,
    InputFinished,
}

struct RenderEncoder {
    renderer: Renderer,
    options: RenderEncodeOptions,
    bytes: Vec<u8>,
    state: EncodeState,
    root_done: bool,
    /// Document-independent materialization scratch, reused across every item of this (possibly recycled) session so
    /// the document-sized cycle bitmap is allocated once, not once per item. The workspace is uncharged scratch that
    /// jqf-data leaves all-clear after every materialization, success and error alike, so `reset` need not touch it.
    workspace: MaterializeWorkspace,
}

impl RenderEncoder {
    /// Reinitializes one recycled encoder for one more ordered item: drops every byte and flag a previous item may have
    /// left behind — including one that aborted mid-offer, whose partial staging must never reach the next item —
    /// leaving exactly the state a fresh [`EncoderFactoryImpl::start`] would have produced.
    fn reset(&mut self) {
        self.bytes.clear();
        self.state = EncodeState::Active;
        self.root_done = false;
    }

    /// Renders one item to its complete frame and stages it.
    fn encode_item(&mut self, item: EncodeItem<'_, '_>, resources: &mut ResourceContext<'_>) -> Result<(), CodecError> {
        let frame = match item {
            EncodeItem::Owned(value) => self.render_value(value, resources)?,
            EncodeItem::Located { product, node } => {
                let owned = product
                    .document()
                    .materialize_node_with(&mut self.workspace, node, resources)
                    .map_err(map_data)?;
                self.render_value(&owned, resources)?
            }
        };
        self.bytes.extend_from_slice(frame.as_bytes());
        Ok(())
    }

    /// Renders one owned value to its frame text.
    fn render_value(&self, value: &jqf_data::Value, resources: &ResourceContext<'_>) -> Result<String, CodecError> {
        match self.renderer {
            Renderer::Plain => plain::render(value, resources),
            Renderer::GfmTable => table::render(value, table::TableRenderer::Gfm, self.options, resources),
            Renderer::HtmlTable => table::render(value, table::TableRenderer::Html, self.options, resources),
            Renderer::GridTable => table::render(value, table::TableRenderer::Grid, self.options, resources),
            Renderer::Tree => tree::render(value),
            Renderer::Terminal => spans::render(value, self.options, resources),
            Renderer::Shell => shell::render(value, self.options),
            Renderer::Hist => hist::render(value),
        }
    }
}

impl EncoderSession for RenderEncoder {
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
    fn encode(
        &mut self,
        item: EncodeItem<'_, '_>,
        sink: &mut dyn ByteSink,
        context: &mut CodecRunContext<'_, '_>,
    ) -> Result<PreservationReport, CodecError> {
        loop {
            if self.state == EncodeState::InputFinished {
                if !self.bytes.is_empty() {
                    sink.write_all(&self.bytes, context.resources())?;
                    self.bytes.clear();
                }
                return Ok(report());
            }
            if self.root_done {
                self.state = EncodeState::InputFinished;
                continue;
            }
            if self.bytes.len() >= OFFER_BYTES {
                sink.write_all(&self.bytes, context.resources())?;
                self.bytes.clear();
                continue;
            }
            let remaining = context.resources().remaining_work() as usize;
            match context.resources().admit_work_transitions(remaining.max(1))? {
                WorkAdmission::Pending => context.replenish_work()?,
                WorkAdmission::Granted(granted) => {
                    for _ in 0..granted {
                        if self.root_done || self.bytes.len() >= OFFER_BYTES {
                            break;
                        }
                        self.encode_item(item, context.resources())?;
                        self.root_done = true;
                    }
                }
            }
        }
    }
}

/// The renderer's preservation evidence: the value is PRESENTED as text, never preserved as data; ordering is retained;
/// no source is reused.
const fn report() -> PreservationReport {
    PreservationReport::new(
        PreservationOutcome::Omitted,
        PreservationOutcome::Omitted,
        PreservationOutcome::Exact,
        PreservationOutcome::Omitted,
    )
}

fn map_data(error: DataError) -> CodecError {
    jqf_codec_core::map_data(error, "render encoder document materialization")
}
