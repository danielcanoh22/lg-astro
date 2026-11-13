//! Draws scenes of [`Draw`] commands with a small GLES2-compatible GPU painter.
//!
//! Rust owns the vector glyphs and the draw list; SDL only supplies the GL context, and no
//! browser, LVGL or Qt runtime is involved. Scenes are laid out in logical 1920×1080 units and
//! scaled to the drawable through its [`Viewport`]. Three programs draw everything: the paint
//! shader (rounded rectangles, shadows, raster images and the folded wallpaper background), and
//! the vector and text shaders for `.lvec` meshes and cached text runs. Images decode on the
//! [`image_loader`] worker; only their GPU uploads happen here.
//!
//! The renderer is created, used and dropped on the SDL thread while its GL context is current.
//! Every `unsafe` GL call in this module relies on that invariant.
use crate::{
    font::Font,
    image_loader::{self, Decoded},
    raster::{Size, Viewport},
    state::Rect,
    vector,
};
use gl::types::*;
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    ffi::{CStr, CString},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

/// Straight (not premultiplied) RGBA, each channel from 0 to 1.
pub type Color = [f32; 4];
/// The interface's off-white foreground colour.
pub const WHITE: Color = [0.96, 0.96, 0.96, 1.];
/// CPU bytes of cached glyph outlines (and at most 4096 glyphs) before the least recently drawn
/// are evicted.
const GLYPH_GEOMETRY_BUDGET: usize = 8 * 1024 * 1024;
/// GPU bytes of cached text-run meshes (and at most 512 runs) before the least recently drawn
/// are evicted.
const TEXT_GEOMETRY_BUDGET: usize = 32 * 1024 * 1024;
/// GPU bytes of image textures before those not drawn this frame are evicted, oldest first.
const TEXTURE_BUDGET: usize = 48 * 1024 * 1024;
/// GPU uploads per event-loop turn stop after about one 1080p RGBA wallpaper.
const UPLOAD_BUDGET: usize = 1920 * 1080 * 4;
/// Failed image paths are remembered so they are not decoded every frame, up to this many.
const FAILED_IMAGE_LIMIT: usize = 256;

/// The font and line layout of a text command.
#[derive(Clone, Copy, Debug)]
pub struct TextStyle {
    /// Font size in logical pixels per em.
    pub size: f32,
    /// Font weight, which selects the font file (see `load_font`).
    pub weight: u16,
    /// CSS letter-spacing in em, not pixels.
    pub tracking: f32,
    /// Line height as a multiple of `size`.
    pub line_height: f32,
    pub align_right: bool,
}
impl TextStyle {
    /// Returns a regular-weight, left-aligned style with a 1.2 line height.
    pub const fn new(size: f32) -> Self {
        Self {
            size,
            weight: 400,
            tracking: 0.,
            line_height: 1.2,
            align_right: false,
        }
    }
    pub const fn weight(mut self, weight: u16) -> Self {
        self.weight = weight;
        self
    }
    pub const fn tracking(mut self, tracking: f32) -> Self {
        self.tracking = tracking;
        self
    }
    /// Sets the line height as a multiple of the size.
    pub const fn leading(mut self, height: f32) -> Self {
        self.line_height = height;
        self
    }
    pub const fn right(mut self) -> Self {
        self.align_right = true;
        self
    }
}

/// One scene command, in logical 1920×1080 coordinates; a scene draws its commands in order.
///
/// [`crate::damage`] compares consecutive scenes command by command, so a command's fields must
/// describe everything it paints.
#[derive(Clone)]
pub enum Draw {
    /// A rounded rectangle: rect, corner radius, border width (0 fills it) and colour.
    Shape(Rect, f32, f32, Color),
    /// A rounded rectangle with a vertical gradient: rect, corner radius, top and bottom colours.
    RoundedGradient(Rect, f32, Color, Color),
    /// A soft rounded-rectangle shadow: rect, corner radius, blur distance and colour.
    Shadow(Rect, f32, f32, Color),
    /// Wrapped text: content, layout box, style, colour and whether lines are centred.
    Text(String, Rect, TextStyle, Color, bool),
    /// A raster image or `.lvec` mesh file: path, rect, corner radius and whether it is fitted
    /// inside the rect (`true`) instead of stretched to it.
    Image(String, Rect, f32, bool),
    /// A [`Draw::Image`] multiplied by a tint colour.
    TintedImage(String, Rect, f32, bool, Color),
    /// Installed icon and its backdrop, composited before opacity and edge fading.
    AppImage(Arc<crate::app_artwork::Artwork>, Rect, f32, Color),
    /// Restricts later commands to a rect, or lifts the restriction.
    Clip(Option<Rect>),
    /// Fades later commands vertically: [top fade-in start, end, bottom fade-out start, end] in
    /// logical y; `None` turns it off.
    EdgeFade(Option<[f32; 4]>),
    /// Horizontal counterpart: [left fade-in start, end, right fade-out start, end].
    EdgeFadeX(Option<[f32; 4]>),
}

/// An uploaded image and its cache bookkeeping.
struct Texture {
    id: GLuint,
    width: u32,
    height: u32,
    bytes: usize,
    /// The last frame that drew it, for eviction.
    frame: u64,
    /// Uploaded without an alpha channel.
    opaque: bool,
}
impl Drop for Texture {
    fn drop(&mut self) {
        // SAFETY: textures are dropped on the SDL thread with the context current, and the
        // pointer names this texture's one name.
        unsafe {
            gl::DeleteTextures(1, &self.id);
        }
    }
}

/// A glyph outline tessellated in ems, shared by every text run that uses it.
struct Glyph {
    geometry: Vec<astro_vector_geometry::Vertex>,
    frame: u64,
}

/// Identifies a laid-out text run; equal keys share one cached mesh.
#[derive(Clone, PartialEq, Eq, Hash)]
struct TextKey {
    text: String,
    weight: u16,
    // Logical size, tracking, line height, available width and height.
    layout: [u32; 5],
    /// Bits: right-aligned, centred, multisampled (which drops the edge fringe).
    flags: u8,
}

/// A laid-out text run's mesh, relative to its layout box origin, with the glyph counts reported
/// each time it is drawn.
struct TextRun {
    mesh: Option<vector::Mesh>,
    missing_glyphs: usize,
    failures: usize,
    frame: u64,
}

/// GL driver details reported in diagnostics.
#[derive(Clone, Serialize)]
pub struct GraphicsInfo {
    pub vendor: String,
    pub renderer: String,
    pub version: String,
    pub max_texture_size: u32,
    pub max_viewport: [i32; 2],
    /// Whether fragment shaders get `highp` floats; GLES2 only guarantees `mediump`.
    pub fragment_highp: bool,
}

/// Statistics of the last rendered frame and the cache sizes, reported in diagnostics.
#[derive(Clone, Serialize)]
pub struct RenderStats {
    pub target: Size,
    pub viewport: Viewport,
    pub font_antialiasing: &'static str,
    pub multisample_samples: u32,
    pub max_logical_font_px: f32,
    pub max_physical_font_px: f32,
    pub cached_glyphs: usize,
    pub glyph_tessellations: usize,
    /// Milliseconds this frame spent tessellating new glyph outlines (first-use work).
    pub glyph_ms: f64,
    /// Milliseconds this frame spent loading new logo meshes (first-use work).
    pub vector_load_ms: f64,
    /// GPU uploads of decoded images since the previous frame, plus any image decoded on
    /// the SDL thread (synchronous mode only).
    pub image_load_ms: f64,
    pub glyph_failures: usize,
    pub missing_glyphs: usize,
    pub missing_images: usize,
    /// Images still decoding on the worker, not drawn this frame.
    pub pending_images: usize,
    pub glyph_geometry_bytes: usize,
    pub cached_text_runs: usize,
    pub text_geometry_bytes: usize,
    pub text_triangles: usize,
    pub artwork_bytes: usize,
    pub vector_meshes: usize,
    pub vector_bytes: usize,
    pub vector_triangles: usize,
    pub high_resolution_wallpaper: bool,
    pub draw_calls: usize,
    pub cpu_submission_ms: f64,
    /// Share of the drawable redrawn in the last frame (1 without partial redraw).
    pub redrawn_fraction: f64,
}
impl RenderStats {
    fn new(target: Size) -> Self {
        Self {
            target,
            viewport: Viewport::new(target),
            font_antialiasing: "vector grayscale coverage",
            multisample_samples: 0,
            max_logical_font_px: 0.,
            max_physical_font_px: 0.,
            cached_glyphs: 0,
            glyph_tessellations: 0,
            glyph_ms: 0.,
            vector_load_ms: 0.,
            image_load_ms: 0.,
            glyph_failures: 0,
            missing_glyphs: 0,
            missing_images: 0,
            pending_images: 0,
            glyph_geometry_bytes: 0,
            cached_text_runs: 0,
            text_geometry_bytes: 0,
            text_triangles: 0,
            artwork_bytes: 0,
            vector_meshes: 0,
            vector_bytes: 0,
            vector_triangles: 0,
            high_resolution_wallpaper: false,
            draw_calls: 0,
            cpu_submission_ms: 0.,
            redrawn_fraction: 1.,
        }
    }
}

/// What one draw call draws.
enum BatchKind {
    /// Paint-shader vertices `start..start + count`, all sampling one texture.
    Paint {
        texture: GLuint,
        start: usize,
        count: usize,
        /// An app icon's backdrop gradient, top and bottom RGB.
        backdrop: Option<[[u8; 3]; 2]>,
        /// The outgoing wallpaper of a cross-fade, sampled from texture unit 1.
        outgoing: Option<GLuint>,
    },
    /// A cached `.lvec` mesh stretched to `rect`.
    Vector {
        path: String,
        rect: Rect,
        tint: Color,
    },
    /// A cached text run placed at `origin`.
    Text {
        key: TextKey,
        origin: [f32; 2],
        tint: Color,
    },
}

/// A draw call with the clip and edge fade in effect when it was recorded.
struct Batch {
    kind: BatchKind,
    clip: Option<Rect>,
    // [top fade-in, top end, bottom start, bottom fade-out, left ×2, right ×2]; 0 = off.
    fade: Option<[f32; 8]>,
}

/// Returns whether `rect` covers the whole logical screen.
fn full_screen(rect: &Rect) -> bool {
    rect.x <= 0. && rect.y <= 0. && rect.x + rect.w >= 1920. && rect.y + rect.h >= 1080.
}

/// An opaque, untinted, square full-screen image: the wallpaper that scrims can fold into.
fn plain_wallpaper(command: &Draw) -> Option<&str> {
    match command {
        Draw::Image(path, rect, radius, false) if full_screen(rect) && *radius <= 0. => Some(path),
        Draw::TintedImage(path, rect, radius, false, tint)
            if full_screen(rect) && *radius <= 0. && *tint == [1.; 4] =>
        {
            Some(path)
        }
        _ => None,
    }
}

