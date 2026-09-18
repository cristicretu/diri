use super::*;
use gpui::{AtlasKey, Corners, PlatformAtlas, RenderSvgParams, TransformationMatrix, hsla, px};
use std::borrow::Cow;

#[derive(Clone, Copy, Debug)]
enum Foreground {
    Glyph,
    Badge,
    Path,
}

fn scene(renderer: &MetalRenderer, foreground: Foreground, backdrop: Option<f32>) -> Scene {
    let bounds = Bounds::new(
        point(ScaledPixels(0.0), ScaledPixels(0.0)),
        size(ScaledPixels(16.0), ScaledPixels(16.0)),
    );
    let content_mask = ContentMask { bounds };
    let mut scene = Scene::default();
    if let Some(lightness) = backdrop {
        scene.insert_primitive(Quad {
            bounds,
            content_mask,
            background: hsla(0.0, 0.0, lightness, 1.0).into(),
            ..Default::default()
        });
    }
    // A dark translucent sidebar over desktop content.
    scene.insert_primitive(Quad {
        bounds,
        content_mask,
        background: hsla(0.0, 0.0, 0.15, 0.89).into(),
        ..Default::default()
    });
    match foreground {
        Foreground::Glyph => {
            // Deterministic glyph coverage, including antialiased edge pixels;
            // uses the same atlas and shader as text and monochrome icons.
            let size = size(DevicePixels(16), DevicePixels(16));
            let key = AtlasKey::Svg(RenderSvgParams {
                path: "compositing-test-coverage".into(),
                size,
            });
            let tile = renderer
                .sprite_atlas
                .get_or_insert_with(&key, &mut || {
                    let coverage = (0..256).map(|i| ((i % 16) * 17) as u8).collect();
                    Ok(Some((size, Cow::Owned(coverage))))
                })
                .unwrap()
                .unwrap();
            scene.insert_primitive(MonochromeSprite {
                order: Default::default(),
                pad: 0,
                bounds,
                content_mask,
                color: hsla(0.0, 0.0, 1.0, 0.86),
                tile,
                transformation: TransformationMatrix::unit(),
            });
        }
        Foreground::Badge => scene.insert_primitive(Quad {
            bounds,
            content_mask,
            background: hsla(0.1, 0.7, 0.6, 0.25).into(),
            corner_radii: Corners::all(ScaledPixels(6.0)),
            ..Default::default()
        }),
        Foreground::Path => {
            let mut path = Path::new(point(px(2.0), px(2.0)));
            path.line_to(point(px(14.0), px(3.0)));
            path.line_to(point(px(4.0), px(14.0)));
            path.line_to(point(px(2.0), px(2.0)));
            path.color = hsla(0.1, 0.7, 0.6, 0.5).into();
            path.content_mask = ContentMask {
                bounds: Bounds::new(point(px(0.0), px(0.0)), size(px(16.0), px(16.0))),
            };
            scene.insert_primitive(path.scale(1.0));
        }
    }
    scene.finish();
    scene
}

fn assert_compositing_matches_opaque(foreground: Foreground) {
    let mut renderer = MetalRenderer::new_headless(Arc::new(Mutex::new(Default::default())));
    let size = size(DevicePixels(16), DevicePixels(16));
    renderer.update_transparency(true);
    let translucent = renderer
        .render_scene_to_image(&scene(&renderer, foreground, None), size)
        .unwrap();

    // Rendering onto a transparent window then compositing over the desktop
    // must match drawing directly over that desktop color. Additive alpha
    // produces dark fringes on white while hiding the error on black.
    for backdrop in [1.0, 0.5, 0.0] {
        renderer.update_transparency(false);
        let opaque = renderer
            .render_scene_to_image(&scene(&renderer, foreground, Some(backdrop)), size)
            .unwrap();
        for (x, y, pixel) in translucent.enumerate_pixels() {
            let expected = opaque.get_pixel(x, y);
            for channel in 0..3 {
                let composited =
                    f32::from(pixel[channel]) + (255.0 - f32::from(pixel[3])) * backdrop;
                assert!(
                    (composited - f32::from(expected[channel])).abs() <= 2.0,
                    "{foreground:?} over {backdrop}: pixel ({x}, {y}) channel {channel} \
                     composited to {composited}, expected {}; RGBA={pixel:?}",
                    expected[channel],
                );
            }
            assert_eq!(expected[3], 255, "opaque surfaces must stay opaque");
        }
    }
}

#[test]
fn translucent_glyph_edges_match_opaque_compositing() {
    assert_compositing_matches_opaque(Foreground::Glyph);
}

#[test]
fn translucent_badge_edges_match_opaque_compositing() {
    assert_compositing_matches_opaque(Foreground::Badge);
}

#[test]
fn translucent_paths_match_opaque_compositing() {
    assert_compositing_matches_opaque(Foreground::Path);
}
