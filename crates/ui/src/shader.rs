//! Catalogued GPU fills: `shader(Effect::StarShimmer { .. })` anywhere a child
//! is legal.
//!
//! GPUI cannot compile app-supplied shader source. Each [`Effect`] maps to a
//! compiled-in `EffectQuad` primitive (Metal / WGSL / HLSL). Authoring a new
//! look is a fork patch, then a new variant here.

use gpui::{
    App, Bounds, EffectKind, Element, GlobalElementId, Hsla, InspectorElementId, IntoElement,
    LayoutId, PaintEffect, Pixels, Refineable, Style, StyleRefinement, Styled, Window, hsla,
};

/// A catalogued fragment effect. Drop it in a layout with [`shader`].
#[derive(Clone, Copy, Debug)]
pub enum Effect {
    /// Fixed dot lattice over a seeded, bubbling noise field.
    StarShimmer { seed: u32, speed: f32, variant: u32 },
    /// The same lattice, quieter — queued tiles.
    SoftNoise { seed: u32, amount: f32 },
    /// Diagonal progress band. `t` is 0..=1.
    ProgressWash { t: f32 },
}

impl Effect {
    pub fn star_shimmer(seed: u32) -> Self {
        Self::StarShimmer {
            seed,
            speed: 1.0,
            variant: 0,
        }
    }

    fn kind(self) -> EffectKind {
        match self {
            Self::StarShimmer { .. } => EffectKind::StarShimmer,
            Self::SoftNoise { .. } => EffectKind::SoftNoise,
            Self::ProgressWash { .. } => EffectKind::ProgressWash,
        }
    }

    fn seed(self) -> f32 {
        match self {
            Self::StarShimmer { seed, .. } | Self::SoftNoise { seed, .. } => seed as f32,
            Self::ProgressWash { t } => t,
        }
    }

    fn params(self) -> [f32; 8] {
        match self {
            Self::StarShimmer { speed, variant, .. } => {
                [speed, 1.0, 0.0, 0.0, variant as f32, 0.0, 0.0, 0.0]
            }
            Self::SoftNoise { amount, .. } => {
                [0.28, amount.clamp(0.0, 1.0), 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]
            }
            Self::ProgressWash { t } => [0.0, 1.0, t.clamp(0.0, 1.0), 0.0, 0.0, 0.0, 0.0, 0.0],
        }
    }

    fn animates(self) -> bool {
        match self {
            Self::StarShimmer { speed, .. } => speed > 0.0,
            Self::SoftNoise { .. } => true,
            Self::ProgressWash { .. } => false,
        }
    }
}

/// Fill `bounds` with `effect`. Style it like any other element (size, radius).
pub fn shader(effect: Effect) -> Shader {
    Shader {
        effect,
        progress: None,
        style: StyleRefinement::default(),
    }
}

pub struct Shader {
    effect: Effect,
    progress: Option<f32>,
    style: StyleRefinement,
}

impl Shader {
    /// Overlay a diagonal progress wash on [`Effect::StarShimmer`] when the
    /// run reports real download progress.
    pub fn progress(mut self, t: Option<f32>) -> Self {
        self.progress = t;
        self
    }
}

impl Styled for Shader {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl Element for Shader {
    type RequestLayoutState = Style;
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Style) {
        let mut style = Style::default();
        style.refine(&self.style);
        let layout_id = window.request_layout(style.clone(), [], cx);
        (layout_id, style)
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Style,
        _window: &mut Window,
        _cx: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        style: &mut Style,
        _prepaint: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let reduced = cx.reduce_motion();
        if self.effect.animates() && !reduced {
            window.request_animation_frame();
        }

        let rem = window.rem_size();
        let corner_radii = style
            .corner_radii
            .to_pixels(rem)
            .clamp_radii_for_quad_size(bounds.size);

        let mut params = self.effect.params();
        if let Some(t) = self.progress.filter(|t| *t > 0.0) {
            params[2] = t.clamp(0.0, 1.0);
        }
        // Device-pixel cell so a 12px lattice holds across scale factors.
        params[3] = 12.0 * window.scale_factor();

        let (color0, color1) = effect_colors(self.effect);
        window.paint_effect_quad(
            bounds,
            PaintEffect {
                kind: self.effect.kind(),
                seed: self.effect.seed(),
                params,
                color0,
                color1,
                corner_radii,
                frozen: reduced,
            },
        );
    }
}

impl IntoElement for Shader {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

fn effect_colors(effect: Effect) -> (Hsla, Hsla) {
    // Transparent tile; color1 is the dot/wash ink. Alpha lives in the shader.
    match effect {
        Effect::StarShimmer { .. } | Effect::SoftNoise { .. } | Effect::ProgressWash { .. } => {
            (hsla(0.0, 0.0, 1.0, 0.0), hsla(0.0, 0.0, 1.0, 1.0))
        }
    }
}