/// The incoming wallpaper of a cross-fade: full-screen and square, tinted only in alpha.
fn incoming_wallpaper(command: &Draw) -> Option<(&str, f32)> {
    match command {
        Draw::TintedImage(path, rect, radius, false, tint)
            if full_screen(rect) && *radius <= 0. && tint[..3] == [1.; 3] =>
        {
            Some((path, tint[3]))
        }
        _ => None,
    }
}

/// Full-screen layers the renderer draws in one pass: a wallpaper, the wallpaper fading in over
/// it during a cross-fade, and the full-screen fills directly above them.
#[derive(Debug, PartialEq)]
pub(crate) struct Background<'a> {
    pub wallpaper: &'a str,
    /// The incoming wallpaper and its opacity.
    pub incoming: Option<(&'a str, f32)>,
    pub scrims: Vec<Scrim>,
    /// Commands these layers span, the wallpaper's included.
    pub len: usize,
}

/// The background layers starting at `commands[0]`, unless it is a lone wallpaper (or none).
pub(crate) fn background(commands: &[Draw]) -> Option<Background<'_>> {
    let wallpaper = commands.first().and_then(plain_wallpaper)?;
    let incoming = commands
        .get(1)
        .filter(|command| !profile_skip(command))
        .and_then(incoming_wallpaper);
    let mut len = 1 + usize::from(incoming.is_some());
    let mut scrims = Vec::new();
    while let Some(next) = commands.get(len) {
        if profile_skip(next) {
            len += 1;
        } else if let Some(scrim) = Scrim::of(next) {
            scrims.push(scrim);
            len += 1;
        } else {
            break;
        }
    }
    (incoming.is_some() || !scrims.is_empty()).then_some(Background {
        wallpaper,
        incoming,
        scrims,
        len,
    })
}

/// A full-screen translucent fill without corners or border: a flat colour or a vertical
/// gradient (straight alpha, interpolated like the paint shader's vertex colours).
#[derive(Debug, PartialEq)]
pub(crate) enum Scrim {
    Flat(Color),
    Gradient(Color, Color),
}
impl Scrim {
    /// Recognises a full-screen fill that can fold into the wallpaper.
    fn of(command: &Draw) -> Option<Self> {
        match command {
            Draw::Shape(rect, radius, border, color)
                if full_screen(rect) && *radius <= 0. && *border <= 0. =>
            {
                Some(Self::Flat(*color))
            }
            Draw::RoundedGradient(rect, radius, top, bottom)
                if full_screen(rect) && *radius <= 0. =>
            {
                Some(Self::Gradient(*top, *bottom))
            }
            _ => None,
        }
    }
    /// Returns the colour at relative height `t`, 0 at the top and 1 at the bottom.
    fn at(&self, t: f32) -> Color {
        match self {
            Self::Flat(color) => *color,
            Self::Gradient(top, bottom) => {
                std::array::from_fn(|i| top[i] + (bottom[i] - top[i]) * t)
            }
        }
    }
}

/// Intersects two `[x, y, width, height]` pixel rects; disjoint rects give a zero size.
fn intersect(a: [i32; 4], b: [i32; 4]) -> [i32; 4] {
    let x = a[0].max(b[0]);
    let y = a[1].max(b[1]);
    let right = (a[0] + a[2]).min(b[0] + b[2]);
    let top = (a[1] + a[3]).min(b[1] + b[3]);
    [x, y, (right - x).max(0), (top - y).max(0)]
}

/// Square images skip the rounded-rectangle distance field: a negative radius selects the
/// shader's plain texture path. Per pixel that field cost as much as the rest of the Home scene
/// on a webOS 5 Mali for the full-screen wallpaper.
fn plain_radius(radius: f32) -> f32 {
    if radius > 0. { radius } else { -1. }
}

/// Measurement aid: ASTRO_PROFILE_SKIP=wallpaper,image,app,shape,gradient,shadow,text or all
/// leaves those draws out, to find which part of the scene costs the GPU time.
fn profile_skip(command: &Draw) -> bool {
    static SKIP: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    let skip = SKIP.get_or_init(|| {
        std::env::var("ASTRO_PROFILE_SKIP")
            .map(|value| value.split(',').map(str::to_owned).collect())
            .unwrap_or_default()
    });
    if skip.is_empty() {
        return false;
    }
    let kind = match command {
        Draw::Image(_, rect, ..) | Draw::TintedImage(_, rect, ..)
            if rect.w >= 1920. && rect.h >= 1080. =>
        {
            "wallpaper"
        }
        Draw::Image(..) | Draw::TintedImage(..) => "image",
        Draw::AppImage(..) => "app",
        Draw::Shape(..) => "shape",
        Draw::RoundedGradient(..) => "gradient",
        Draw::Shadow(..) => "shadow",
        Draw::Text(..) => "text",
        Draw::Clip(_) | Draw::EdgeFade(_) | Draw::EdgeFadeX(_) => return false,
    };
    skip.iter().any(|entry| entry == kind || entry == "all")
}

/// Stem emboldening for small text, in physical pixels per edge: thin stems get an opaque
/// core instead of only grey edge pixels, which a TV's scale-up of the graphics plane would
/// soften further.
/// Full strength up to 16 physical pixels, none from 28. Zero in the default MSAA mode.
fn embolden(physical_size: f32, strength: f32) -> f32 {
    strength * ((28. - physical_size) / 12.).clamp(0., 1.)
}

/// Snaps a logical rect's edges to whole drawable pixels at `scale` drawable pixels per logical
/// pixel, unless `ASTRO_SNAP=0`.
fn snap_rect(rect: Rect, scale: f32) -> Rect {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ENABLED.get_or_init(|| std::env::var("ASTRO_SNAP").as_deref() != Ok("0")) {
        return rect;
    }
    let scale = scale.max(0.01);
    let snap = |value: f32| (value * scale).round() / scale;
    let (x0, y0) = (snap(rect.x), snap(rect.y));
    // Never collapse a visible hairline.
    let x1 = snap(rect.x + rect.w).max(x0 + 1. / scale);
    let y1 = snap(rect.y + rect.h).max(y0 + 1. / scale);
    Rect::new(x0, y0, x1 - x0, y1 - y0)
}

/// The GL painter and its caches of fonts, glyphs, text runs, textures and vector meshes.
pub struct Renderer {
    program: GLuint,
    vector_program: GLuint,
    text_program: GLuint,
    text_embolden: f32,
    buffer: GLuint,
    vao: GLuint,
    white: Texture,
    font_directory: PathBuf,
    /// Loaded faces by font ID (see [`load_font`]).
    fonts: HashMap<u16, Font>,
    failed_fonts: HashSet<u16>,
    glyphs: HashMap<(char, u16), Glyph>,
    text_runs: HashMap<TextKey, TextRun>,
    textures: HashMap<String, Texture>,
    failed_images: HashSet<String>,
    pending_images: HashSet<String>,
    images: image_loader::Loader,
    /// Decode on the SDL thread instead (scripted and screenshot runs need complete frames).
    synchronous_images: bool,
    /// The last full-screen wallpaper drawn, shown while its replacement decodes.
    last_wallpaper: Option<String>,
    /// Upload time not yet reported, counted in the next frame's `image_load_ms`.
    upload_ms: f64,
    /// Loaded meshes by path; `None` remembers a file that failed to load.
    vectors: HashMap<String, Option<vector::Mesh>>,
    /// The file drawn for each wallpaper path (see [`Self::resolve`]).
    artwork_variants: HashMap<String, String>,
    /// This frame's paint vertices, 14 `f32` each: position, uv, colour, shape and parameters.
    vertices: Vec<f32>,
    batches: Vec<Batch>,
    clip: Option<Rect>,
    // [top fade-in, top end, bottom start, bottom fade-out, left ×2, right ×2]; 0 = off.
    fade: Option<[f32; 8]>,
    frame: u64,
    stats: RenderStats,
    pub info: GraphicsInfo,
}

impl Renderer {
    /// Sets the vertical (`offset` 0) or horizontal (`offset` 4) half of the edge fade.
    fn set_fade(&mut self, offset: usize, fade: Option<[f32; 4]>) {
        let mut next = self.fade.unwrap_or([0.; 8]);
        next[offset..offset + 4].copy_from_slice(&fade.unwrap_or([0.; 4]));
        self.fade = (next != [0.; 8]).then_some(next);
    }

