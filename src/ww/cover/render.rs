//! Draws a cover: the window rendered small into a texture, blurred with niri's blur, scaled
//! back over the window, dimmed, and the card's lines on top.
//!
//! Cheap on purpose: the window is rendered at a quarter of the output scale, so the blur works
//! on a sixteenth of the pixels; it reblurs only when the window's content changed. Each card
//! line is a texture of its own, drawn again only when its text or size changes: once a second
//! for the countdown, once a minute for the clock.

use std::cell::RefCell;
use std::time::Instant;

use pangocairo::cairo::{self, ImageSurface};
use pangocairo::pango::{Alignment, EllipsizeMode, FontDescription, Weight};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::{Element as _, Kind};
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::utils::CommitCounter;
use smithay::backend::renderer::{Offscreen as _, Renderer as _, Texture as _};
use smithay::utils::{Logical, Point, Rectangle, Scale, Size, Transform};

use super::Look;
use crate::layout::LayoutElementRenderElement;
use crate::render_helpers::blur::{Blur, BlurOptions};
use crate::render_helpers::memory::MemoryBuffer;
use crate::render_helpers::offscreen::{OffscreenBuffer, OffscreenData};
use crate::render_helpers::primary_gpu_texture::PrimaryGpuTextureRenderElement;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};

/// The window is rendered at this part of the output scale before the blur.
const DOWNSCALE: f64 = 0.25;
/// Passes of niri's dual-kawase blur on the small texture, and its spread.
const BLUR: BlurOptions = BlurOptions {
    passes: 2,
    offset: 2.,
};
/// The warm dark over the blur, and how much of it.
const DIM_COLOR: [f32; 4] = [
    0x2A as f32 / 255.,
    0x1D as f32 / 255.,
    0x16 as f32 / 255.,
    1.,
];
const DIM_ALPHA: f32 = 0.65;
/// The card is laid out on a window this tall and scaled to the real one.
const LAYOUT_HEIGHT: f64 = 1080.;
/// Lines may take this part of the window's width; longer ones end in an ellipsis.
const LINE_WIDTH_PART: f64 = 0.9;
const FONT_FAMILY: &str = "sans-serif";

/// A line of the card: where its middle sits and how it looks, on a 1080 high window.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Style {
    y: f64,
    font_px: f64,
    bold: bool,
    color: [f64; 3],
}

const fn rgb(hex: u32) -> [f64; 3] {
    [
        ((hex >> 16) & 0xff) as f64 / 255.,
        ((hex >> 8) & 0xff) as f64 / 255.,
        (hex & 0xff) as f64 / 255.,
    ]
}

const TITLE: Style = Style {
    y: 380.,
    font_px: 84.,
    bold: true,
    color: rgb(0xF5E8D7),
};
const COUNTDOWN: Style = Style {
    y: 520.,
    font_px: 64.,
    bold: false,
    color: rgb(0xF5E8D7),
};
const CLOCK: Style = Style {
    y: 720.,
    font_px: 170.,
    bold: true,
    color: rgb(0xE8C49C),
};
const FOOTER: Style = Style {
    y: 900.,
    font_px: 44.,
    bold: false,
    color: rgb(0xD8C8B4),
};

/// An element of a cover, front to back.
pub enum CoverElement {
    Texture(PrimaryGpuTextureRenderElement),
    SolidColor(SolidColorRenderElement),
}

/// What a tile keeps to draw its window's cover.
#[derive(Debug, Default)]
pub struct CoverRender {
    offscreen: OffscreenBuffer,
    inner: RefCell<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    blur: Option<Blur>,
    /// The blurred window, and the offscreen commit it shows.
    blurred: Option<(TextureBuffer<GlesTexture>, CommitCounter)>,
    dim: Option<SolidColorBuffer>,
    lines: Vec<Line>,
}

#[derive(Debug)]
struct Line {
    text: String,
    style: Style,
    /// Font size and width limit in physical pixels.
    px: (i32, i32),
    buffer: TextureBuffer<GlesTexture>,
}

