//! The app's bundled assets: today just the titlebar's window-control
//! glyphs (family-made, the same bytes as the kuma apps' set).

use std::borrow::Cow;

use gpui::{AssetSource, SharedString};

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        let bytes: Option<&'static [u8]> = match path {
            "icons/titlebar_minimize.svg" => {
                Some(include_bytes!("../assets/icons/titlebar_minimize.svg"))
            }
            "icons/titlebar_maximize.svg" => {
                Some(include_bytes!("../assets/icons/titlebar_maximize.svg"))
            }
            "icons/titlebar_close.svg" => Some(include_bytes!("../assets/icons/titlebar_close.svg")),
            _ => None,
        };
        Ok(bytes.map(Cow::Borrowed))
    }

    fn list(&self, _path: &str) -> gpui::Result<Vec<SharedString>> {
        Ok(Vec::new())
    }
}
