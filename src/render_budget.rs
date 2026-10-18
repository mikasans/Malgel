//! Dynamic render-budget gate for heavy Markdown / Typst previews.
//!
//! When an estimate of pages, pixmap memory, or source size exceeds a soft
//! budget, the UI shows an inline banner and does **not** auto-render until
//! the user confirms. Light documents stay on the automatic path.

/// Soft budgets — exceed any one and the preview waits for confirmation.
pub const BUDGET_SOURCE_BYTES: usize = 256 * 1024;
pub const BUDGET_PAGES: usize = 10;
/// Estimated RGBA pixmap MiB across all live sheets (A4 @ 2× ≈ 7.7 MiB each).
pub const BUDGET_PIXMAP_MIB: f64 = 40.0;
pub const BUDGET_EMBEDDED_IMAGE_BYTES: u64 = 8 * 1024 * 1024;

/// A cheap, conservative cost estimate — never a full compile.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderEstimate {
    pub source_bytes: usize,
    pub pages: usize,
    pub pixmap_mib: f64,
    pub image_bytes: u64,
    pub reasons: Vec<&'static str>,
}

impl RenderEstimate {
    pub fn exceeds_budget(&self) -> bool {
        !self.reasons.is_empty()
    }

    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if self.pages > 0 {
            parts.push(format!(
                "~{} page{}",
                self.pages,
                if self.pages == 1 { "" } else { "s" }
            ));
        }
        if self.pixmap_mib >= 1.0 {
            parts.push(format!("~{:.0} MiB rasters", self.pixmap_mib));
        }
        if self.source_bytes >= 1024 {
            parts.push(format!(
                "{:.1} KB source",
                self.source_bytes as f64 / 1024.0
            ));
        }
        if self.image_bytes >= 1024 * 1024 {
            parts.push(format!(
                "{:.1} MiB images",
                self.image_bytes as f64 / (1024.0 * 1024.0)
            ));
        }
        if parts.is_empty() {
            "heavy preview".into()
        } else {
            parts.join(", ")
        }
    }
}

/// Whether the user has cleared the gate for this document tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RenderConsent {
    /// Under budget, or user clicked Render — auto-update freely.
    #[default]
    Allowed,
    /// Over budget; waiting for an inline-banner confirm.
    Pending,
    /// User chose not to render; stay light until they reverse it.
    Skipped,
}

/// Estimate Typst live-preview cost from source text alone (no compile).
///
/// Page count: `#pagebreak` markers plus a loose length heuristic so a
/// long single-page stream still trips the budget. Pixmap estimate assumes
/// A4 @ `SHEET_SCALE` 2× (~7.7 MiB) per page — posters are capped elsewhere
/// but still count as one heavy sheet.
pub fn estimate_typst(source: &str, embedded_image_bytes: u64) -> RenderEstimate {
    let source_bytes = source.len();
    let pagebreaks = source.matches("#pagebreak").count();
    // ~2.5 KB of source ≈ one dense page of body text once laid out.
    let length_pages = (source_bytes / 2500).max(1);
    let pages = (pagebreaks + 1).max(length_pages);
    let pixmap_mib = pages as f64 * 7.7;
    let mut reasons = Vec::new();
    if source_bytes > BUDGET_SOURCE_BYTES {
        reasons.push("large source");
    }
    if pages > BUDGET_PAGES {
        reasons.push("many pages");
    }
    if pixmap_mib > BUDGET_PIXMAP_MIB {
        reasons.push("heavy rasters");
    }
    if embedded_image_bytes > BUDGET_EMBEDDED_IMAGE_BYTES {
        reasons.push("large images");
    }
    RenderEstimate {
        source_bytes,
        pages,
        pixmap_mib,
        image_bytes: embedded_image_bytes,
        reasons,
    }
}

/// Estimate Markdown preview cost (source size + fenced Mermaid count).
pub fn estimate_markdown(source: &str) -> RenderEstimate {
    let source_bytes = source.len();
    let mermaid_blocks = source
        .split("```")
        .skip(1)
        .step_by(2)
        .filter(|block| {
            let lang = block
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            lang == "mermaid" || lang.starts_with("mermaid")
        })
        .count();
    // Treat each Mermaid fence as ~one expensive raster sheet.
    let pages = mermaid_blocks;
    let pixmap_mib = mermaid_blocks as f64 * 4.0;
    let mut reasons = Vec::new();
    if source_bytes > BUDGET_SOURCE_BYTES {
        reasons.push("large source");
    }
    if mermaid_blocks > 6 {
        reasons.push("many diagrams");
    }
    if pixmap_mib > BUDGET_PIXMAP_MIB {
        reasons.push("heavy diagrams");
    }
    RenderEstimate {
        source_bytes,
        pages,
        pixmap_mib,
        image_bytes: 0,
        reasons,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_typst_is_under_budget() {
        let e = estimate_typst("= Hi\n\nHello.\n", 0);
        assert!(!e.exceeds_budget(), "{e:?}");
    }

    #[test]
    fn many_pagebreaks_trip_budget() {
        let mut src = String::from("= Doc\n");
        for i in 0..15 {
            src.push_str(&format!("\n#pagebreak()\n== S{i}\n"));
        }
        let e = estimate_typst(&src, 0);
        assert!(e.exceeds_budget(), "{e:?}");
        assert!(e.pages > BUDGET_PAGES);
    }

    #[test]
    fn large_images_trip_budget() {
        let e = estimate_typst("= Hi\n", BUDGET_EMBEDDED_IMAGE_BYTES + 1);
        assert!(e.exceeds_budget());
        assert!(e.reasons.contains(&"large images"));
    }

    #[test]
    fn many_mermaid_trip_markdown_budget() {
        let mut src = String::new();
        for i in 0..8 {
            src.push_str(&format!("```mermaid\ngraph TD; A{i}-->B{i}\n```\n\n"));
        }
        let e = estimate_markdown(&src);
        assert!(e.exceeds_budget(), "{e:?}");
    }
}
