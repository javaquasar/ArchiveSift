#![cfg(windows)]

use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{FreeLibrary, HMODULE};
use windows::Win32::System::LibraryLoader::{
    FindResourceW, LoadLibraryExW, LoadResource, LockResource, SizeofResource,
    LOAD_LIBRARY_AS_DATAFILE, LOAD_LIBRARY_AS_IMAGE_RESOURCE,
};

struct ResourceModule(HMODULE);

impl Drop for ResourceModule {
    fn drop(&mut self) {
        // The module was loaded as data, so no application code is executed.
        unsafe {
            let _ = FreeLibrary(self.0);
        }
    }
}

fn resource_bytes(module: &ResourceModule, id: u16, kind: u16) -> &[u8] {
    // MAKEINTRESOURCEW uses integer IDs in the low 16 bits of a pointer.
    unsafe {
        let resource = FindResourceW(
            module.0,
            PCWSTR(id as usize as *const u16),
            PCWSTR(kind as usize as *const u16),
        );
        assert!(!resource.is_invalid(), "Missing resource {kind}/{id}");
        let length = SizeofResource(module.0, resource) as usize;
        assert!(length > 0);
        let data = LoadResource(module.0, resource).unwrap();
        let pointer = LockResource(data);
        assert!(!pointer.is_null());
        std::slice::from_raw_parts(pointer.cast::<u8>(), length)
    }
}

#[test]
fn windows_executable_embeds_all_icon_resolutions() {
    let path = std::env::var_os("ARCHIVESIFT_ICON_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_archive-sift")));
    let wide_path = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let module = ResourceModule(unsafe {
        LoadLibraryExW(
            PCWSTR(wide_path.as_ptr()),
            None,
            LOAD_LIBRARY_AS_DATAFILE | LOAD_LIBRARY_AS_IMAGE_RESOURCE,
        )
        .unwrap()
    });
    let group = resource_bytes(&module, 1, 14); // RT_GROUP_ICON
    assert_eq!(&group[..6], &[0, 0, 1, 0, 7, 0]);
    for (index, expected_size) in [16_u32, 24, 32, 48, 64, 128, 256].into_iter().enumerate() {
        let entry = 6 + index * 14;
        let icon_id = u16::from_le_bytes(group[entry + 12..entry + 14].try_into().unwrap());
        let image = image::load_from_memory(resource_bytes(&module, icon_id, 3))
            .unwrap()
            .into_rgba8(); // RT_ICON
        assert_eq!(image.dimensions(), (expected_size, expected_size));
        if expected_size == 256 {
            let runtime =
                image::load_from_memory(include_bytes!("../assets/icons/archive-sift.png"))
                    .unwrap()
                    .into_rgba8();
            assert_eq!(image, runtime);
        }
    }
}
