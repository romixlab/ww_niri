//! Draws overlay items on a surface: a window's tile or an output.
//!
//! An item is its content's lines over its backdrop: a dim, under it a blur of what is under its
//! box (niri's framebuffer effect), and under both the fill: a colour or an image scaled to cover
//! the box. On a window, the topmost item at `Fill` with a blur is a
//! cover: the window is rendered small into a texture, blurred with niri's blur and scaled back
//! instead of the window, so nothing of it shows; it reblurs only when the window's content
//! changed. Cheap on purpose: the copy has a sixteenth of the pixels.
//!
//! Each line is a texture of its own, kept per item and drawn again only when its text or size
//! changes: once a second for a countdown or a progress, once a minute for a clock.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

use pangocairo::cairo::{self, ImageSurface};
use pangocairo::pango::{Alignment, EllipsizeMode, FontDescription, Weight};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::{Element as _, Kind};
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::utils::CommitCounter;
use smithay::backend::renderer::{Offscreen as _, Renderer as _, Texture as _};
use smithay::utils::{Logical, Point, Rectangle, Scale, Size, Transform};
use ww_niri_api::{Anchor, At, ContentOwned, FillOwned, TextSize, METER_BLOCKS};

use super::image::Image;
use super::Look;
use crate::layout::LayoutElementRenderElement;
use crate::render_helpers::background_effect::RenderParams;
use crate::render_helpers::blur::{Blur, BlurOptions};
use crate::render_helpers::framebuffer_effect::{FramebufferEffect, FramebufferEffectElement};
use crate::render_helpers::memory::MemoryBuffer;
use crate::render_helpers::offscreen::{OffscreenBuffer, OffscreenData};
use crate::render_helpers::primary_gpu_texture::PrimaryGpuTextureRenderElement;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::RenderTarget;

/// A cover's window copy is rendered at this part of the output scale before the blur.
const DOWNSCALE: f64 = 0.25;
/// The spread of each blur pass.
const BLUR_OFFSET: f64 = 2.;
/// The warm dark over the blur.
const DIM_COLOR: [f32; 4] = [
    0x2A as f32 / 255.,
    0x1D as f32 / 255.,
    0x16 as f32 / 255.,
    1.,
];
/// Content is laid out on a surface this tall and scaled to the real one.
const LAYOUT_HEIGHT: f64 = 1080.;
/// Lines may take this part of the surface's width; longer ones end in an ellipsis.
const LINE_WIDTH_PART: f64 = 0.9;
/// Space around an anchored item's lines, and between them, on a 1080 high surface.
const PADDING: f64 = 28.;
const LINE_GAP: f64 = 12.;
const FONT_FAMILY: &str = "sans-serif";

/// How a line looks, on a 1080 high surface.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Style {
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

const TEXT_COLOR: [f64; 3] = rgb(0xF5E8D7);
const CLOCK_COLOR: [f64; 3] = rgb(0xE8C49C);
const QUIET_COLOR: [f64; 3] = rgb(0xD8C8B4);

const fn style(font_px: f64, bold: bool, color: [f64; 3]) -> Style {
    Style {
        font_px,
        bold,
        color,
    }
}

/// A card's lines: where each one's middle sits on a 1080 high surface, and its style.
const CARD_TITLE: (f64, Style) = (380., style(84., true, TEXT_COLOR));
const CARD_COUNTDOWN: (f64, Style) = (520., style(64., false, TEXT_COLOR));
const CARD_CLOCK: (f64, Style) = (720., style(170., true, CLOCK_COLOR));
const CARD_FOOTER: (f64, Style) = (900., style(44., false, QUIET_COLOR));

fn text_style(size: TextSize) -> Style {
    let font_px = match size {
        TextSize::Small => 36.,
        TextSize::Large => 96.,
        // A size from a newer client: the middle one.
        TextSize::Medium | TextSize::Unknown(_) => 56.,
    };
    style(font_px, false, TEXT_COLOR)
}

fn clock_style(size: TextSize) -> Style {
    let font_px = match size {
        TextSize::Small => 56.,
        TextSize::Large => 170.,
        TextSize::Medium | TextSize::Unknown(_) => 110.,
    };
    style(font_px, true, CLOCK_COLOR)
}