    /// Compiles the shaders and opens the regular font from the directory containing `font`.
    ///
    /// `gles` selects GLSL ES 1.00 shaders; otherwise GLSL 1.50 for a desktop core profile, which
    /// also needs a vertex array object. `text_embolden` is the small-text emboldening strength in
    /// physical pixels per edge.
    ///
    /// # Errors
    ///
    /// Fails when the regular font cannot be loaded or a shader does not compile or link.
    pub fn new(font: &Path, gles: bool, text_embolden: f32) -> Result<Self, String> {
        let directory = font.parent().ok_or("Invalid font path")?;
        let mut fonts = HashMap::new();
        // Open only the common face. Metrics and outlines stay lazy even when
        // the much larger CJK fallback is needed later.
        fonts.insert(400, load_font(directory, 400)?);
        let mut max_texture_size = 0;
        let mut max_viewport = [0; 2];
        let mut precision = 0;
        // SAFETY: the renderer's GL context is current on the SDL thread (see the module docs); the out pointers
        // name live locals of the sizes these queries write.
        unsafe {
            gl::GetIntegerv(gl::MAX_TEXTURE_SIZE, &mut max_texture_size);
            gl::GetIntegerv(gl::MAX_VIEWPORT_DIMS, max_viewport.as_mut_ptr());
            if gles && gl::GetShaderPrecisionFormat::is_loaded() {
                let mut range = [0; 2];
                gl::GetShaderPrecisionFormat(
                    gl::FRAGMENT_SHADER,
                    gl::HIGH_FLOAT,
                    range.as_mut_ptr(),
                    &mut precision,
                );
            }
        }
        let fragment_highp = !gles || precision > 0;
        let qualifier = if fragment_highp { "highp" } else { "mediump" };
        let vertex_prefix = if gles {
            format!("#version 100\nprecision highp float;\n#define PAINT_PRECISION {qualifier}\n")
        } else {
            "#version 150\n#define attribute in\n#define varying out\n#define PAINT_PRECISION\n"
                .into()
        };
        let fragment_prefix = if gles {
            format!(
                "#version 100\nprecision {qualifier} float;\n#define PAINT_PRECISION {qualifier}\n"
            )
        } else {
            "#version 150\n#define varying in\n#define texture2D texture\nout vec4 out_color;\n#define gl_FragColor out_color\n#define PAINT_PRECISION\n".into()
        };
        let program = link(
            &(vertex_prefix.clone() + include_str!("shaders/paint.vert")),
            &(fragment_prefix.clone() + include_str!("shaders/paint.frag")),
            &[c"a_position", c"a_uv", c"a_color", c"a_shape", c"a_params"],
        )?;
        let vector_program = match link(
            &(vertex_prefix.clone() + include_str!("shaders/vector.vert")),
            &(fragment_prefix.clone() + include_str!("shaders/vector.frag")),
            &[c"a_position", c"a_offset", c"a_color", c"a_coverage"],
        ) {
            Ok(program) => program,
            Err(error) => {
                // SAFETY: the context is current and `program` was linked above.
                unsafe { gl::DeleteProgram(program) };
                return Err(error);
            }
        };
        let text_program = match link(
            &(vertex_prefix + include_str!("shaders/text.vert")),
            &(fragment_prefix + include_str!("shaders/vector.frag")),
            &[c"a_position", c"a_offset", c"a_origin", c"a_coverage"],
        ) {
            Ok(program) => program,
            Err(error) => {
                // SAFETY: the context is current and both programs were linked above.
                unsafe {
                    gl::DeleteProgram(program);
                    gl::DeleteProgram(vector_program);
                }
                return Err(error);
            }
        };
        let mut buffer = 0;
        let mut vao = 0;
        // SAFETY: the renderer's GL context is current on the SDL thread (see the module docs); `buffer` and
        // `vao` are live out pointers.
        unsafe {
            if !gles {
                gl::GenVertexArrays(1, &mut vao);
                gl::BindVertexArray(vao);
            }
            gl::GenBuffers(1, &mut buffer);
            gl::Enable(gl::BLEND);
            // Preserve an opaque destination alpha. Using BlendFunc for both
            // channels makes translucent UI darken again in PNG/window compositing.
            gl::BlendFuncSeparate(
                gl::SRC_ALPHA,
                gl::ONE_MINUS_SRC_ALPHA,
                gl::ONE,
                gl::ONE_MINUS_SRC_ALPHA,
            );
            gl::Disable(gl::DEPTH_TEST);
            gl::PixelStorei(gl::UNPACK_ALIGNMENT, 1);
        }
        let white = texture(1, 1, &[255; 4]);
        Ok(Self {
            program,
            vector_program,
            text_program,
            text_embolden,
            buffer,
            vao,
            white,
            font_directory: directory.into(),
            fonts,
            failed_fonts: HashSet::new(),
            glyphs: HashMap::new(),
            text_runs: HashMap::new(),
            textures: HashMap::new(),
            failed_images: HashSet::new(),
            pending_images: HashSet::new(),
            images: image_loader::Loader::new(),
            synchronous_images: false,
            last_wallpaper: None,
            upload_ms: 0.,
            vectors: HashMap::new(),
            artwork_variants: HashMap::new(),
            vertices: Vec::with_capacity(65536),
            batches: Vec::new(),
            clip: None,
            fade: None,
            frame: 0,
            stats: RenderStats::new(Size::HD),
            info: GraphicsInfo {
                vendor: gl_string(gl::VENDOR),
                renderer: gl_string(gl::RENDERER),
                version: gl_string(gl::VERSION),
                max_texture_size: max_texture_size.max(0) as u32,
                max_viewport,
                fragment_highp,
            },
        })
    }

    /// Loads font `id` on first use; a failure is logged once and remembered.
    fn ensure_font(&mut self, id: u16) -> bool {
        if self.fonts.contains_key(&id) {
            return true;
        }
        if self.failed_fonts.contains(&id) {
            return false;
        }
        match load_font(&self.font_directory, id) {
            Ok(font) => {
                self.fonts.insert(id, font);
                true
            }
            Err(error) => {
                eprintln!("{error}");
                self.failed_fonts.insert(id);
                false
            }
        }
    }

    /// Returns the outline of `c` in font `font`, which must be loaded, tessellating it on first
    /// use.
    fn glyph(&mut self, c: char, font: u16) -> Result<&[astro_vector_geometry::Vertex], String> {
        let key = (c, font);
        if !self.glyphs.contains_key(&key) {
            let started = Instant::now();
            let geometry = self.fonts[&font].geometry(c)?;
            self.stats.glyph_ms += started.elapsed().as_secs_f64() * 1000.;
            self.glyphs.insert(
                key,
                Glyph {
                    geometry,
                    frame: self.frame,
                },
            );
            self.stats.glyph_tessellations += 1;
        }
        let glyph = self.glyphs.get_mut(&key).unwrap();
        glyph.frame = self.frame;
        Ok(&glyph.geometry)
    }

    /// Evicts the least recently drawn glyphs and text runs beyond their budgets, keeping those
    /// drawn this frame, and updates the cache statistics.
    fn trim_text_geometry(&mut self) {
        let glyph_bytes =
            |glyph: &Glyph| glyph.geometry.len() * size_of::<astro_vector_geometry::Vertex>();
        let mut bytes: usize = self.glyphs.values().map(glyph_bytes).sum();
        if bytes > GLYPH_GEOMETRY_BUDGET || self.glyphs.len() > 4096 {
            let mut old: Vec<_> = self
                .glyphs
                .iter()
                .filter(|(_, glyph)| glyph.frame != self.frame)
                .map(|(key, glyph)| (*key, glyph.frame))
                .collect();
            old.sort_by_key(|(_, frame)| *frame);
            for (key, _) in old {
                if bytes <= GLYPH_GEOMETRY_BUDGET && self.glyphs.len() <= 4096 {
                    break;
                }
                if let Some(glyph) = self.glyphs.remove(&key) {
                    bytes -= glyph_bytes(&glyph);
                }
            }
        }
        let run_bytes = |run: &TextRun| run.mesh.as_ref().map_or(0, |mesh| mesh.bytes);
        let mut bytes: usize = self.text_runs.values().map(run_bytes).sum();
        if bytes > TEXT_GEOMETRY_BUDGET || self.text_runs.len() > 512 {
            let mut old: Vec<_> = self
                .text_runs
                .iter()
                .filter(|(_, run)| run.frame != self.frame)
                .map(|(key, run)| (key.clone(), run.frame))
                .collect();
            old.sort_by_key(|(_, frame)| *frame);
            for (key, _) in old {
                if bytes <= TEXT_GEOMETRY_BUDGET && self.text_runs.len() <= 512 {
                    break;
                }
                if let Some(run) = self.text_runs.remove(&key) {
                    bytes -= run_bytes(&run);
                }
            }
        }
        self.stats.cached_glyphs = self.glyphs.len();
        self.stats.glyph_geometry_bytes = self.glyphs.values().map(glyph_bytes).sum();
        self.stats.cached_text_runs = self.text_runs.len();
        self.stats.text_geometry_bytes = self.text_runs.values().map(run_bytes).sum();
    }

    /// Adds a single-colour [`Self::gradient_quad`].
    fn quad(
        &mut self,
        rect: Rect,
        uv: [f32; 4],
        radius: f32,
        border: f32,
        color: Color,
        texture: GLuint,
    ) {
        self.gradient_quad(rect, uv, radius, border, [color, color], texture, None);
    }

    /// Adds a pixel-snapped quad for the paint shader.
    ///
    /// `uv` maps the texture, `colors` are the top and bottom vertex colours, a positive `border`
    /// draws only an outline that wide, and a negative one blurs the edge by that distance. A
    /// negative `radius` skips the rounded shape (see [`plain_radius`]); `backdrop` draws app
    /// artwork over its gradient.
    #[allow(clippy::too_many_arguments)]
    fn gradient_quad(
        &mut self,
        rect: Rect,
        uv: [f32; 4],
        radius: f32,
        border: f32,
        colors: [Color; 2],
        texture: GLuint,
        backdrop: Option<[[u8; 3]; 2]>,
    ) {
        if rect.w <= 0. || rect.h <= 0. {
            return;
        }
        let rect = self.snap(rect);
        let start = self.vertices.len() / 14;
        for (x, y) in [(0., 0.), (1., 0.), (0., 1.), (0., 1.), (1., 0.), (1., 1.)] {
            let color = colors[y as usize];
            self.vertices.extend_from_slice(&[
                rect.x + x * rect.w,
                rect.y + y * rect.h,
                uv[0] + x * (uv[2] - uv[0]),
                uv[1] + y * (uv[3] - uv[1]),
                color[0],
                color[1],
                color[2],
                color[3],
                (x - 0.5) * rect.w,
                (y - 0.5) * rect.h,
                rect.w / 2.,
                rect.h / 2.,
                radius,
                border,
            ]);
        }
        self.paint_batch(start, 6, texture, backdrop, None);
    }

    /// Appends `count` paint vertices from `start`, merged into the previous batch when it uses
    /// the same textures, clip and fade.
    fn paint_batch(
        &mut self,
        start: usize,
        count: usize,
        texture: GLuint,
        backdrop: Option<[[u8; 3]; 2]>,
        outgoing: Option<GLuint>,
    ) {
        if let Some(batch) = self.batches.last_mut() {
            let same_clip = match (batch.clip, self.clip) {
                (None, None) => true,
                (Some(a), Some(b)) => a.x == b.x && a.y == b.y && a.w == b.w && a.h == b.h,
                _ => false,
            };
            if same_clip
                && batch.fade == self.fade
                && let BatchKind::Paint {
                    texture: previous,
                    count: batch_count,
                    backdrop: previous_backdrop,
                    outgoing: previous_outgoing,
                    ..
                } = &mut batch.kind
                && *previous == texture
                && *previous_backdrop == backdrop
                && *previous_outgoing == outgoing
            {
                *batch_count += count;
                return;
            }
        }
        self.batches.push(Batch {
            kind: BatchKind::Paint {
                texture,
                start,
                count,
                backdrop,
                outgoing,
            },
            clip: self.clip,
            fade: self.fade,
        });
    }

