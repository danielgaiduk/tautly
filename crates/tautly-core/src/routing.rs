//! Side-model choice (spec 7.5): the model for compaction summaries, memory work and
//! `web_fetch` questions.
//!
//! Seam for Task 27: this is the selection that exists before side-model routing lands.
//! Task 27 owns this file and replaces [`side_model_id`] with the full rule (the catalog's
//! `tier = "small"` model of the main provider, else the main model) under the same
//! signature, plus the `SideRunner` that sends side requests to the side model's own
//! provider. The engine calls it at session start and on every `SetModel`, with
//! `cfg.model` set to the new main model, so a switch recomputes the side model unless
//! `routing.side_model` is set.

use tautly_provider::{Catalog, ModelId};

use crate::config::Config;
use crate::provider_setup::provider_label;

/// `routing.side_model` when set, else the main model (`cfg.model`).
pub fn side_model_id(cfg: &Config, _catalog: &Catalog) -> ModelId {
    cfg.routing
        .side_model
        .clone()
        .unwrap_or_else(|| cfg.model.clone())
}

/// The model a compaction summary request names. Summaries still go through the main
/// model's provider (until Task 27's `SideRunner`), so the side model is used only when
/// that provider serves it, else the main model.
pub(crate) fn summary_model(main: &ModelId, side: &ModelId) -> ModelId {
    if provider_label(side) == provider_label(main) {
        side.clone()
    } else {
        main.clone()
    }
}