/// The card's lines at `now`.
fn lines(look: &Look, now: Instant) -> Vec<(String, Style)> {
    let mut lines = vec![(look.card.title.clone(), TITLE)];
    if let Some(countdown) = look.countdown(now) {
        lines.push((countdown, COUNTDOWN));
    }
    if look.card.clock {
        if let Some(clock) = local_clock() {
            lines.push((clock, CLOCK));
        }
    }
    if let Some(footer) = &look.card.footer {
        lines.push((footer.clone(), FOOTER));
    }
    lines
}

/// `HH:MM` in the local time zone.
fn local_clock() -> Option<String> {
    let now = pangocairo::glib::DateTime::now_local().ok()?;
    Some(now.format("%H:%M").ok()?.to_string())
}

impl CoverRender {
    /// Draws the cover of a window at `loc`, `size` big; `window` renders the window's own
    /// elements at a scale, at (0, 0). Answers the offscreen data for the window's frame
    /// callbacks.
    #[expect(
        clippy::too_many_arguments,
        reason = "a tile's render arguments, as niri's"
    )]
    pub fn render(
        &self,
        renderer: &mut GlesRenderer,
        look: &Look,
        loc: Point<f64, Logical>,
        size: Size<f64, Logical>,
        scale: Scale<f64>,
        alpha: f32,
        window: impl FnOnce(
            &mut GlesRenderer,
            Scale<f64>,
        ) -> Vec<LayoutElementRenderElement<GlesRenderer>>,
        push: &mut dyn FnMut(CoverElement),
    ) -> Option<OffscreenData> {
        let _span = tracy_client::span!("CoverRender::render");

        let mut inner = self.inner.borrow_mut();
        let inner = &mut *inner;

        let now = Instant::now();
        let k = size.h / LAYOUT_HEIGHT;
        let width_px = (size.w * scale.x * LINE_WIDTH_PART).round() as i32;
        let mut kept = Vec::new();
        for (text, style) in lines(look, now) {
            let px = ((style.font_px * k * scale.y).round() as i32, width_px);
            let found = inner
                .lines
                .iter()
                .position(|l| l.text == text && l.style == style && l.px == px);
            let line = match found {
                Some(i) => inner.lines.swap_remove(i),
                None => match render_line(renderer, &text, style, px, scale) {
                    Ok(buffer) => Line {
                        text,
                        style,
                        px,
                        buffer,
                    },
                    Err(err) => {
                        warn!("ww: error drawing a cover's card line: {err:?}");
                        continue;
                    }
                },
            };
            let line_size = line.buffer.logical_size();
            let at =
                loc + Point::from(((size.w - line_size.w) / 2., style.y * k - line_size.h / 2.));
            let elem = TextureRenderElement::from_texture_buffer(
                line.buffer.clone(),
                at,
                alpha,
                None,
                None,
                Kind::Unspecified,
            );
            push(CoverElement::Texture(PrimaryGpuTextureRenderElement(elem)));
            kept.push(line);
        }
        inner.lines = kept;

        let blurred = self.blurred(inner, renderer, scale, window);
        // Without the blur nothing of the window may show.
        let dim_alpha = if blurred.is_some() { DIM_ALPHA } else { 1. };
        let dim = inner
            .dim
            .get_or_insert_with(|| SolidColorBuffer::new(size, DIM_COLOR));
        dim.resize(size);
        push(CoverElement::SolidColor(
            SolidColorRenderElement::from_buffer(dim, loc, dim_alpha * alpha, Kind::Unspecified),
        ));

        let Blurred {
            buffer,
            src,
            mut data,
        } = blurred?;
        let elem = TextureRenderElement::from_texture_buffer(
            buffer,
            loc + src.loc,
            alpha,
            Some(Rectangle::from_size(src.size)),
            None,
            Kind::Unspecified,
        );
        data.id = elem.id().clone();
        push(CoverElement::Texture(PrimaryGpuTextureRenderElement(elem)));
        Some(data)
    }

    /// The window, rendered small and blurred; reblurred only when it changed.
    fn blurred(
        &self,
        inner: &mut Inner,
        renderer: &mut GlesRenderer,
        scale: Scale<f64>,
        window: impl FnOnce(
            &mut GlesRenderer,
            Scale<f64>,
        ) -> Vec<LayoutElementRenderElement<GlesRenderer>>,
    ) -> Option<Blurred> {
        let small = Scale::from(scale.x * DOWNSCALE);
        let elements = window(renderer, small);
        let (elem, _sync, data) = self
            .offscreen
            .render(renderer, small, &elements)
            .map_err(|err| warn!("ww: error rendering a covered window: {err:?}"))
            .ok()?;
        let commit = elem.current_commit();
        // The texture may be larger than what the window drew into it.
        let src = Rectangle::new(elem.offset(), elem.logical_size());

        if let Some((buffer, seen)) = &inner.blurred {
            if *seen == commit && buffer.texture().size() == elem.texture().size() {
                let buffer = buffer.clone();
                return Some(Blurred { buffer, src, data });
            }
        }

        if inner
            .blur
            .as_ref()
            .is_some_and(|b| b.context_id() != renderer.context_id())
        {
            inner.blur = None;
            inner.blurred = None;
        }
        if inner.blur.is_none() {
            inner.blur = Blur::new(renderer);
        }
        let blur = inner.blur.as_mut()?;
        let source = elem.texture();
        let res = blur
            .prepare_textures(
                |fourcc, size| renderer.create_buffer(fourcc, size),
                source,
                BLUR,
            )
            .and_then(|()| blur.render(renderer, source, BLUR));
        let texture = match res {
            Ok(texture) => texture,
            Err(err) => {
                warn!("ww: error blurring a covered window: {err:?}");
                return None;
            }
        };

        let buffer = match inner.blurred.take() {
            Some((mut buffer, _)) if buffer.texture().tex_id() == texture.tex_id() => {
                buffer.bump_commit();
                buffer
            }
            _ => {
                TextureBuffer::from_texture(renderer, texture, small, Transform::Normal, Vec::new())
            }
        };
        inner.blurred = Some((buffer.clone(), commit));
        Some(Blurred { buffer, src, data })
    }
}