    /// The wallpaper with full-screen translucent fills folded in: each fill maps a pixel p to
    /// p * (1 - a) + c * a, so together they are p * m + k, drawn in one pass instead of one
    /// full-screen layer each. Strips keep gradients' products of colour and alpha exact.
    /// With `outgoing` (texture, alpha), p is the mix of that wallpaper and `texture` at alpha:
    /// what `texture` drawn at alpha over it leaves, so a cross-fade is one pass as well.
    fn folded_quad(
        &mut self,
        rect: Rect,
        texture: GLuint,
        scrims: &[Scrim],
        outgoing: Option<(GLuint, f32)>,
    ) {
        let (mode, mix) = match outgoing {
            Some((_, alpha)) => (-3., alpha),
            None => (-2., 0.),
        };
        const STRIPS: usize = 16;
        let fold = |t: f32| {
            let (mut m, mut k) = ([1f32; 3], [0f32; 3]);
            for scrim in scrims {
                let color = scrim.at(t);
                for i in 0..3 {
                    m[i] *= 1. - color[3];
                    k[i] = k[i] * (1. - color[3]) + color[i] * color[3];
                }
            }
            (m, k)
        };
        let start = self.vertices.len() / 14;
        for strip in 0..STRIPS {
            let rows = [
                strip as f32 / STRIPS as f32,
                (strip + 1) as f32 / STRIPS as f32,
            ];
            let folds = rows.map(fold);
            for (x, row) in [(0., 0), (1., 0), (0., 1), (0., 1), (1., 0), (1., 1)] {
                let (m, k) = folds[row];
                let t = rows[row];
                self.vertices.extend_from_slice(&[
                    rect.x + x * rect.w,
                    rect.y + t * rect.h,
                    x,
                    t,
                    m[0],
                    m[1],
                    m[2],
                    1.,
                    k[0],
                    k[1],
                    k[2],
                    mix,
                    mode,
                    0.,
                ]);
            }
        }
        self.paint_batch(
            start,
            STRIPS * 6,
            texture,
            None,
            outgoing.map(|(texture, _)| texture),
        );
    }

