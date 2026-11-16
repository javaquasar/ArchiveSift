use std::path::PathBuf;

use crate::models::ScrapedItem;

pub(crate) mod preview;

pub(crate) const PROFILE: &str = "knife";
pub(crate) const MAX_NAME: usize = 512;

pub(crate) struct KnifeProfile<'a> {
    pub manufacturer: &'a str,
    pub model: &'a str,
    pub steel: Option<&'a str>,
}

impl<'a> KnifeProfile<'a> {
    pub fn from_item(item: &'a ScrapedItem) -> Result<Option<Self>, &'static str> {
        let value = |kind| {
            item.attributes
                .iter()
                .find(|attribute| attribute.kind == kind)
                .map(|attribute| attribute.value.trim())
                .filter(|value| !value.is_empty())
        };
        if value("catalog_profile") != Some(PROFILE) {
            return Ok(None);
        }
        let manufacturer = value("manufacturer")
            .filter(|value| value.len() <= MAX_NAME)
            .ok_or("Knife profile requires a valid manufacturer")?;
        let model = value("model")
            .filter(|value| value.len() <= MAX_NAME)
            .ok_or("Knife profile requires a valid model")?;
        Ok(Some(Self {
            manufacturer,
            model,
            steel: value("steel"),
        }))
    }

    pub fn directory(&self) -> PathBuf {
        PathBuf::from(PROFILE)
            .join(component(self.manufacturer, 64))
            .join(component(self.model, 80))
    }

    pub fn image_stem(&self, index: usize) -> String {
        format!(
            "{} - {} - {} - {index:03}",
            component(self.manufacturer, 40),
            component(self.model, 64),
            component(self.steel.unwrap_or("Unknown Steel"), 32),
        )
    }
}

pub(crate) fn identity(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn component(value: &str, limit: usize) -> String {
    let mut safe = String::new();
    for character in value.trim().chars() {
        let character = match character {
            c if c.is_control() => '_',
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c => c,
        };
        if safe.len() + character.len_utf8() > limit {
            break;
        }
        safe.push(character);
    }
    let safe = safe.trim_matches([' ', '.']);
    if safe.is_empty() {
        "_".into()
    } else if is_windows_device_name(safe) {
        format!("_{safe}")
    } else {
        safe.to_owned()
    }
}

fn is_windows_device_name(name: &str) -> bool {
    let device = name
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    matches!(
        device.trim_end(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        device
            .trim_end()
            .strip_prefix(prefix)
            .is_some_and(|suffix| {
                matches!(
                    suffix,
                    "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
    })
}

// Use the same set when planning archived and downloaded images.
pub(crate) fn image_extension(extension: &str) -> bool {
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "jpg" | "jpeg" | "png" | "webp" | "gif" | "avif" | "bmp" | "tiff" | "tif"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_portable_and_keep_original_spacing() {
        let knife = KnifeProfile {
            manufacturer: "AUX.txt",
            model: "../Folding: Knife. ",
            steel: Some("CPM 3V"),
        };
        assert_eq!(
            knife.directory(),
            PathBuf::from("knife")
                .join("_AUX.txt")
                .join("_Folding_ Knife")
        );
        assert_eq!(
            knife.image_stem(2),
            "_AUX.txt - _Folding_ Knife - CPM 3V - 002"
        );
        assert_eq!(identity("  Élite \t Knife "), "élite knife");
        assert_eq!(component("ééé", 5), "éé");
    }
}