const NOTICE_TITLE: Style = style(56., true, TEXT_COLOR);
const NOTICE_BODY: Style = style(40., false, QUIET_COLOR);
const PROGRESS_ELAPSED: Style = style(48., true, CLOCK_COLOR);
const METER_BAR: Style = style(44., false, CLOCK_COLOR);

/// An element of the overlay, front to back.
pub enum OverlayElement {
    Texture(PrimaryGpuTextureRenderElement),
    SolidColor(SolidColorRenderElement),
    Blur(FramebufferEffectElement),
}

/// What a tile or an output keeps to draw its items between frames.
#[derive(Debug, Default)]
pub struct OverlayRender {
    offscreen: OffscreenBuffer,
    inner: RefCell<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    blur: Option<Blur>,
    /// The window's blurred copy, the offscreen commit and the passes it shows.
    blurred: Option<(TextureBuffer<GlesTexture>, CommitCounter, u8)>,
    /// Under a cover whose blur failed.
    opaque: Option<SolidColorBuffer>,
    items: HashMap<u64, ItemRender>,
}

/// What one item keeps between frames.
#[derive(Debug)]
struct ItemRender {
    lines: Vec<Line>,
    dim: SolidColorBuffer,
    effect: FramebufferEffect,
    color: SolidColorBuffer,
    /// The fill's image and its texture.
    image: Option<(Rc<Image>, TextureBuffer<GlesTexture>)>,
}

impl Default for ItemRender {
    fn default() -> Self {
        Self {
            lines: Vec::new(),
            dim: SolidColorBuffer::new(Size::default(), DIM_COLOR),
            effect: FramebufferEffect::new(),
            color: SolidColorBuffer::new(Size::default(), [0., 0., 0., 1.]),
            image: None,
        }
    }
}

#[derive(Debug)]
struct Line {
    text: String,
    style: Style,
    /// Font size and width limit in physical pixels.
    px: (i32, i32),
    buffer: TextureBuffer<GlesTexture>,
}

/// A line to draw: its text, its style, and on a card where its middle sits.
struct Want {
    text: String,
    style: Style,
    card_y: Option<f64>,
}

impl Want {
    fn new(text: String, style: Style) -> Self {
        Self {
            text,
            style,
            card_y: None,
        }
    }
}

/// The item's lines at `now`.
fn wants(look: &Look, now: Instant) -> Vec<Want> {
    let card = |text: String, (y, style): (f64, Style)| Want {
        text,
        style,
        card_y: Some(y),
    };
    let mut wants = Vec::new();
    match &look.content {
        ContentOwned::Card(c) => {
            wants.push(card(c.title.clone(), CARD_TITLE));
            if let Some(countdown) = look.countdown(now) {
                wants.push(card(countdown, CARD_COUNTDOWN));
            }
            if c.clock {
                wants.extend(local_clock().map(|t| card(t, CARD_CLOCK)));
            }
            if let Some(footer) = &c.footer {
                wants.push(card(footer.clone(), CARD_FOOTER));
            }
        }
        ContentOwned::Text { text, size } => wants.push(Want::new(text.clone(), text_style(*size))),
        ContentOwned::Clock { size } => {
            wants.extend(local_clock().map(|t| Want::new(t, clock_style(*size))));
        }
        ContentOwned::Notice { title, body } => {
            wants.push(Want::new(title.clone(), NOTICE_TITLE));
            wants.extend(body.clone().map(|b| Want::new(b, NOTICE_BODY)));
        }
        ContentOwned::Progress { title, detail } => {
            wants.push(Want::new(title.clone(), NOTICE_TITLE));
            wants.extend(detail.clone().map(|d| Want::new(d, NOTICE_BODY)));
            wants.extend(look.elapsed(now).map(|t| Want::new(t, PROGRESS_ELAPSED)));
        }
        ContentOwned::Meter { label, percent } => {
            wants.push(Want::new(label.clone(), NOTICE_TITLE));
            wants.push(Want::new(meter_bar(*percent), METER_BAR));
        }
        // Refused at `put`.
        ContentOwned::Unknown(_) => {}
    }
    wants
}

