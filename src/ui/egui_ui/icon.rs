use std::sync::{Arc, OnceLock};

use eframe::egui;

pub(super) fn application_icon() -> Arc<egui::IconData> {
    static ICON: OnceLock<Arc<egui::IconData>> = OnceLock::new();
    Arc::clone(ICON.get_or_init(|| {
        let image =
            image::load_from_memory(include_bytes!("../../../assets/icons/archive-sift.png"))
                .expect("Invalid embedded ArchiveSift icon")
                .into_rgba8();
        Arc::new(egui::IconData {
            width: image.width(),
            height: image.height(),
            rgba: image.into_raw(),
        })
    }))
}

/// All native windows use the same identity, including independent viewports.
pub(super) fn application_viewport() -> egui::ViewportBuilder {
    egui::ViewportBuilder::default()
        .with_icon(application_icon())
        .with_app_id("archive-sift")
}

pub(super) fn native_options() -> eframe::NativeOptions {
    eframe::NativeOptions {
        viewport: application_viewport().with_title("ArchiveSift"),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn application_icon_is_embedded_square_rgba_with_transparent_corners() {
        let icon = application_icon();
        assert_eq!((icon.width, icon.height), (256, 256));
        assert_eq!(icon.rgba.len(), 256 * 256 * 4);
        assert_eq!(icon.rgba[3], 0);
        assert!(icon.rgba.chunks_exact(4).any(|pixel| pixel[3] == 255));
        assert!(Arc::ptr_eq(&icon, &application_icon()));
        let options = native_options();
        assert!(Arc::ptr_eq(options.viewport.icon.as_ref().unwrap(), &icon));
        assert_eq!(options.viewport.app_id.as_deref(), Some("archive-sift"));
    }

    #[test]
    fn windows_ico_contains_every_size_and_matches_the_window_icon() {
        let bytes = include_bytes!("../../../assets/icons/archive-sift.ico");
        assert_eq!(&bytes[..6], &[0, 0, 1, 0, 7, 0]);
        for (index, size) in [16_u32, 24, 32, 48, 64, 128, 256].into_iter().enumerate() {
            let entry = 6 + index * 16;
            let actual_size = if bytes[entry] == 0 {
                256
            } else {
                u32::from(bytes[entry])
            };
            assert_eq!(actual_size, size);
            assert_eq!(bytes[entry], bytes[entry + 1]);
            let length =
                u32::from_le_bytes(bytes[entry + 8..entry + 12].try_into().unwrap()) as usize;
            let offset =
                u32::from_le_bytes(bytes[entry + 12..entry + 16].try_into().unwrap()) as usize;
            let image = image::load_from_memory(&bytes[offset..offset + length])
                .unwrap()
                .into_rgba8();
            assert_eq!(image.dimensions(), (size, size));
            if size == 256 {
                assert_eq!(image.as_raw(), &application_icon().rgba);
            }
        }
    }
}
