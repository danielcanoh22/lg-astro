//! Reads TrueType metrics and individual glyph outlines on demand.
//!
//! Glyphs are outlined and tessellated one at a time as text needs them; the font is never
//! preprocessed as a whole, which keeps CJK coverage cheap until it is used.
use astro_vector_geometry::{self as geometry, Vertex};
use geometry::lyon_tessellation::{
    FillRule,
    math::point,
    path::{Path, path::Builder},
};
use owned_ttf_parser::{AsFaceRef, GlyphId, OutlineBuilder, OwnedFace};

/// A parsed TrueType face; metrics are scaled to the requested size in pixels.
pub struct Font {
    face: OwnedFace,
}

/// Vertical metrics in pixels at a size; `descent` is negative below the baseline.
pub struct LineMetrics {
    pub ascent: f32,
    pub descent: f32,
}

/// Horizontal metrics of one character in pixels at a size.
pub struct Metrics {
    pub advance_width: f32,
}

impl Font {
    /// Parses the first face of a font file.
    ///
    /// # Errors
    ///
    /// Fails when the data is not a valid TrueType or OpenType font.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, String> {
        OwnedFace::from_vec(bytes, 0)
            .map(|face| Self { face })
            .map_err(|e| e.to_string())
    }

    /// Returns the glyph for a character, or 0 (`.notdef`) when the font lacks it.
    pub fn lookup_glyph_index(&self, character: char) -> u16 {
        self.face
            .as_face_ref()
            .glyph_index(character)
            .map_or(0, |id| id.0)
    }

    /// Returns the ascent and descent at `size` pixels per em; always `Some`.
    pub fn horizontal_line_metrics(&self, size: f32) -> Option<LineMetrics> {
        let face = self.face.as_face_ref();
        let scale = size / f32::from(face.units_per_em());
        Some(LineMetrics {
            ascent: f32::from(face.ascender()) * scale,
            descent: f32::from(face.descender()) * scale,
        })
    }

    /// Returns the advance width of a character at `size` pixels per em.
    pub fn metrics(&self, character: char, size: f32) -> Metrics {
        let face = self.face.as_face_ref();
        let glyph = GlyphId(self.lookup_glyph_index(character));
        Metrics {
            advance_width: f32::from(face.glyph_hor_advance(glyph).unwrap_or(0)) * size
                / f32::from(face.units_per_em()),
        }
    }

    /// Returns the pair adjustment at `size` pixels per em from the legacy `kern` table, if any.
    pub fn horizontal_kern(&self, left: char, right: char, size: f32) -> Option<f32> {
        let face = self.face.as_face_ref();
        let left = face.glyph_index(left)?;
        let right = face.glyph_index(right)?;
        for table in face.tables().kern?.subtables {
            if table.horizontal
                && !table.has_cross_stream
                && let Some(value) = table.glyphs_kerning(left, right)
            {
                return Some(f32::from(value) * size / f32::from(face.units_per_em()));
            }
        }
        None
    }

    /// Tessellates a character's outline in ems with y pointing down, origin on the baseline.
    ///
    /// Characters without an outline, such as spaces, return no vertices.
    ///
    /// # Errors
    ///
    /// Fails when [`geometry::tessellate`] rejects the outline.
    pub fn geometry(&self, character: char) -> Result<Vec<Vertex>, String> {
        let face = self.face.as_face_ref();
        let mut outline = GlyphOutline {
            builder: Path::builder(),
            scale: 1. / f32::from(face.units_per_em()),
            open: false,
        };
        let glyph = GlyphId(self.lookup_glyph_index(character));
        if face.outline_glyph(glyph, &mut outline).is_none() {
            return Ok(Vec::new());
        }
        if outline.open {
            outline.builder.end(true);
        }
        // Flatten at subpixel precision for glyphs up to 256 physical px/em,
        // covering the current 1080p and 4K typography with the same geometry.
        geometry::tessellate(&outline.builder.build(), 0.15 / 256., FillRule::NonZero)
    }
}

/// Converts font-unit outline callbacks into a lyon path in ems, flipping y to point down.
struct GlyphOutline {
    builder: Builder,
    scale: f32,
    open: bool,
}

impl OutlineBuilder for GlyphOutline {
    fn move_to(&mut self, x: f32, y: f32) {
        if self.open {
            self.builder.end(true);
        }
        self.builder.begin(point(x * self.scale, -y * self.scale));
        self.open = true;
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.builder.line_to(point(x * self.scale, -y * self.scale));
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.builder.quadratic_bezier_to(
            point(x1 * self.scale, -y1 * self.scale),
            point(x * self.scale, -y * self.scale),
        );
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.builder.cubic_bezier_to(
            point(x1 * self.scale, -y1 * self.scale),
            point(x2 * self.scale, -y2 * self.scale),
            point(x * self.scale, -y * self.scale),
        );
    }
    fn close(&mut self) {
        if self.open {
            self.builder.end(true);
            self.open = false;
        }
    }
}
