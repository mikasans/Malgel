//! Dynamic render-budget gate for heavy Markdown / Typst previews.
//!
//! When the estimated raster / embedded-image footprint reaches **150 MiB**,
//! the UI shows an inline banner and does **not** auto-render until the user
//! confirms. Smaller documents stay on the automatic path (no nag).

/// Soft budget — only prompt when estimated usage is at least this many MiB.
pub const BUDGET_PIXMAP_MIB: f64 = 150.0;

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
/// Page count: `#pagebreak` markers plus a loose length heuristic. Pixmap
/// estimate assumes A4 @ `SHEET_SCALE` 2× (~7.7 MiB) per page, plus embedded
/// image bytes. The gate trips only when that total is ≥ `BUDGET_PIXMAP_MIB`.
pub fn estimate_typst(source: &str, embedded_image_bytes: u64) -> RenderEstimate {
    let source_bytes = source.len();
    let pagebreaks = source.matches("#pagebreak").count();
    // ~2.5 KB of source ≈ one dense page of body text once laid out.
    let length_pages = (source_bytes / 2500).max(1);
    let pages = (pagebreaks + 1).max(length_pages);
    let pixmap_mib = pages as f64 * 7.7 + (embedded_image_bytes as f64 / (1024.0 * 1024.0));
    let mut reasons = Vec::new();
    if pixmap_mib >= BUDGET_PIXMAP_MIB {
        reasons.push("heavy rasters");
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
    if pixmap_mib >= BUDGET_PIXMAP_MIB {
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
    fn many_pages_alone_do_not_nag_under_150_mib() {
        // 15 pages ≈ 115.5 MiB < 150 — must not prompt.
        let mut src = String::from("= Doc\n");
        for i in 0..15 {
            src.push_str(&format!("\n#pagebreak()\n== S{i}\n"));
        }
        let e = estimate_typst(&src, 0);
        assert!(
            !e.exceeds_budget(),
            "15 pages should be under 150 MiB: {e:?}"
        );
        assert!(e.pixmap_mib < BUDGET_PIXMAP_MIB);
    }

    #[test]
    fn heavy_raster_estimate_trips_at_150_mib() {
        // ~20 pages ≈ 154 MiB.
        let mut src = String::from("= Doc\n");
        for i in 0..20 {
            src.push_str(&format!("\n#pagebreak()\n== S{i}\n"));
        }
        let e = estimate_typst(&src, 0);
        assert!(e.exceeds_budget(), "{e:?}");
        assert!(e.pixmap_mib >= BUDGET_PIXMAP_MIB);
        assert!(e.reasons.contains(&"heavy rasters"));
    }

    #[test]
    fn large_embedded_images_count_toward_mib() {
        // 1 page (~7.7) + 150 MiB images ≥ 150.
        let e = estimate_typst("= Hi\n", 150 * 1024 * 1024);
        assert!(e.exceeds_budget());
        assert!(e.pixmap_mib >= BUDGET_PIXMAP_MIB);
    }

    #[test]
    fn few_mermaid_do_not_nag() {
        let mut src = String::new();
        for i in 0..8 {
            src.push_str(&format!("```mermaid\ngraph TD; A{i}-->B{i}\n```\n\n"));
        }
        let e = estimate_markdown(&src);
        assert!(!e.exceeds_budget(), "8 mermaid ≈ 32 MiB: {e:?}");
    }

    #[test]
    fn many_mermaid_trip_at_150_mib() {
        let mut src = String::new();
        for i in 0..40 {
            src.push_str(&format!("```mermaid\ngraph TD; A{i}-->B{i}\n```\n\n"));
        }
        let e = estimate_markdown(&src);
        assert!(e.exceeds_budget(), "{e:?}");
    }

    #[test]
    fn mermaid_budget_boundary_is_inclusive_at_150() {
        // 37 × 4.0 = 148 < 150; 38 × 4.0 = 152 ≥ 150.
        let mut under = String::new();
        for i in 0..37 {
            under.push_str(&format!("```mermaid\ngraph TD; A{i}-->B{i}\n```\n\n"));
        }
        assert!(!estimate_markdown(&under).exceeds_budget());

        let mut over = String::new();
        for i in 0..38 {
            over.push_str(&format!("```mermaid\ngraph TD; A{i}-->B{i}\n```\n\n"));
        }
        let e = estimate_markdown(&over);
        assert!(e.exceeds_budget(), "{e:?}");
        assert!(e.pixmap_mib >= BUDGET_PIXMAP_MIB);
    }

    #[test]
    fn typst_budget_boundary_around_nineteen_pages() {
        // 19 × 7.7 = 146.3 < 150; 20 × 7.7 = 154 ≥ 150.
        let mut under = String::from("= Doc\n");
        for i in 0..18 {
            under.push_str(&format!("\n#pagebreak()\n== S{i}\n"));
        }
        let e = estimate_typst(&under, 0);
        assert!(!e.exceeds_budget(), "{e:?}");
        assert!(e.pixmap_mib < BUDGET_PIXMAP_MIB);

        let mut over = String::from("= Doc\n");
        for i in 0..19 {
            over.push_str(&format!("\n#pagebreak()\n== S{i}\n"));
        }
        let e = estimate_typst(&over, 0);
        assert!(e.exceeds_budget(), "{e:?}");
    }
}