    /// Lays out and queues a text run, reusing the cached mesh when the same text and layout were
    /// drawn before.
    ///
    /// Lines wrap at spaces, or between characters for a word wider than the box, and stop at the
    /// box height. Characters missing from the main font fall back to the CJK or Korean font.
    /// Text entirely outside the screen or clip is skipped.
    fn text(&mut self, text: &str, rect: Rect, style: TextStyle, color: Color, centered: bool) {
        // Layout remains logical. Reuse the same run geometry at every density;
        // its shader snaps insertion origins and computes pixel-sized coverage.
        let size_f = style.size;
        let viewport = self.stats.viewport;
        self.stats.max_logical_font_px = self.stats.max_logical_font_px.max(size_f);
        self.stats.max_physical_font_px = self
            .stats
            .max_physical_font_px
            .max(size_f * viewport.text_scale());
        let padded = rect.inset(-size_f);
        if padded.x + padded.w < 0.
            || padded.y + padded.h < 0.
            || padded.x > 1920.
            || padded.y > 1080.
            || self.clip.is_some_and(|clip| {
                padded.x + padded.w < clip.x
                    || padded.y + padded.h < clip.y
                    || padded.x > clip.x + clip.w
                    || padded.y > clip.y + clip.h
            })
        {
            return;
        }
        let origin = [rect.x, rect.y];
        let key = TextKey {
            text: text.into(),
            weight: style.weight,
            layout: [
                size_f.to_bits(),
                style.tracking.to_bits(),
                style.line_height.to_bits(),
                rect.w.to_bits(),
                rect.h.to_bits(),
            ],
            flags: u8::from(style.align_right)
                | (u8::from(centered) << 1)
                | (u8::from(self.stats.multisample_samples > 1) << 2),
        };
        if self.text_runs.contains_key(&key) {
            self.submit_text(key, origin, color);
            return;
        }
        let rect = Rect::new(0., 0., rect.w, rect.h);
        // Font IDs name files (see `load_font`): the weight, +1000 for the display cut from 32 px,
        // and 2400/2500 (CJK) or 3400/3500 (Korean) for the fallbacks.
        let font_id = style.weight + if style.size >= 32. { 1000 } else { 0 };
        let cjk_id = if style.weight >= 500 { 2500 } else { 2400 };
        let hangul_id = cjk_id + 1000;
        if !self.ensure_font(font_id) {
            self.stats.glyph_failures += text.chars().count();
            return;
        }
        let (mut needs_cjk, mut needs_hangul) = (false, false);
        for c in text.chars() {
            if !c.is_ascii_control() && self.fonts[&font_id].lookup_glyph_index(c) == 0 {
                if is_hangul(c) {
                    needs_hangul = true;
                } else {
                    needs_cjk = true;
                }
            }
        }
        // Each fallback loads only when a run needs it, so Korean text never
        // pulls in the larger Chinese font and vice versa.
        let cjk_ready = needs_cjk && self.ensure_font(cjk_id);
        let hangul_ready = needs_hangul && self.ensure_font(hangul_id);
        let fonts = &self.fonts;
        let choose_font = |c: char| {
            if fonts[&font_id].lookup_glyph_index(c) != 0 {
                font_id
            } else if is_hangul(c) {
                if hangul_ready { hangul_id } else { font_id }
            } else if cjk_ready {
                cjk_id
            } else {
                font_id
            }
        };
        let font = &fonts[&font_id];
        let line_height = size_f * style.line_height;
        let baseline = font.horizontal_line_metrics(size_f).map_or(size_f, |m| {
            (line_height - m.ascent + m.descent) / 2. + m.ascent
        });
        let tracking = style.tracking * size_f;
        let advance = |c| {
            if c == '\n' {
                0.
            } else {
                fonts[&choose_font(c)].metrics(c, size_f).advance_width + tracking
            }
        };
        let mut lines: Vec<String> = Vec::new();
        let mut line = String::new();
        let mut width = 0.;
        // Word wrapping, with character fallback for a title wider than the tile.
        for word in text.split_inclusive(char::is_whitespace) {
            let word_width: f32 = word.chars().map(advance).sum();
            if width > 0. && width + word_width > rect.w {
                lines.push(line.trim_end().into());
                line.clear();
                width = 0.;
            }
            for c in word.chars() {
                if c == '\n' {
                    lines.push(line.trim_end().into());
                    line.clear();
                    width = 0.;
                    continue;
                }
                let w = advance(c);
                if width > 0. && width + w > rect.w {
                    lines.push(line.trim_end().into());
                    line.clear();
                    width = 0.;
                }
                line.push(c);
                width += w;
            }
        }
        if !line.is_empty() {
            lines.push(line.trim_end().into());
        }
        let mut placed = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            let top = rect.y + index as f32 * line_height;
            if index > 0 && top + line_height > rect.y + rect.h + 1. {
                break;
            }
            let mut previous = None;
            let width: f32 = line
                .chars()
                .map(|c| {
                    let kern = previous
                        .and_then(|p| font.horizontal_kern(p, c, size_f))
                        .unwrap_or(0.);
                    previous = Some(c);
                    advance(c) + kern
                })
                .sum();
            let mut x = rect.x
                + if style.align_right {
                    rect.w - width
                } else if centered {
                    (rect.w - width) / 2.
                } else {
                    0.
                };
            let mut previous = None;
            for c in line.chars() {
                if let Some(previous) = previous {
                    x += font.horizontal_kern(previous, c, size_f).unwrap_or(0.);
                }
                let selected = choose_font(c);
                let baseline = if selected == font_id {
                    baseline
                } else {
                    fonts[&selected]
                        .horizontal_line_metrics(size_f)
                        .map_or(baseline, |m| {
                            (line_height - m.ascent + m.descent) / 2. + m.ascent
                        })
                };
                placed.push((c, x, top + baseline, selected));
                x += advance(c);
                previous = Some(c);
            }
        }
        let mut vertices = Vec::new();
        let mut missing_glyphs = 0;
        let mut failures = 0;
        let multisampled = self.stats.multisample_samples > 1;
        for (c, x, baseline, selected) in placed {
            if self.fonts[&selected].lookup_glyph_index(c) == 0 && !c.is_whitespace() {
                missing_glyphs += 1;
            }
            match self.glyph(c, selected) {
                Ok(geometry) => {
                    for triangle in geometry.chunks_exact(3) {
                        // True sample coverage needs only the original fill.
                        // Exclude the boundary fringe, not just its alpha.
                        if multisampled && triangle.iter().any(|vertex| vertex.coverage < 1.) {
                            continue;
                        }
                        for vertex in triangle {
                            vertices.extend_from_slice(&[
                                vertex.position.x * size_f,
                                vertex.position.y * size_f,
                                if multisampled { 0. } else { vertex.offset[0] },
                                if multisampled { 0. } else { vertex.offset[1] },
                                x,
                                baseline,
                                0.,
                                0.,
                                vertex.coverage,
                            ]);
                        }
                    }
                }
                Err(error) => {
                    eprintln!("Astro glyph {c:?}: {error}");
                    failures += 1;
                }
            }
        }
        let mesh = if vertices.is_empty() {
            None
        } else {
            match vector::Mesh::upload(1., 1., &vertices) {
                Ok(mesh) => Some(mesh),
                Err(error) => {
                    eprintln!("Astro text: {error}");
                    failures += 1;
                    None
                }
            }
        };
        self.text_runs.insert(
            key.clone(),
            TextRun {
                mesh,
                missing_glyphs,
                failures,
                frame: self.frame,
            },
        );
        self.submit_text(key, origin, color);
    }

    /// Queues a cached text run at `origin` and adds its glyph counts to this frame's statistics.
    fn submit_text(&mut self, key: TextKey, origin: [f32; 2], tint: Color) {
        let run = self.text_runs.get_mut(&key).unwrap();
        run.frame = self.frame;
        self.stats.missing_glyphs += run.missing_glyphs;
        self.stats.glyph_failures += run.failures;
        if let Some(mesh) = &run.mesh {
            self.stats.text_triangles += mesh.vertices as usize / 3;
            self.batches.push(Batch {
                kind: BatchKind::Text { key, origin, tint },
                clip: self.clip,
                fade: self.fade,
            });
        }
    }

    /// Draws an image command without folded fills.
    fn image(&mut self, path: &str, rect: Rect, radius: f32, contain: bool, tint: Color) {
        self.folded_image(path, rect, radius, contain, tint, &[]);
    }

    /// Returns the file actually drawn for `path`: a 4K drawable uses a wallpaper's `@2x`
    /// variant when the file exists.
    fn resolve(&mut self, path: &str) -> String {
        if self.stats.viewport.scale > 1.
            && self.info.max_texture_size >= 3840
            && Path::new(path)
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|p| p == "wallpapers")
        {
            self.artwork_variants
                .entry(path.into())
                .or_insert_with(|| {
                    let original = Path::new(path);
                    let variant = original.with_file_name(format!(
                        "{}@2x.jpg",
                        original.file_stem().unwrap_or_default().to_string_lossy()
                    ));
                    if variant.is_file() {
                        variant.to_string_lossy().into_owned()
                    } else {
                        path.into()
                    }
                })
                .clone()
        } else {
            path.into()
        }
    }

    /// Decode on the SDL thread (scripted runs) instead of the image worker.
    pub fn set_synchronous_images(&mut self, synchronous: bool) {
        self.synchronous_images = synchronous;
    }

    /// Starts decoding `path` unless it is already uploaded, pending or failed.
    pub fn request(&mut self, path: &str) {
        let path = self.resolve(path);
        self.request_resolved(&path);
    }

    /// Requests an already resolved path, decoding it on this thread in synchronous mode or
    /// when the worker is unavailable.
    fn request_resolved(&mut self, path: &str) {
        if self.textures.contains_key(path)
            || self.failed_images.contains(path)
            || self.pending_images.contains(path)
        {
            return;
        }
        if !self.synchronous_images && self.images.request(path) {
            self.pending_images.insert(path.into());
            return;
        }
        let started = Instant::now();
        let deferred = self.upload_ms;
        let pixels = image_loader::decode(Path::new(path))
            .map_err(|error| eprintln!("Artwork {path}: {error}"))
            .ok();
        self.pending_images.insert(path.into());
        self.upload(Decoded {
            path: path.into(),
            pixels,
        });
        // Counted in this frame with its decode, not again on the next frame.
        self.upload_ms = deferred;
        self.stats.image_load_ms += started.elapsed().as_secs_f64() * 1000.;
    }

    /// Uploads a decoded image. Replies for paths no longer pending (forgotten while
    /// decoding) are dropped. Returns the texture bytes uploaded.
    fn upload(&mut self, decoded: Decoded) -> usize {
        if !self.pending_images.remove(&decoded.path) {
            return 0;
        }
        let Some(pixels) = decoded.pixels else {
            if self.failed_images.len() >= FAILED_IMAGE_LIMIT {
                self.failed_images.clear();
            }
            self.failed_images.insert(decoded.path);
            return 0;
        };
        let started = Instant::now();
        let format = if pixels.has_alpha() {
            gl::RGBA
        } else {
            gl::RGB
        };
        let mut texture = texture_with(pixels.width(), pixels.height(), format, pixels.raw());
        texture.frame = self.frame;
        let bytes = texture.bytes;
        self.textures.insert(decoded.path, texture);
        self.upload_ms += started.elapsed().as_secs_f64() * 1000.;
        bytes
    }

    /// Uploads images the worker finished, about one wallpaper's worth per call. Returns the
    /// scene paths that now draw differently (including a 4K variant's original path), so
    /// partial redraw can repaint them although the scene itself did not change.
    pub fn poll(&mut self) -> Vec<String> {
        let mut loaded = Vec::new();
        let mut uploaded = 0;
        while uploaded < UPLOAD_BUDGET
            && let Some(decoded) = self.images.try_recv()
        {
            let path = decoded.path.clone();
            if !self.pending_images.contains(&path) {
                // Forgotten while decoding: dropped by `upload`.
                self.upload(decoded);
                continue;
            }
            // A failure also changes pixels: a wallpaper substitute stops being drawn.
            uploaded += self.upload(decoded);
            loaded.extend(
                self.artwork_variants
                    .iter()
                    .filter(|(original, variant)| *variant == &path && *original != &path)
                    .map(|(original, _)| original.clone()),
            );
            loaded.push(path);
        }
        loaded
    }

    /// Whether `path` has been decoded (uploaded, or failed and drawn as missing).
    pub fn ready(&mut self, path: &str) -> bool {
        let path = self.resolve(path);
        self.textures.contains_key(&path) || self.failed_images.contains(&path)
    }

    /// Whether an image is still decoding, so the event loop should wake soon to poll.
    pub fn pending(&self) -> bool {
        !self.pending_images.is_empty()
    }

    /// Blocks until every requested image is decoded and uploaded, at most `timeout`; e.g. after
    /// preparing the first frame, so the window never opens on an empty background.
    pub fn wait_pending(&mut self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while !self.pending_images.is_empty()
            && let Some(decoded) = self.images.recv_until(deadline)
        {
            self.upload(decoded);
        }
    }

    /// Drops cached, failed and pending images whose path contains `fragment`, e.g. a
    /// deleted wallpaper's directory name.
    pub fn forget(&mut self, fragment: &str) {
        self.textures.retain(|path, _| !path.contains(fragment));
        self.failed_images.retain(|path| !path.contains(fragment));
        self.pending_images.retain(|path| !path.contains(fragment));
        self.artwork_variants
            .retain(|path, variant| !path.contains(fragment) && !variant.contains(fragment));
        if self
            .last_wallpaper
            .as_ref()
            .is_some_and(|path| path.contains(fragment))
        {
            self.last_wallpaper = None;
        }
    }

    /// Draws a raster image or `.lvec` mesh into `rect`, folding `scrims` into a full-screen
    /// wallpaper.
    fn folded_image(
        &mut self,
        path: &str,
        mut rect: Rect,
        radius: f32,
        contain: bool,
        tint: Color,
        scrims: &[Scrim],
    ) {
        if path.ends_with(".lvec") {
            self.vector_image(path, rect, contain, tint);
            return;
        }
        let Some((id, width, height)) = self.texture_for(path, &rect, radius, contain) else {
            return;
        };
        if contain {
            let scale = (rect.w / width as f32).min(rect.h / height as f32);
            let w = width as f32 * scale;
            let h = height as f32 * scale;
            rect = Rect::new(rect.x + (rect.w - w) / 2., rect.y + (rect.h - h) / 2., w, h);
        }
        if scrims.is_empty() {
            self.quad(rect, [0., 0., 1., 1.], plain_radius(radius), 0., tint, id);
        } else {
            self.folded_quad(rect, id, scrims, None);
        }
    }

    /// Whether `path` (its 4K variant, if drawn instead) is uploaded without an alpha channel.
    fn opaque(&mut self, path: &str) -> bool {
        let path = self.resolve(path);
        self.textures
            .get(&path)
            .is_some_and(|texture| texture.opaque)
    }

    /// A cross-fade between two uploaded opaque wallpapers in one pass with `scrims` folded in:
    /// the incoming wallpaper drawn at `alpha` over the outgoing one is their mix, which the
    /// shader computes before applying the folded fills.
    fn crossfade(&mut self, outgoing: &str, incoming: &str, alpha: f32, scrims: &[Scrim]) {
        let screen = Rect::new(0., 0., 1920., 1080.);
        let Some((outgoing, ..)) = self.texture_for(outgoing, &screen, 0., false) else {
            return;
        };
        let Some((incoming, ..)) = self.texture_for(incoming, &screen, 0., false) else {
            return;
        };
        self.folded_quad(screen, incoming, scrims, Some((outgoing, alpha)));
    }

    /// The texture (id, width, height) drawn for an image command this frame, requested if not
    /// yet loaded; none for an offscreen, failed or still decoding image. A full-screen wallpaper
    /// still decoding is replaced by the last wallpaper drawn.
    fn texture_for(
        &mut self,
        path: &str,
        rect: &Rect,
        radius: f32,
        contain: bool,
    ) -> Option<(GLuint, u32, u32)> {
        // Offscreen cards, e.g. gallery thumbnails scrolled out of view, are not loaded.
        if !visible(rect, self.clip) {
            return None;
        }
        let path = self.resolve(path);
        if path.ends_with("@2x.jpg") {
            self.stats.high_resolution_wallpaper = true;
        }
        let wallpaper = !contain && radius <= 0. && full_screen(rect);
        self.request_resolved(&path);
        let path = if self.textures.contains_key(&path) {
            path
        } else if self.failed_images.contains(&path) {
            self.stats.missing_images += 1;
            return None;
        } else if wallpaper
            && let Some(last) = self
                .last_wallpaper
                .clone()
                .filter(|last| self.textures.contains_key(last))
        {
            last
        } else {
            self.stats.pending_images += 1;
            return None;
        };
        if wallpaper {
            self.last_wallpaper = Some(path.clone());
        }
        let frame = self.frame;
        let texture = self.textures.get_mut(&path)?;
        texture.frame = frame;
        Some((texture.id, texture.width, texture.height))
    }

    /// Moves edges onto whole drawable pixels. A TV that scales a 1080p graphics plane up to the
    /// panel turns a half-covered edge pixel into a soft band; whole pixels stay crisp.
    /// `ASTRO_SNAP=0` keeps logical positions (for comparison).
    fn snap(&self, rect: Rect) -> Rect {
        snap_rect(rect, self.stats.viewport.scale)
    }

    /// Loads a `.lvec` mesh on first use and queues it, clipped to the current clip.
    fn vector_image(&mut self, path: &str, mut rect: Rect, contain: bool, tint: Color) {
        if rect.w <= 0. || rect.h <= 0. {
            return;
        }
        let scale = self.stats.viewport.scale;
        let started = Instant::now();
        let loaded = !self.vectors.contains_key(path);
        let mesh = self.vectors.entry(path.into()).or_insert_with(|| {
            match vector::Mesh::load(Path::new(path)) {
                Ok(mesh) => Some(mesh),
                Err(error) => {
                    eprintln!("{error}");
                    None
                }
            }
        });
        if loaded {
            self.stats.vector_load_ms += started.elapsed().as_secs_f64() * 1000.;
        }
        let Some(mesh) = mesh else {
            self.stats.missing_images += 1;
            return;
        };
        if contain {
            let scale = (rect.w / mesh.width).min(rect.h / mesh.height);
            let width = mesh.width * scale;
            let height = mesh.height * scale;
            rect = Rect::new(
                rect.x + (rect.w - width) / 2.,
                rect.y + (rect.h - height) / 2.,
                width,
                height,
            );
        }
        // Logos keep their size; only their placement moves onto the pixel grid.
        let snapped = snap_rect(rect, scale);
        rect = Rect::new(snapped.x, snapped.y, rect.w, rect.h);
        let clip = if let Some(clip) = self.clip {
            let x = rect.x.max(clip.x);
            let y = rect.y.max(clip.y);
            Rect::new(
                x,
                y,
                (rect.x + rect.w).min(clip.x + clip.w).max(x) - x,
                (rect.y + rect.h).min(clip.y + clip.h).max(y) - y,
            )
        } else {
            rect
        };
        if clip.w <= 0. || clip.h <= 0. {
            return;
        }
        self.stats.vector_triangles += mesh.vertices as usize / 3;
        self.batches.push(Batch {
            kind: BatchKind::Vector {
                path: path.into(),
                rect,
                tint,
            },
            clip: Some(clip),
            fade: self.fade,
        });
    }

    /// Draws installed app artwork, fitted to 68% of the card, over its sampled backdrop
    /// gradient; the texture is uploaded on first use.
    fn app_image(
        &mut self,
        image: &crate::app_artwork::Artwork,
        rect: Rect,
        radius: f32,
        tint: Color,
    ) {
        let texture = self.textures.entry(image.key.clone()).or_insert_with(|| {
            texture(
                image.pixels.width(),
                image.pixels.height(),
                image.pixels.as_raw(),
            )
        });
        texture.frame = self.frame;
        let scale =
            (rect.w * 0.68 / texture.width as f32).min(rect.h * 0.68 / texture.height as f32);
        let w = texture.width as f32 * scale;
        let h = texture.height as f32 * scale;
        let id = texture.id;
        self.gradient_quad(
            rect,
            [
                0.5 - rect.w / (2. * w),
                0.5 - rect.h / (2. * h),
                0.5 + rect.w / (2. * w),
                0.5 + rect.h / (2. * h),
            ],
            radius,
            0.,
            [tint; 2],
            id,
            Some(image.backdrop),
        );
    }

    /// A logical damage rect in drawable pixels ([x, y, width, height], origin bottom-left),
    /// grown to whole pixels.
    pub fn damage_pixels(rect: Rect, width: u32, height: u32) -> [i32; 4] {
        let viewport = Viewport::new(Size::new(width, height));
        let sx = viewport.width as f32 / 1920.;
        let sy = viewport.height as f32 / 1080.;
        let x0 = (viewport.x as f32 + rect.x.max(0.) * sx).floor() as i32;
        let x1 = (viewport.x as f32 + (rect.x + rect.w).min(1920.) * sx).ceil() as i32;
        let y0 = (viewport.y as f32 + (1080. - (rect.y + rect.h).min(1080.)) * sy).floor() as i32;
        let y1 = (viewport.y as f32 + (1080. - rect.y.max(0.)) * sy).ceil() as i32;
        if x1 <= x0 || y1 <= y0 {
            // No change still names, clears and redraws one pixel inside the drawable. An empty
            // or off-surface region left the driver nothing to preserve: Mali presented black.
            return [viewport.x, viewport.y, 1, 1];
        }
        [x0, y0, x1 - x0, y1 - y0]
    }

    /// The logical area drawable pixels from `damage_pixels` cover. Grown to whole pixels it
    /// can exceed the original logical damage; commands are culled against this area, so none
    /// that paints into the cleared pixels is skipped.
    pub fn damage_area(pixels: [i32; 4], width: u32, height: u32) -> Rect {
        let viewport = Viewport::new(Size::new(width, height));
        let sx = viewport.width as f32 / 1920.;
        let sy = viewport.height as f32 / 1080.;
        let [x, y, w, h] = pixels;
        Rect::new(
            (x - viewport.x) as f32 / sx,
            1080. - (y + h - viewport.y) as f32 / sy,
            w as f32 / sx,
            h as f32 / sy,
        )
    }

    /// Prepares first-use work for a scene without drawing it (text layout and glyph
    /// outlines, logo meshes, images), e.g. another page while the screen is idle, so its
    /// first appearance does not pay for it. The visible frame's statistics stay unchanged.
    pub fn prepare(&mut self, commands: &[Draw], width: u32, height: u32) {
        let stats = self.stats.clone();
        // Uploads since the last frame still belong to the next visible frame.
        let uploads = self.upload_ms;
        self.build(commands, width, height, None);
        self.vertices.clear();
        self.batches.clear();
        self.stats = stats;
        self.upload_ms = uploads;
    }

    /// Lays out the scene into vertices and batches; commands entirely outside `area` are
    /// skipped.
    fn build(&mut self, commands: &[Draw], width: u32, height: u32, area: Option<Rect>) {
        self.stats = RenderStats::new(Size::new(width, height));
        self.stats.image_load_ms = std::mem::take(&mut self.upload_ms);
        // Captures can bind a single-sample FBO even when the actual TV window
        // is multisampled, so query the active target rather than assume.
        let mut samples = 0;
        // SAFETY: the renderer's GL context is current on the SDL thread (see the module docs); `samples` is
        // a live out pointer.
        unsafe { gl::GetIntegerv(gl::SAMPLES, &mut samples) };
        self.stats.multisample_samples = samples.max(0) as u32;
        if samples > 1 {
            self.stats.font_antialiasing = "multisample vector coverage";
        }
        self.frame += 1;
        self.vertices.clear();
        self.batches.clear();
        self.clip = None;
        self.fade = None;
        let mut index = 0;
        while index < commands.len() {
            let command = &commands[index];
            index += 1;
            if profile_skip(command) {
                continue;
            }
            if self.clip.is_none()
                && self.fade.is_none()
                && let Some(layers) = background(&commands[index - 1..])
            {
                match layers.incoming {
                    None => {
                        index += layers.len - 1;
                        let screen = Rect::new(0., 0., 1920., 1080.);
                        self.folded_image(
                            layers.wallpaper,
                            screen,
                            0.,
                            false,
                            [1.; 4],
                            &layers.scrims,
                        );
                        continue;
                    }
                    // Otherwise drawn layer by layer, as is a wallpaper still decoding or with
                    // transparency, whose layers do not reduce to one mix of the two.
                    Some((incoming, alpha))
                        if self.opaque(layers.wallpaper) && self.opaque(incoming) =>
                    {
                        index += layers.len - 1;
                        self.crossfade(layers.wallpaper, incoming, alpha, &layers.scrims);
                        continue;
                    }
                    Some(_) => {}
                }
            }
            // Outside the damage the scissor would discard it anyway; skip its layout too.
            if let (Some(area), Some(bounds)) = (area, crate::damage::bounds(command))
                && !crate::damage::intersects(area, bounds)
            {
                continue;
            }
            match command {
                Draw::Shape(rect, radius, border, color) => self.quad(
                    *rect,
                    [0., 0., 1., 1.],
                    *radius,
                    *border,
                    *color,
                    self.white.id,
                ),
                Draw::RoundedGradient(rect, radius, top, bottom) => self.gradient_quad(
                    *rect,
                    [0., 0., 1., 1.],
                    *radius,
                    0.,
                    [*top, *bottom],
                    self.white.id,
                    None,
                ),
                Draw::Shadow(rect, radius, blur, color) => self.quad(
                    rect.inset(-*blur),
                    [0., 0., 1., 1.],
                    *radius,
                    -*blur,
                    *color,
                    self.white.id,
                ),
                Draw::Text(text, rect, size, color, centered) => {
                    self.text(text, *rect, *size, *color, *centered)
                }
                Draw::Image(path, rect, radius, contain) => {
                    self.image(path, *rect, *radius, *contain, [1.; 4])
                }
                Draw::TintedImage(path, rect, radius, contain, tint) => {
                    self.image(path, *rect, *radius, *contain, *tint)
                }
                Draw::AppImage(image, rect, radius, tint) => {
                    self.app_image(image, *rect, *radius, *tint)
                }
                Draw::Clip(clip) => self.clip = *clip,
                Draw::EdgeFade(fade) => self.set_fade(0, *fade),
                Draw::EdgeFadeX(fade) => self.set_fade(4, *fade),
            }
        }
    }

    /// Draws the scene. With `damage` (drawable pixels from `damage_pixels`), only those pixels
    /// are cleared and drawn; commands entirely outside them are skipped, and the rest of the
    /// back buffer keeps its earlier contents.
    pub fn render(&mut self, commands: &[Draw], width: u32, height: u32, damage: Option<[i32; 4]>) {
        let area = damage.map(|pixels| Self::damage_area(pixels, width, height));
        let started = Instant::now();
        self.build(commands, width, height, area);
        let Viewport {
            x: vx,
            y: vy,
            width: vw,
            height: vh,
            scale,
        } = self.stats.viewport;
        if let Some([_, _, w, h]) = damage {
            self.stats.redrawn_fraction = f64::from(w * h) / f64::from((width * height).max(1));
        }
        // SAFETY: the renderer's GL context is current on the SDL thread (see the module docs).
        let scissor = |area: Option<[i32; 4]>| unsafe {
            match area {
                Some([x, y, w, h]) => {
                    gl::Enable(gl::SCISSOR_TEST);
                    gl::Scissor(x, y, w.max(0), h.max(0));
                }
                None => gl::Disable(gl::SCISSOR_TEST),
            }
        };
        // SAFETY: the renderer's GL context is current on the SDL thread (see the module docs). The upload covers
        // exactly `self.vertices`, the attribute offsets and strides match the 14-float paint and
        // 9-float mesh layouts, and every batch's buffer, texture and vertex range was created by
        // this renderer for this frame.
        unsafe {
            gl::Viewport(0, 0, width as i32, height as i32);
            scissor(damage);
            gl::ClearColor(0.04, 0.04, 0.05, 1.);
            gl::Clear(gl::COLOR_BUFFER_BIT);
            gl::Viewport(vx, vy, vw, vh);
            if self.vao != 0 {
                gl::BindVertexArray(self.vao);
            }
            gl::BindBuffer(gl::ARRAY_BUFFER, self.buffer);
            gl::BufferData(
                gl::ARRAY_BUFFER,
                (self.vertices.len() * size_of::<f32>()) as isize,
                self.vertices.as_ptr().cast(),
                gl::DYNAMIC_DRAW,
            );
            gl::UseProgram(self.program);
            // One drawable pixel in logical units: the paint shader's edge filter width.
            gl::Uniform1f(
                gl::GetUniformLocation(self.program, c"u_aa".as_ptr()),
                1. / scale.max(0.01),
            );
            gl::Uniform1i(
                gl::GetUniformLocation(self.program, c"u_texture".as_ptr()),
                0,
            );
            gl::Uniform1i(
                gl::GetUniformLocation(self.program, c"u_outgoing".as_ptr()),
                1,
            );
            gl::ActiveTexture(gl::TEXTURE0);
            let mut paint_bound = false;
            let mut current_fade = None;
            for batch in &self.batches {
                if current_fade != Some(batch.fade) {
                    // The same cheap alpha mask applies to shapes, textures,
                    // icons and text; the unmasked wallpaper remains beneath.
                    let fade = batch.fade.unwrap_or([0.; 8]);
                    for program in [self.program, self.vector_program, self.text_program] {
                        gl::UseProgram(program);
                        gl::Uniform4f(
                            gl::GetUniformLocation(program, c"u_edge_fade".as_ptr()),
                            fade[0],
                            fade[1],
                            fade[2],
                            fade[3],
                        );
                        gl::Uniform4f(
                            gl::GetUniformLocation(program, c"u_edge_fade_x".as_ptr()),
                            fade[4],
                            fade[5],
                            fade[6],
                            fade[7],
                        );
                    }
                    current_fade = Some(batch.fade);
                    paint_bound = false;
                }
                let clip = batch.clip.map(|clip| {
                    [
                        vx + (clip.x * vw as f32 / 1920.).round() as i32,
                        vy + ((1080. - clip.y - clip.h) * vh as f32 / 1080.).round() as i32,
                        (clip.w * vw as f32 / 1920.).round() as i32,
                        (clip.h * vh as f32 / 1080.).round() as i32,
                    ]
                });
                scissor(match (clip, damage) {
                    (Some(a), Some(b)) => Some(intersect(a, b)),
                    (a, b) => a.or(b),
                });
                match &batch.kind {
                    BatchKind::Paint {
                        texture,
                        start,
                        count,
                        backdrop,
                        outgoing,
                    } => {
                        if !paint_bound {
                            gl::UseProgram(self.program);
                            gl::BindBuffer(gl::ARRAY_BUFFER, self.buffer);
                            let mut offset = 0;
                            for (index, size) in [2, 2, 4, 4, 2].into_iter().enumerate() {
                                gl::EnableVertexAttribArray(index as u32);
                                gl::VertexAttribPointer(
                                    index as u32,
                                    size,
                                    gl::FLOAT,
                                    gl::FALSE,
                                    56,
                                    (offset * size_of::<f32>()) as *const _,
                                );
                                offset += size as usize;
                            }
                            paint_bound = true;
                        }
                        if let Some(outgoing) = outgoing {
                            gl::ActiveTexture(gl::TEXTURE1);
                            gl::BindTexture(gl::TEXTURE_2D, *outgoing);
                            gl::ActiveTexture(gl::TEXTURE0);
                        }
                        gl::BindTexture(gl::TEXTURE_2D, *texture);
                        gl::Uniform1i(
                            gl::GetUniformLocation(self.program, c"u_app".as_ptr()),
                            i32::from(backdrop.is_some()),
                        );
                        if let Some(stops) = backdrop {
                            for (name, rgb) in
                                [(c"u_app_top", stops[0]), (c"u_app_bottom", stops[1])]
                            {
                                gl::Uniform3f(
                                    gl::GetUniformLocation(self.program, name.as_ptr()),
                                    rgb[0] as f32 / 255.,
                                    rgb[1] as f32 / 255.,
                                    rgb[2] as f32 / 255.,
                                );
                            }
                        }
                        gl::DrawArrays(gl::TRIANGLES, *start as i32, *count as i32);
                    }
                    BatchKind::Vector { path, rect, tint } => {
                        let Some(mesh) = self.vectors.get(path).and_then(Option::as_ref) else {
                            continue;
                        };
                        self.draw_mesh(
                            self.vector_program,
                            mesh,
                            [rect.w / mesh.width, rect.h / mesh.height, rect.x, rect.y],
                            *tint,
                            0.,
                        );
                        paint_bound = false;
                    }
                    BatchKind::Text { key, origin, tint } => {
                        let Some(mesh) = self.text_runs.get(key).and_then(|run| run.mesh.as_ref())
                        else {
                            continue;
                        };
                        let size = f32::from_bits(key.layout[0]) * vh as f32 / 1080.;
                        self.draw_mesh(
                            self.text_program,
                            mesh,
                            [1., 1., origin[0], origin[1]],
                            *tint,
                            embolden(size, self.text_embolden),
                        );
                        paint_bound = false;
                    }
                }
            }
            gl::Disable(gl::SCISSOR_TEST);
        }
        // Keep the current scene, evict old full-resolution wallpapers before memory grows.
        let mut bytes: usize = self.textures.values().map(|t| t.bytes).sum();
        if bytes > TEXTURE_BUDGET {
            let mut old: Vec<_> = self
                .textures
                .iter()
                .filter(|(_, t)| t.frame != self.frame)
                .map(|(path, t)| (path.clone(), t.frame))
                .collect();
            old.sort_by_key(|(_, frame)| *frame);
            for (path, _) in old {
                if bytes <= TEXTURE_BUDGET {
                    break;
                }
                if let Some(t) = self.textures.remove(&path) {
                    bytes -= t.bytes;
                }
            }
        }
        self.trim_text_geometry();
        self.stats.vector_meshes = self.vectors.values().flatten().count();
        self.stats.vector_bytes = self.vectors.values().flatten().map(|mesh| mesh.bytes).sum();
        self.stats.artwork_bytes = bytes + self.stats.vector_bytes;
        self.stats.draw_calls = self.batches.len();
        self.stats.cpu_submission_ms = started.elapsed().as_secs_f64() * 1000.;
    }

    /// Draws a mesh with the vector or text program; `transform` is [x scale, y scale, x offset,
    /// y offset] from mesh units to logical pixels.
    fn draw_mesh(
        &self,
        program: GLuint,
        mesh: &vector::Mesh,
        transform: [f32; 4],
        tint: Color,
        embolden: f32,
    ) {
        let viewport = self.stats.viewport;
        // SAFETY: the renderer's GL context is current on the SDL thread (see the module docs); `mesh.buffer`
        // holds `mesh.vertices` vertices in the 9-float layout the attribute pointers describe.
        unsafe {
            gl::UseProgram(program);
            if program == self.text_program {
                gl::Uniform1f(
                    gl::GetUniformLocation(program, c"u_fringe".as_ptr()),
                    if self.stats.multisample_samples > 1 {
                        0.
                    } else {
                        0.5
                    },
                );
                gl::Uniform1f(
                    gl::GetUniformLocation(program, c"u_embolden".as_ptr()),
                    embolden,
                );
            }
            gl::BindBuffer(gl::ARRAY_BUFFER, mesh.buffer);
            gl::DisableVertexAttribArray(4);
            let mut offset = 0;
            for (index, size) in [2, 2, 4, 1].into_iter().enumerate() {
                gl::EnableVertexAttribArray(index as u32);
                gl::VertexAttribPointer(
                    index as u32,
                    size,
                    gl::FLOAT,
                    gl::FALSE,
                    36,
                    (offset * size_of::<f32>()) as *const _,
                );
                offset += size as usize;
            }
            gl::Uniform4f(
                gl::GetUniformLocation(program, c"u_transform".as_ptr()),
                transform[0],
                transform[1],
                transform[2],
                transform[3],
            );
            gl::Uniform2f(
                gl::GetUniformLocation(program, c"u_pixel_scale".as_ptr()),
                viewport.width as f32 / 1920.,
                viewport.height as f32 / 1080.,
            );
            gl::Uniform4f(
                gl::GetUniformLocation(program, c"u_tint".as_ptr()),
                tint[0],
                tint[1],
                tint[2],
                tint[3],
            );
            gl::DrawArrays(gl::TRIANGLES, 0, mesh.vertices);
        }
    }

    pub fn stats(&self) -> &RenderStats {
        &self.stats
    }

    /// Saves the bound framebuffer's `width` × `height` pixels as an image, flipping GL's
    /// bottom-up rows.
    ///
    /// # Errors
    ///
    /// Fails when the image cannot be encoded or written.
    pub fn screenshot(&self, path: &Path, width: u32, height: u32) -> Result<(), String> {
        let mut pixels = vec![0; width as usize * height as usize * 4];
        // SAFETY: the renderer's GL context is current on the SDL thread (see the module docs); `pixels` holds
        // exactly `width` × `height` RGBA pixels, so ReadPixels stays in bounds.
        unsafe {
            gl::ReadPixels(
                0,
                0,
                width as i32,
                height as i32,
                gl::RGBA,
                gl::UNSIGNED_BYTE,
                pixels.as_mut_ptr().cast(),
            );
        }
        let mut image = image::RgbaImage::from_raw(width, height, pixels)
            .ok_or("Invalid framebuffer dimensions")?;
        image::imageops::flip_vertical_in_place(&mut image);
        image.save(path).map_err(|e| e.to_string())
    }

    /// Renders the scene into an offscreen texture of `size` and saves it, independent of desktop
    /// HiDPI and window limits.
    ///
    /// # Errors
    ///
    /// Fails when `size` exceeds the GPU's texture or viewport limits, the framebuffer is
    /// incomplete or the image cannot be saved.
    pub fn capture(
        &mut self,
        commands: &[Draw],
        path: &Path,
        size: Size,
    ) -> Result<RenderStats, String> {
        if size.width > self.info.max_texture_size
            || size.height > self.info.max_texture_size
            || size.width > self.info.max_viewport[0] as u32
            || size.height > self.info.max_viewport[1] as u32
        {
            return Err(format!(
                "Capture {}x{} exceeds GPU limits: {}px texture, {:?} viewport",
                size.width, size.height, self.info.max_texture_size, self.info.max_viewport
            ));
        }
        let image = texture(size.width, size.height, &[]);
        let mut framebuffer = 0;
        // SAFETY: the renderer's GL context is current on the SDL thread (see the module docs); `framebuffer` is a
        // live out pointer and `image` stays alive until this function returns.
        unsafe {
            gl::GenFramebuffers(1, &mut framebuffer);
            gl::BindFramebuffer(gl::FRAMEBUFFER, framebuffer);
            gl::FramebufferTexture2D(
                gl::FRAMEBUFFER,
                gl::COLOR_ATTACHMENT0,
                gl::TEXTURE_2D,
                image.id,
                0,
            );
            if gl::CheckFramebufferStatus(gl::FRAMEBUFFER) != gl::FRAMEBUFFER_COMPLETE {
                gl::BindFramebuffer(gl::FRAMEBUFFER, 0);
                gl::DeleteFramebuffers(1, &framebuffer);
                return Err("Screenshot framebuffer is incomplete".into());
            }
        }
        self.render(commands, size.width, size.height, None);
        let stats = self.stats.clone();
        let result = self.screenshot(path, size.width, size.height);
        // SAFETY: the context is current; this restores the window framebuffer and deletes the one
        // created above.
        unsafe {
            gl::BindFramebuffer(gl::FRAMEBUFFER, 0);
            gl::DeleteFramebuffers(1, &framebuffer);
        }
        result.map(|_| stats)
    }

    /// Verification aid (ASTRO_DAMAGE_VERIFY=1): compares the window's back buffer after a
    /// partial redraw with the same scene drawn in full offscreen at the window's sample count.
    /// Returns the pixels differing by more than 16 in a channel, and the largest difference.
    pub fn verify_window(
        &mut self,
        commands: &[Draw],
        width: u32,
        height: u32,
        save: Option<&Path>,
    ) -> (usize, u8) {
        let (w, h) = (width as i32, height as i32);
        let length = width as usize * height as usize * 4;
        let mut window = vec![0u8; length];
        let mut full = vec![0u8; length];
        let samples = self.stats.multisample_samples as i32;
        let stats = self.stats.clone();
        let mut framebuffers = [0; 2];
        let mut renderbuffers = [0; 2];
        // SAFETY: the renderer's GL context is current on the SDL thread (see the module docs); `window` holds
        // `width` × `height` RGBA pixels and the name arrays hold two entries each.
        unsafe {
            gl::BindFramebuffer(gl::FRAMEBUFFER, 0);
            gl::ReadPixels(
                0,
                0,
                w,
                h,
                gl::RGBA,
                gl::UNSIGNED_BYTE,
                window.as_mut_ptr().cast(),
            );
            gl::GenFramebuffers(2, framebuffers.as_mut_ptr());
            gl::GenRenderbuffers(2, renderbuffers.as_mut_ptr());
            gl::BindRenderbuffer(gl::RENDERBUFFER, renderbuffers[0]);
            if samples > 1 {
                gl::RenderbufferStorageMultisample(gl::RENDERBUFFER, samples, gl::RGBA8, w, h);
            } else {
                gl::RenderbufferStorage(gl::RENDERBUFFER, gl::RGBA8, w, h);
            }
            gl::BindRenderbuffer(gl::RENDERBUFFER, renderbuffers[1]);
            gl::RenderbufferStorage(gl::RENDERBUFFER, gl::RGBA8, w, h);
            for (framebuffer, renderbuffer) in framebuffers.into_iter().zip(renderbuffers) {
                gl::BindFramebuffer(gl::FRAMEBUFFER, framebuffer);
                gl::FramebufferRenderbuffer(
                    gl::FRAMEBUFFER,
                    gl::COLOR_ATTACHMENT0,
                    gl::RENDERBUFFER,
                    renderbuffer,
                );
            }
            gl::BindFramebuffer(gl::FRAMEBUFFER, framebuffers[0]);
        }
        self.render(commands, width, height, None);
        // SAFETY: the context is current; `full` holds `width` × `height` RGBA pixels, and the
        // framebuffers and renderbuffers created above are deleted after use.
        unsafe {
            // Resolve a multisampled render into the single-sample buffer ReadPixels can read.
            gl::BindFramebuffer(gl::READ_FRAMEBUFFER, framebuffers[0]);
            gl::BindFramebuffer(gl::DRAW_FRAMEBUFFER, framebuffers[1]);
            gl::BlitFramebuffer(0, 0, w, h, 0, 0, w, h, gl::COLOR_BUFFER_BIT, gl::NEAREST);
            gl::BindFramebuffer(gl::FRAMEBUFFER, framebuffers[1]);
            gl::ReadPixels(
                0,
                0,
                w,
                h,
                gl::RGBA,
                gl::UNSIGNED_BYTE,
                full.as_mut_ptr().cast(),
            );
            gl::BindFramebuffer(gl::FRAMEBUFFER, 0);
            gl::DeleteFramebuffers(2, framebuffers.as_ptr());
            gl::DeleteRenderbuffers(2, renderbuffers.as_ptr());
        }
        self.stats = stats;
        let mut differing = 0;
        let mut largest = 0;
        for (a, b) in window.chunks_exact(4).zip(full.chunks_exact(4)) {
            let difference = (0..3).map(|i| a[i].abs_diff(b[i])).max().unwrap_or(0);
            largest = largest.max(difference);
            differing += usize::from(difference > 16);
        }
        if let Some(stem) = save.filter(|_| differing > 0) {
            for (name, pixels) in [("window", window), ("full", full)] {
                if let Some(image) = image::RgbaImage::from_raw(width, height, pixels) {
                    let image = image::imageops::flip_vertical(&image);
                    let _ = image.save(stem.with_extension(format!("{name}.png")));
                }
            }
        }
        (differing, largest)
    }
}