/// A meter's bar of [METER_BLOCKS] blocks, the filled ones rounded to the nearest, then the
/// percent.
fn meter_bar(percent: u16) -> String {
    let filled = (percent.min(100) * METER_BLOCKS + 50) / 100;
    let mut bar = "\u{25A0}".repeat(usize::from(filled));
    bar.push_str(&"\u{25A1}".repeat(usize::from(METER_BLOCKS - filled)));
    format!("{bar}  {percent} %")
}

/// `HH:MM` in the local time zone.
fn local_clock() -> Option<String> {
    let now = pangocairo::glib::DateTime::now_local().ok()?;
    Some(now.format("%H:%M").ok()?.to_string())
}

/// What a surface's [OverlayRender::render] did.
pub struct Drawn {
    /// A cover hides the window: neither it nor its popups are drawn.
    pub covered: bool,
    /// The offscreen data of the cover's window copy, for the window's frame callbacks.
    pub data: Option<OffscreenData>,
}

/// A surface's place and scale on its output.
#[derive(Clone, Copy)]
pub struct Area {
    pub loc: Point<f64, Logical>,
    pub size: Size<f64, Logical>,
    pub scale: Scale<f64>,
    pub alpha: f32,
}

/// Renders the window's own elements at a scale, at (0, 0), for a cover's copy.
pub type WindowRender<'a> = &'a mut dyn FnMut(
    &mut GlesRenderer,
    Scale<f64>,
) -> Vec<LayoutElementRenderElement<GlesRenderer>>;

/// Marks `looks` drawn when `target` is the screen; frames for a screencast or a screenshot
/// show nobody the item.
pub fn mark_drawn(looks: &[Rc<Look>], target: RenderTarget) {
    if target == RenderTarget::Output {
        for look in looks {
            look.drawn.set(true);
        }
    }
}

impl OverlayRender {
    /// Draws `looks`, front to back, on `area`; `window` renders the window under them, `None`
    /// on an output.
    pub fn render(
        &self,
        renderer: &mut GlesRenderer,
        looks: &[Rc<Look>],
        area: Area,
        window: Option<WindowRender<'_>>,
        push: &mut dyn FnMut(OverlayElement),
    ) -> Drawn {
        let _span = tracy_client::span!("OverlayRender::render");

        let mut inner = self.inner.borrow_mut();
        let inner = &mut *inner;
        inner
            .items
            .retain(|id, _| looks.iter().any(|l| l.id == *id));

        let cover = window
            .is_some()
            .then(|| looks.iter().find(|l| l.covers()))
            .flatten()
            .map(|l| l.id);
        let now = Instant::now();
        for look in looks {
            let item = inner.items.entry(look.id).or_default();
            let is_cover = cover == Some(look.id);
            item.render(renderer, look, area, now, is_cover, push);
        }

        let (Some(cover), Some(window)) = (cover, window) else {
            return Drawn {
                covered: false,
                data: None,
            };
        };
        let passes = (looks.iter().find(|l| l.id == cover)).map_or(1, |l| l.backdrop.blur);
        let data = match self.blurred(inner, renderer, area.scale, passes, window) {
            Some(Blurred {
                buffer,
                src,
                mut data,
            }) => {
                let elem = TextureRenderElement::from_texture_buffer(
                    buffer,
                    area.loc + src.loc,
                    area.alpha,
                    Some(Rectangle::from_size(src.size)),
                    None,
                    Kind::Unspecified,
                );
                data.id = elem.id().clone();
                push(OverlayElement::Texture(PrimaryGpuTextureRenderElement(
                    elem,
                )));
                Some(data)
            }
            None => {
                // Without the blur nothing of the window may show.
                let opaque = (inner.opaque)
                    .get_or_insert_with(|| SolidColorBuffer::new(area.size, DIM_COLOR));
                opaque.resize(area.size);
                push(OverlayElement::SolidColor(
                    SolidColorRenderElement::from_buffer(
                        opaque,
                        area.loc,
                        area.alpha,
                        Kind::Unspecified,
                    ),
                ));
                None
            }
        };
        Drawn {
            covered: true,
            data,
        }
    }

