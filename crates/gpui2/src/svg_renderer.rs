use crate::{DevicePixels, IsZero, Result, SharedString, Size, AnyAssetSource};
use anyhow::anyhow;
use std::hash::Hash;

#[derive(Clone, PartialEq, Hash, Eq)]
pub struct RenderSvgParams {
    pub(crate) path: SharedString,
    pub(crate) size: Size<DevicePixels>,
}

pub struct SvgRenderer {
    asset_source: AnyAssetSource,
}

impl SvgRenderer {
    pub fn new(asset_source: AnyAssetSource) -> Self {
        Self { asset_source }
    }

    pub fn render(&self, params: &RenderSvgParams) -> Result<Vec<u8>> {
        if params.size.is_zero() {
            return Err(anyhow!("can't render at a zero size"));
        }

        // Load the tree.
        let bytes = self.asset_source.load(params.path.clone())?;
        let tree = usvg::Tree::from_data(&bytes, &usvg::Options::default())?;

        // Render the SVG to a pixmap with the specified width and height.
        let mut pixmap =
            tiny_skia::Pixmap::new(params.size.width.into(), params.size.height.into()).unwrap();
        resvg::render(
            &tree,
            usvg::FitTo::Width(params.size.width.into()),
            pixmap.as_mut(),
        );

        // Convert the pixmap's pixels into an alpha mask.
        let alpha_mask = pixmap
            .pixels()
            .iter()
            .map(|p| p.alpha())
            .collect::<Vec<_>>();
        Ok(alpha_mask)
    }
}