/// Returns whether `c` is Hangul, which the Korean font covers.
///
/// Noto Sans SC (font-cjk) has no Hangul syllables; these go to font-kr instead.
/// Keep in sync with `is_hangul()` in `tools/font-weights.py`, which builds font-kr.
fn is_hangul(c: char) -> bool {
    matches!(
        c,
        '\u{1100}'..='\u{11FF}'
            | '\u{3130}'..='\u{318F}'
            | '\u{A960}'..='\u{A97F}'
            | '\u{AC00}'..='\u{D7A3}'
            | '\u{D7B0}'..='\u{D7FF}'
    )
}

/// Loads a font by ID: `font-<weight>`, `font-display-<weight>` (+1000), `font-cjk-<weight>`
/// (+2000) or `font-kr-<weight>` (+3000), all `.ttf`.
///
/// # Errors
///
/// Fails when the file is missing or not a valid font.
fn load_font(directory: &Path, id: u16) -> Result<Font, String> {
    let name = if id >= 3000 {
        format!("font-kr-{}.ttf", id - 3000)
    } else if id >= 2000 {
        format!("font-cjk-{}.ttf", id - 2000)
    } else if id >= 1000 {
        format!("font-display-{}.ttf", id - 1000)
    } else {
        format!("font-{id}.ttf")
    };
    let path = directory.join(&name);
    let started = Instant::now();
    let bytes = std::fs::read(&path)
        .map_err(|e| format!("Font {}: {e}. Regenerate native assets.", path.display()))?;
    let font = Font::from_bytes(bytes).map_err(|e| format!("Font {}: {e}", path.display()))?;
    if cfg!(feature = "webos") {
        eprintln!(
            "Astro font: {name} loaded in {} ms",
            started.elapsed().as_millis()
        );
    }
    Ok(font)
}

