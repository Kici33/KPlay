//! KPlay's application icon, embedded in the binary from the project asset.

const ICON_PNG: &[u8] = include_bytes!("../assets/kplay-icon.png");

/// Decode the embedded icon and resize it to `size` x `size` RGBA pixels.
pub fn rgba(size: u32) -> Vec<u8> {
    image::load_from_memory_with_format(ICON_PNG, image::ImageFormat::Png)
        .expect("embedded KPlay icon must be a valid PNG")
        .resize_exact(size, size, image::imageops::FilterType::Lanczos3)
        .into_rgba8()
        .into_raw()
}