    /// The window, rendered small and blurred; reblurred only when it or the passes changed.
    fn blurred(
        &self,
        inner: &mut Inner,
        renderer: &mut GlesRenderer,
        scale: Scale<f64>,
        passes: u8,
        window: WindowRender<'_>,
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

        if let Some((buffer, seen, seen_passes)) = &inner.blurred {
            if *seen == commit
                && *seen_passes == passes
                && buffer.texture().size() == elem.texture().size()
            {
                let buffer = buffer.clone();
                return Some(Blurred { buffer, src, data });
            }
        }

        if (inner.blur.as_ref()).is_some_and(|b| b.context_id() != renderer.context_id()) {
            inner.blur = None;
            inner.blurred = None;
        }
        if inner.blur.is_none() {
            inner.blur = Blur::new(renderer);
        }
        let blur = inner.blur.as_mut()?;
        let options = BlurOptions {
            passes,
            offset: BLUR_OFFSET,
        };
        let source = elem.texture();
        let res = blur
            .prepare_textures(
                |fourcc, size| renderer.create_buffer(fourcc, size),
                source,
                options,
            )
            .and_then(|()| blur.render(renderer, source, options));
        let texture = match res {
            Ok(texture) => texture,
            Err(err) => {
                warn!("ww: error blurring a covered window: {err:?}");
                return None;
            }
        };

        let buffer = match inner.blurred.take() {
            Some((mut buffer, _, _)) if buffer.texture().tex_id() == texture.tex_id() => {
                buffer.bump_commit();
                buffer
            }
            _ => {
                TextureBuffer::from_texture(renderer, texture, small, Transform::Normal, Vec::new())
            }
        };
        inner.blurred = Some((buffer.clone(), commit, passes));
        Some(Blurred { buffer, src, data })
    }
}

impl ItemRender {
    /// Pushes the item's lines, then its backdrop; a cover's blurred window copy is the
    /// caller's.
    fn render(
        &mut self,
        renderer: &mut GlesRenderer,
        look: &Look,
        area: Area,
        now: Instant,
        is_cover: bool,
        push: &mut dyn FnMut(OverlayElement),
    ) {
        let k = area.size.h / LAYOUT_HEIGHT;
        let width_px = (area.size.w * area.scale.x * LINE_WIDTH_PART).round() as i32;
        let mut kept = Vec::new();
        let mut placed = Vec::new();
        for want in wants(look, now) {
            let px = (
                (want.style.font_px * k * area.scale.y).round() as i32,
                width_px,
            );
            let found = (self.lines.iter())
                .position(|l| l.text == want.text && l.style == want.style && l.px == px);
            let line = match found {
                Some(i) => self.lines.swap_remove(i),
                None => match render_line(renderer, &want.text, want.style, px, area.scale) {
                    Ok(buffer) => Line {
                        text: want.text,
                        style: want.style,
                        px,
                        buffer,
                    },
                    Err(err) => {
                        warn!("ww: error drawing an overlay line: {err:?}");
                        continue;
                    }
                },
            };
            placed.push((line.buffer.clone(), want.card_y));
            kept.push(line);
        }
        self.lines = kept;

        let sizes: Vec<Size<f64, Logical>> = placed.iter().map(|(b, _)| b.logical_size()).collect();
        let content = stack_size(&sizes, k);
        let pad = PADDING * k;
        let boxed = match look.at {
            At::Anchor { anchor, dx, dy } => {
                let size = Size::from((content.w + 2. * pad, content.h + 2. * pad));
                let offset = Point::from((f64::from(dx.get()), f64::from(dy.get())));
                Rectangle::new(anchored(area, anchor, size) + offset, size)
            }
            // Refused at `put`.
            At::Fill | At::Unknown(_) => Rectangle::new(area.loc, area.size),
        };

        // Lines centred in the box, stacked, or at a card's places.
        let mut y = boxed.loc.y + (boxed.size.h - content.h) / 2.;
        for ((buffer, card_y), size) in placed.into_iter().zip(&sizes) {
            let x = boxed.loc.x + (boxed.size.w - size.w) / 2.;
            let top = match card_y {
                Some(card_y) => boxed.loc.y + card_y * k - size.h / 2.,
                None => y,
            };
            y += size.h + LINE_GAP * k;
            let elem = TextureRenderElement::from_texture_buffer(
                buffer,
                Point::from((x, top)),
                area.alpha,
                None,
                None,
                Kind::Unspecified,
            );
            push(OverlayElement::Texture(PrimaryGpuTextureRenderElement(
                elem,
            )));
        }

        let dim = f32::from(look.backdrop.dim) / 100.;
        if dim > 0. {
            self.dim.resize(boxed.size);
            push(OverlayElement::SolidColor(
                SolidColorRenderElement::from_buffer(
                    &self.dim,
                    boxed.loc,
                    dim * area.alpha,
                    Kind::Unspecified,
                ),
            ));
        }
        if look.backdrop.blur > 0 && !is_cover {
            let options = BlurOptions {
                passes: look.backdrop.blur,
                offset: BLUR_OFFSET,
            };
            let params = RenderParams {
                geometry: boxed,
                subregion: None,
                clip: None,
                scale: area.scale.x,
            };
            push(OverlayElement::Blur(self.effect.render(
                None,
                params,
                Some(options),
                0.,
                1.,
            )));
        }
        self.render_fill(renderer, look, boxed, area, push);
    }