impl Drop for Renderer {
    fn drop(&mut self) {
        // SAFETY: the renderer is dropped on the SDL thread with its context current, and these
        // names were created in `Renderer::new`.
        unsafe {
            gl::DeleteBuffers(1, &self.buffer);
            gl::DeleteProgram(self.program);
            gl::DeleteProgram(self.vector_program);
            gl::DeleteProgram(self.text_program);
            if self.vao != 0 {
                gl::DeleteVertexArrays(1, &self.vao);
            }
        }
    }
}

/// Creates an RGBA texture; empty `pixels` leaves its contents undefined, for a render target.
fn texture(width: u32, height: u32, pixels: &[u8]) -> Texture {
    texture_with(width, height, gl::RGBA, pixels)
}

/// `format` is RGBA or RGB; UNPACK_ALIGNMENT is 1, so RGB rows of any width are tightly packed.
fn texture_with(width: u32, height: u32, format: GLenum, pixels: &[u8]) -> Texture {
    let channels = if format == gl::RGB { 3 } else { 4 };
    let mut id = 0;
    // SAFETY: called on the SDL thread with the context current; `pixels` is empty (no upload) or
    // holds `width` × `height` tightly packed pixels of `format`.
    unsafe {
        gl::GenTextures(1, &mut id);
        gl::BindTexture(gl::TEXTURE_2D, id);
        gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR as i32);
        gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as i32);
        gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE as i32);
        gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE as i32);
        gl::TexImage2D(
            gl::TEXTURE_2D,
            0,
            format as i32,
            width as i32,
            height as i32,
            0,
            format,
            gl::UNSIGNED_BYTE,
            if pixels.is_empty() {
                std::ptr::null()
            } else {
                pixels.as_ptr().cast()
            },
        );
    }
    Texture {
        id,
        width,
        height,
        bytes: width as usize * height as usize * channels,
        frame: 0,
        opaque: format == gl::RGB,
    }
}

