//! TypeScript-specific per-symbol metadata.
//!
//! `TsSymbolData` carries data that the TypeScript extractor populates
//! on each `Symbol` during extraction. The diff engine never reads this
//! data — it's consumed only by TS-specific analysis (hierarchy inference,
//! SD pipeline, report building).

use serde::{Deserialize, Serialize};

/// Per-symbol metadata for TypeScript/React components.
///
/// This is the concrete type for `Language::SymbolData`.
/// Carried on `Symbol<TsSymbolData>` throughout the TS pipeline.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TsSymbolData {
    /// Components from the same package that this component renders internally
    /// in its JSX return tree. Determined by parsing the `.tsx` source file.
    ///
    /// Used for hierarchy inference: components in the same family that do NOT
    /// appear in this list are likely consumer-provided children.
    ///
    /// Only populated for Function/Variable/Constant symbols that represent
    /// React components with JSX render functions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rendered_components: Vec<String>,

    /// CSS class tokens used by this component (e.g., `["inputGroup", "inputGroupItem"]`).
    /// Extracted from `styles.xxx` references in component source files.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub css: Vec<String>,
}