    /// Pushes the fill of `boxed`: a colour, or the image scaled to cover the box, its middle
    /// in the box's middle.
    fn render_fill(
        &mut self,
        renderer: &mut GlesRenderer,
        look: &Look,
        boxed: Rectangle<f64, Logical>,
        area: Area,
        push: &mut dyn FnMut(OverlayElement),
    ) {
        match &look.backdrop.fill {
            FillOwned::Color(c) => {
                let rgb = |v: u8| f32::from(v) / 255.;
                self.color.set_color([rgb(c.r), rgb(c.g), rgb(c.b), 1.]);
                self.color.resize(boxed.size);
                push(OverlayElement::SolidColor(
                    SolidColorRenderElement::from_buffer(
                        &self.color,
                        boxed.loc,
                        rgb(c.a) * area.alpha,
                        Kind::Unspecified,
                    ),
                ));
            }
            FillOwned::Image(_) => {
                let Some(image) = &look.image else { return };
                let fresh = self
                    .image
                    .as_ref()
                    .is_some_and(|(i, _)| Rc::ptr_eq(i, image));
                if !fresh {
                    match TextureBuffer::from_memory_buffer(renderer, &image.pixels) {
                        Ok(buffer) => self.image = Some((image.clone(), buffer)),
                        Err(err) => {
                            warn!("ww: error uploading the overlay image {image:?}: {err:?}");
                            self.image = None;
                            return;
                        }
                    }
                }
                let Some((_, buffer)) = &self.image else {
                    return;
                };
                let src = cover_crop(buffer.logical_size(), boxed.size);
                let size = boxed.size.to_i32_round();
                let elem = TextureRenderElement::from_texture_buffer(
                    buffer.clone(),
                    boxed.loc,
                    area.alpha,
                    Some(src),
                    Some(size),
                    Kind::Unspecified,
                );
                push(OverlayElement::Texture(PrimaryGpuTextureRenderElement(
                    elem,
                )));
            }
            FillOwned::None | FillOwned::Unknown(_) => {}
        }
    }
}

/// The part of an image of `image` size that, scaled up or down to cover `boxed`, fills it: the
/// image's middle, as wide or as tall as the box's shape allows.
fn cover_crop(image: Size<f64, Logical>, boxed: Size<f64, Logical>) -> Rectangle<f64, Logical> {
    let k = (boxed.w / image.w).max(boxed.h / image.h);
    let size = Size::from((boxed.w / k, boxed.h / k));
    let loc = Point::from(((image.w - size.w) / 2., (image.h - size.h) / 2.));
    Rectangle::new(loc, size)
}

/// The size of lines stacked with gaps.
fn stack_size(sizes: &[Size<f64, Logical>], k: f64) -> Size<f64, Logical> {
    let w = sizes.iter().map(|s| s.w).fold(0., f64::max);
    let gaps = sizes.len().saturating_sub(1) as f64 * LINE_GAP * k;
    let h = sizes.iter().map(|s| s.h).sum::<f64>() + gaps;
    Size::from((w, h))
}