/// Whether any part of `rect` falls on the logical screen and inside `clip`.
fn visible(rect: &Rect, clip: Option<Rect>) -> bool {
    let overlaps = |a: &Rect, b: &Rect| {
        a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
    };
    rect.w > 0.
        && rect.h > 0.
        && overlaps(rect, &Rect::new(0., 0., 1920., 1080.))
        && clip.is_none_or(|clip| overlaps(rect, &clip))
}

/// Returns a GL string such as the vendor, or `unknown`.
fn gl_string(name: GLenum) -> String {
    // SAFETY: called on the SDL thread with the context current; a non-null GetString result is
    // a static NUL-terminated string.
    unsafe {
        let value = gl::GetString(name);
        if value.is_null() {
            "unknown".into()
        } else {
            CStr::from_ptr(value.cast()).to_string_lossy().into_owned()
        }
    }
}

/// Compiles one shader stage.
///
/// # Errors
///
/// Fails with the driver's log when compilation fails, or when the source contains a NUL byte.
fn compile(kind: GLenum, source: &str) -> Result<GLuint, String> {
    let source = CString::new(source).map_err(|e| e.to_string())?;
    // SAFETY: called on the SDL thread with the context current; `source` is NUL-terminated and
    // outlives ShaderSource, and the log buffer's length is passed with it.
    unsafe {
        let shader = gl::CreateShader(kind);
        gl::ShaderSource(shader, 1, &source.as_ptr(), std::ptr::null());
        gl::CompileShader(shader);
        let mut success = 0;
        gl::GetShaderiv(shader, gl::COMPILE_STATUS, &mut success);
        if success == 0 {
            let mut log = vec![0u8; 4096];
            let mut len = 0;
            gl::GetShaderInfoLog(shader, log.len() as i32, &mut len, log.as_mut_ptr().cast());
            gl::DeleteShader(shader);
            return Err(format!(
                "Shader compilation: {}",
                String::from_utf8_lossy(&log[..len as usize])
            ));
        }
        Ok(shader)
    }
}

/// Compiles and links a program, binding `attributes` to locations 0, 1, … in order.
///
/// # Errors
///
/// Fails with the driver's log when a stage does not compile or the program does not link.
fn link(vertex: &str, fragment: &str, attributes: &[&CStr]) -> Result<GLuint, String> {
    let vertex = compile(gl::VERTEX_SHADER, vertex)?;
    let fragment = match compile(gl::FRAGMENT_SHADER, fragment) {
        Ok(fragment) => fragment,
        Err(error) => {
            // SAFETY: the context is current and `vertex` was compiled above.
            unsafe {
                gl::DeleteShader(vertex);
            }
            return Err(error);
        }
    };
    // SAFETY: called on the SDL thread with the context current; the shaders were compiled
    // above, the attribute names are NUL-terminated, and the log buffer's length is passed with
    // it.
    unsafe {
        let program = gl::CreateProgram();
        gl::AttachShader(program, vertex);
        gl::AttachShader(program, fragment);
        for (index, name) in attributes.iter().enumerate() {
            gl::BindAttribLocation(program, index as u32, name.as_ptr());
        }
        gl::LinkProgram(program);
        gl::DeleteShader(vertex);
        gl::DeleteShader(fragment);
        let mut success = 0;
        gl::GetProgramiv(program, gl::LINK_STATUS, &mut success);
        if success == 0 {
            let mut log = vec![0u8; 4096];
            let mut len = 0;
            gl::GetProgramInfoLog(program, log.len() as i32, &mut len, log.as_mut_ptr().cast());
            gl::DeleteProgram(program);
            return Err(format!(
                "Shader linking: {}",
                String::from_utf8_lossy(&log[..len as usize])
            ));
        }
        Ok(program)
    }
}