/// The blurred window: the texture, the part of it the window drew (in logical coordinates
/// from the window's top left) and the offscreen data of the render.
struct Blurred {
    buffer: TextureBuffer<GlesTexture>,
    src: Rectangle<f64, Logical>,
    data: OffscreenData,
}

/// A card line as a texture, centred in its own width.
fn render_line(
    renderer: &mut GlesRenderer,
    text: &str,
    style: Style,
    (size_px, width_px): (i32, i32),
    scale: Scale<f64>,
) -> anyhow::Result<TextureBuffer<GlesTexture>> {
    let _span = tracy_client::span!("cover::render_line");

    let mut font = FontDescription::from_string(FONT_FAMILY);
    font.set_absolute_size(f64::from(size_px) * f64::from(pangocairo::pango::SCALE));
    if style.bold {
        font.set_weight(Weight::Bold);
    }
    let layout_for = |cr: &cairo::Context| {
        let layout = pangocairo::functions::create_layout(cr);
        layout.context().set_round_glyph_positions(false);
        layout.set_font_description(Some(&font));
        layout.set_alignment(Alignment::Center);
        layout.set_width(width_px * pangocairo::pango::SCALE);
        layout.set_ellipsize(EllipsizeMode::End);
        layout.set_text(text);
        layout
    };

    let surface = ImageSurface::create(cairo::Format::ARgb32, 0, 0)?;
    let cr = cairo::Context::new(&surface)?;
    let (_, height) = layout_for(&cr).pixel_size();
    let (width, height) = (width_px.max(1), height.max(1));

    let surface = ImageSurface::create(cairo::Format::ARgb32, width, height)?;
    let cr = cairo::Context::new(&surface)?;
    let layout = layout_for(&cr);
    let [r, g, b] = style.color;
    cr.set_source_rgb(r, g, b);
    pangocairo::functions::show_layout(&cr, &layout);
    drop(cr);

    let data = surface.take_data()?;
    let buffer = MemoryBuffer::new(
        data.to_vec(),
        Fourcc::Argb8888,
        (width, height),
        scale,
        Transform::Normal,
    );
    Ok(TextureBuffer::from_memory_buffer(renderer, &buffer)?)
}