/// Where a box of `size` sits on `area` with its point `anchor` at the area's same point.
fn anchored(area: Area, anchor: Anchor, size: Size<f64, Logical>) -> Point<f64, Logical> {
    use Anchor::{Bottom, BottomLeft, BottomRight, Left, Right, Top, TopLeft, TopRight};
    // Negative where the box is larger than the area.
    let free = (area.size.w - size.w, area.size.h - size.h);
    // Each of 0 (start), 0.5 (middle) or 1 (end) of the free room.
    let (fx, fy) = match anchor {
        TopLeft => (0., 0.),
        Top => (0.5, 0.),
        TopRight => (1., 0.),
        Left => (0., 0.5),
        Right => (1., 0.5),
        BottomLeft => (0., 1.),
        Bottom => (0.5, 1.),
        BottomRight => (1., 1.),
        // A newer client's anchor is refused at `put`.
        Anchor::Center | Anchor::Unknown(_) => (0.5, 0.5),
    };
    area.loc + Point::from((free.0 * fx, free.1 * fy))
}

/// The blurred window: the texture, the part of it the window drew (in logical coordinates
/// from the window's top left) and the offscreen data of the render.
struct Blurred {
    buffer: TextureBuffer<GlesTexture>,
    src: Rectangle<f64, Logical>,
    data: OffscreenData,
}

/// A line as a texture as wide as its text, at most `width_px`, each of its own lines centred.
fn render_line(
    renderer: &mut GlesRenderer,
    text: &str,
    style: Style,
    (size_px, width_px): (i32, i32),
    scale: Scale<f64>,
) -> anyhow::Result<TextureBuffer<GlesTexture>> {
    let _span = tracy_client::span!("overlay::render_line");

    let mut font = FontDescription::from_string(FONT_FAMILY);
    font.set_absolute_size(f64::from(size_px) * f64::from(pangocairo::pango::SCALE));
    if style.bold {
        font.set_weight(Weight::Bold);
    }
    let layout_for = |cr: &cairo::Context, width: Option<i32>| {
        let layout = pangocairo::functions::create_layout(cr);
        layout.context().set_round_glyph_positions(false);
        layout.set_font_description(Some(&font));
        layout.set_alignment(Alignment::Center);
        if let Some(width) = width {
            layout.set_width(width * pangocairo::pango::SCALE);
            layout.set_ellipsize(EllipsizeMode::End);
        }
        layout.set_text(text);
        layout
    };

    let surface = ImageSurface::create(cairo::Format::ARgb32, 0, 0)?;
    let cr = cairo::Context::new(&surface)?;
    let (natural, _) = layout_for(&cr, None).pixel_size();
    let width = natural.clamp(1, width_px.max(1));
    let (_, height) = layout_for(&cr, Some(width)).pixel_size();
    let height = height.max(1);

    let surface = ImageSurface::create(cairo::Format::ARgb32, width, height)?;
    let cr = cairo::Context::new(&surface)?;
    let layout = layout_for(&cr, Some(width));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meter_bar_fills_blocks_by_the_nearest_tenth() {
        let bar = |percent| meter_bar(percent);
        assert_eq!(bar(0), "□□□□□□□□□□  0 %");
        assert_eq!(bar(44), "■■■■□□□□□□  44 %");
        assert_eq!(bar(45), "■■■■■□□□□□  45 %");
        assert_eq!(bar(150), "■■■■■■■■■■  150 %");
    }

    #[test]
    fn cover_crop_keeps_the_middle() {
        let crop = |image: (f64, f64), boxed: (f64, f64)| {
            let r = cover_crop(Size::from(image), Size::from(boxed));
            (r.loc.x, r.loc.y, r.size.w, r.size.h)
        };
        // A wide image in a 16:9 box: cut left and right.
        assert_eq!(
            crop((400., 100.), (160., 90.)),
            (111.11111111111111, 0., 177.77777777777777, 100.)
        );
        // A tall box: cut top and bottom of a square.
        assert_eq!(crop((100., 100.), (200., 100.)), (0., 25., 100., 50.));
        // The same shape: all of it.
        assert_eq!(crop((1920., 1080.), (960., 540.)), (0., 0., 1920., 1080.));
    }
}
