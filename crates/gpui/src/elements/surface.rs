use crate::{
    App, Bounds, Element, ElementId, ExternalSurface, GlobalElementId, InspectorElementId,
    IntoElement, LayoutId, ObjectFit, Pixels, Style, StyleRefinement, Styled, Window,
};
#[cfg(any(target_os = "macos", target_os = "ios"))]
use objc2_core_foundation::CFRetained;
#[cfg(any(target_os = "macos", target_os = "ios"))]
use objc2_core_video::{CVPixelBuffer, CVPixelBufferGetHeight, CVPixelBufferGetWidth};
use refineable::Refineable;

/// A source of a surface's content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SurfaceSource {
    /// A CoreVideo image buffer
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    Surface(CFRetained<CVPixelBuffer>),
    /// A video frame decoded outside GPUI
    External(ExternalSurface),
}

impl From<ExternalSurface> for SurfaceSource {
    fn from(value: ExternalSurface) -> Self {
        SurfaceSource::External(value)
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
impl From<CFRetained<CVPixelBuffer>> for SurfaceSource {
    fn from(value: CFRetained<CVPixelBuffer>) -> Self {
        SurfaceSource::Surface(value)
    }
}

/// A surface element.
pub struct Surface {
    source: SurfaceSource,
    object_fit: ObjectFit,
    style: StyleRefinement,
}

/// Create a new surface element.
pub fn surface(source: impl Into<SurfaceSource>) -> Surface {
    Surface {
        source: source.into(),
        object_fit: ObjectFit::Contain,
        style: Default::default(),
    }
}

impl Surface {
    /// Set the object fit for the image.
    pub fn object_fit(mut self, object_fit: ObjectFit) -> Self {
        self.object_fit = object_fit;
        self
    }
}

impl Element for Surface {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.refine(&self.style);
        let layout_id = window.request_layout(style, [], cx);
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
    }

    fn paint(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        window: &mut Window,
        _: &mut App,
    ) {
        match &self.source {
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            SurfaceSource::Surface(surface) => {
                let size = crate::size(
                    CVPixelBufferGetWidth(surface).into(),
                    CVPixelBufferGetHeight(surface).into(),
                );
                let new_bounds = self.object_fit.get_bounds(bounds, size);
                // TODO: Add support for corner_radii
                window.paint_surface(new_bounds, surface.clone());
            }
            SurfaceSource::External(surface) => {
                let new_bounds = self.object_fit.get_bounds(bounds, surface.size());
                window.paint_external_surface(new_bounds, surface.clone());
            }
        }
    }
}

impl IntoElement for Surface {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Styled for Surface {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CpuNv12Frame, ExternalStreamId, Nv12Layout, ScaledPixels, TestAppContext, YuvColorMatrix,
        point, px, size,
    };
    use std::num::NonZeroU32;

    fn frame(
        stream: ExternalStreamId,
        generation: u64,
        width: u32,
        height: u32,
    ) -> ExternalSurface {
        let layout = Nv12Layout::packed(
            NonZeroU32::new(width).expect("test width is not zero"),
            NonZeroU32::new(height).expect("test height is not zero"),
        );
        let luma_len = (layout.y_stride * height) as usize;
        let chroma_len = (layout.uv_stride * layout.chroma_height()) as usize;
        CpuNv12Frame::new(
            stream,
            generation,
            layout,
            vec![128; luma_len],
            vec![128; chroma_len],
            YuvColorMatrix::Bt709Limited,
        )
        .expect("packed planes")
        .into()
    }

    #[gpui::test]
    fn external_surface_reaches_the_scene_letterboxed(cx: &mut TestAppContext) {
        let window = cx.add_empty_window();
        let stream = ExternalStreamId::new();
        window.draw(point(px(0.), px(0.)), size(px(100.), px(100.)), |_, _| {
            surface(frame(stream, 41, 200, 100))
                .size_full()
                .into_any_element()
        });

        let (painted, scale_factor) = window.update(|window, _| {
            (
                window.rendered_frame.scene.external_surfaces.clone(),
                window.scale_factor(),
            )
        });
        let [painted] = painted.as_slice() else {
            panic!("expected one external surface, got {painted:?}");
        };
        assert_eq!(painted.surface.stream(), stream);
        assert_eq!(painted.surface.generation(), 41);
        assert_eq!(painted.surface.color(), YuvColorMatrix::Bt709Limited);
        let scaled = |value: f32| ScaledPixels(value * scale_factor);
        assert_eq!(
            painted.bounds,
            Bounds {
                origin: point(scaled(0.), scaled(25.)),
                size: size(scaled(100.), scaled(50.)),
            }
        );
    }

    #[gpui::test]
    fn each_frame_paints_the_generation_it_was_given(cx: &mut TestAppContext) {
        let window = cx.add_empty_window();
        let stream = ExternalStreamId::new();
        for generation in [1, 2] {
            window.draw(point(px(10.), px(20.)), size(px(64.), px(32.)), |_, _| {
                surface(frame(stream, generation, 64, 32))
                    .size_full()
                    .object_fit(ObjectFit::Fill)
                    .into_any_element()
            });
            let generations = window.update(|window, _| {
                window
                    .rendered_frame
                    .scene
                    .external_surfaces
                    .iter()
                    .map(|painted| painted.surface.generation())
                    .collect::<Vec<_>>()
            });
            assert_eq!(generations, [generation]);
        }
    }
}
