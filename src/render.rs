use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use anyhow::{Context, anyhow};
use image::{DynamicImage, ImageReader, Limits, Rgb, RgbImage, RgbaImage};
use imageproc::drawing::draw_text_mut;
use std::fs::File;
use std::io::{BufReader, Cursor};
use std::path::Path;

const CANVAS_WIDTH: u32 = 384;
const HORIZONTAL_PADDING: u32 = 8;
const VERTICAL_PADDING: u32 = 8;
const FONT_SIZE: f32 = 24.0;
const STACK_SPACING: u32 = 8;
const MAX_CANVAS_HEIGHT: u32 = u16::MAX as u32;
const MAX_ENCODED_IMAGE_BYTES: usize = 10 * 1024 * 1024;
const MAX_SOURCE_DIMENSION: u32 = 16_384;
const MAX_SOURCE_PIXELS: u64 = 16 * 1024 * 1024;
const MAX_DECODE_ALLOC: u64 = 64 * 1024 * 1024;
const ERROR_DIFFUSION_DIVISOR: i32 = 32;
const FONT_DATA: &[u8] = include_bytes!("../res/zpix.ttf");
const ERROR_DIFFUSION_KERNEL: [(i32, i32, i32); 10] = [
    (1, 0, 5),
    (2, 0, 3),
    (-2, 1, 2),
    (-1, 1, 4),
    (0, 1, 3),
    (1, 1, 4),
    (2, 1, 2),
    (-1, 2, 2),
    (0, 2, 3),
    (1, 2, 2),
];

pub fn render_text(text: &str) -> anyhow::Result<RgbImage> {
    let font = FontRef::try_from_slice(FONT_DATA).context("Embedded font is invalid")?;
    let scale = PxScale::from(FONT_SIZE);
    let available_width = (CANVAS_WIDTH - HORIZONTAL_PADDING * 2) as f32;
    let lines = wrap_text(&font, scale, text, available_width);

    let scaled_font = font.as_scaled(scale);
    let line_height = (scaled_font.ascent() - scaled_font.descent() + scaled_font.line_gap())
        .ceil()
        .max(1.0) as u32;
    let line_count = u32::try_from(lines.len()).context("Text contains too many lines")?;
    let content_height = line_height
        .checked_mul(line_count)
        .context("Rendered text height overflowed")?;
    let canvas_height = content_height
        .checked_add(VERTICAL_PADDING * 2)
        .context("Rendered text height overflowed")?;
    if canvas_height > MAX_CANVAS_HEIGHT {
        return Err(anyhow!("Rendered text is too tall"));
    }

    let mut canvas = RgbImage::from_pixel(CANVAS_WIDTH, canvas_height, Rgb([255, 255, 255]));
    for (line_index, line) in lines.iter().enumerate() {
        let y = VERTICAL_PADDING + line_height * line_index as u32;
        draw_text_mut(
            &mut canvas,
            Rgb([0, 0, 0]),
            HORIZONTAL_PADDING as i32,
            y as i32,
            scale,
            &font,
            line,
        );
    }

    Ok(canvas)
}

pub fn load_image(path: impl AsRef<Path>) -> anyhow::Result<RgbImage> {
    let path = path.as_ref();
    let dimensions = file_image_reader(path)?
        .into_dimensions()
        .context("Failed to inspect image dimensions")?;
    validate_source_dimensions(dimensions.0, dimensions.1)?;

    let decoded = file_image_reader(path)?
        .decode()
        .context("Failed to decode image")?;
    prepare_image(decoded)
}

pub fn load_image_bytes(bytes: &[u8]) -> anyhow::Result<RgbImage> {
    if bytes.len() > MAX_ENCODED_IMAGE_BYTES {
        return Err(anyhow!("Encoded image exceeds the size limit"));
    }

    let dimensions = memory_image_reader(bytes)?
        .into_dimensions()
        .context("Failed to inspect image dimensions")?;
    validate_source_dimensions(dimensions.0, dimensions.1)?;

    let decoded = memory_image_reader(bytes)?
        .decode()
        .context("Failed to decode image")?;
    prepare_image(decoded)
}

pub fn stack_vertical(images: &[RgbImage]) -> anyhow::Result<RgbImage> {
    if images.is_empty() {
        return Err(anyhow!("At least one image section is required"));
    }

    let spacing_count = u32::try_from(images.len() - 1).context("Too many image sections")?;
    let mut total_height = STACK_SPACING
        .checked_mul(spacing_count)
        .context("Combined image height overflowed")?;

    for image in images {
        if image.width() == 0 || image.height() == 0 {
            return Err(anyhow!("Image dimensions must be non-zero"));
        }
        if image.width() > CANVAS_WIDTH {
            return Err(anyhow!("Image width exceeds the canvas"));
        }
        total_height = total_height
            .checked_add(image.height())
            .context("Combined image height overflowed")?;
    }

    if total_height > MAX_CANVAS_HEIGHT {
        return Err(anyhow!("Combined image is too tall"));
    }

    let mut canvas = RgbImage::from_pixel(CANVAS_WIDTH, total_height, Rgb([255, 255, 255]));
    let mut y_offset = 0;
    for (index, image) in images.iter().enumerate() {
        let x_offset = (CANVAS_WIDTH - image.width()) / 2;
        for (x, y, pixel) in image.enumerate_pixels() {
            canvas.put_pixel(x_offset + x, y_offset + y, *pixel);
        }
        y_offset += image.height();
        if index + 1 < images.len() {
            y_offset += STACK_SPACING;
        }
    }

    Ok(canvas)
}

fn memory_image_reader(bytes: &[u8]) -> anyhow::Result<ImageReader<Cursor<&[u8]>>> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .context("Failed to identify image format")?;
    reader.limits(image_decode_limits());
    Ok(reader)
}

fn file_image_reader(path: &Path) -> anyhow::Result<ImageReader<BufReader<File>>> {
    let mut reader = ImageReader::open(path)
        .context("Failed to open image")?
        .with_guessed_format()
        .context("Failed to identify image format")?;
    reader.limits(image_decode_limits());
    Ok(reader)
}

fn image_decode_limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SOURCE_DIMENSION);
    limits.max_image_height = Some(MAX_SOURCE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    limits
}

fn validate_source_dimensions(width: u32, height: u32) -> anyhow::Result<()> {
    if width == 0 || height == 0 {
        return Err(anyhow!("Image dimensions must be non-zero"));
    }
    if width > MAX_SOURCE_DIMENSION || height > MAX_SOURCE_DIMENSION {
        return Err(anyhow!("Image dimensions exceed the limit"));
    }

    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .context("Image dimensions overflowed")?;
    if pixels > MAX_SOURCE_PIXELS {
        return Err(anyhow!("Image contains too many pixels"));
    }

    Ok(())
}

fn prepare_image(decoded: DynamicImage) -> anyhow::Result<RgbImage> {
    let source_width = decoded.width();
    let source_height = decoded.height();
    validate_source_dimensions(source_width, source_height)?;

    let target_height =
        (((source_height as f32 / source_width as f32) * CANVAS_WIDTH as f32) as u32).max(1);
    if target_height > MAX_CANVAS_HEIGHT {
        return Err(anyhow!("Image is too tall"));
    }

    let resized = scale_android_rgb565(&decoded.into_rgba8(), target_height)?;
    Ok(render_photo_mode_zero(&resized))
}

fn scale_android_rgb565(source: &RgbaImage, target_height: u32) -> anyhow::Result<RgbImage> {
    let source_width = source.width();
    let source_height = source.height();
    if source_width == 0 || source_height == 0 || target_height == 0 {
        return Err(anyhow!("Image dimensions must be non-zero"));
    }

    let scale = (CANVAS_WIDTH as f32 / source_width as f32)
        .min(target_height as f32 / source_height as f32);
    let scaled_width = source_width as f32 * scale;
    let scaled_height = source_height as f32 * scale;
    let translation_x = (CANVAS_WIDTH as f32 - scaled_width) / 2.0;
    let translation_y = (target_height / 2) as f32 - scaled_height / 2.0;
    let inverse_scale = 1.0 / scale;
    let inverse_translation_x = -translation_x * inverse_scale;
    let inverse_translation_y = -translation_y * inverse_scale;

    // Android filters premultiplied pixels and applies 24.8 edge coverage before RGB565 storage.
    let clipped_left = android_fixed_dot8(translation_x.max(0.0));
    let clipped_top = android_fixed_dot8(translation_y.max(0.0));
    let clipped_right = android_fixed_dot8((translation_x + scaled_width).min(CANVAS_WIDTH as f32));
    let clipped_bottom =
        android_fixed_dot8((translation_y + scaled_height).min(target_height as f32));
    let mut canvas = RgbImage::from_pixel(
        CANVAS_WIDTH,
        target_height,
        pack_android_rgb565([255, 255, 255]),
    );
    if clipped_left >= clipped_right || clipped_top >= clipped_bottom {
        return Ok(canvas);
    }

    for y in 0..target_height {
        let source_y = (y as f32 + 0.5).mul_add(inverse_scale, inverse_translation_y);

        for x in 0..CANVAS_WIDTH {
            let coverage = android_rect_coverage(
                clipped_left,
                clipped_top,
                clipped_right,
                clipped_bottom,
                x,
                y,
            );
            if coverage == 0 {
                continue;
            }

            let source_x = (x as f32 + 0.5).mul_add(inverse_scale, inverse_translation_x);
            let sample = sample_android_bilinear(source, source_x, source_y);
            let alpha = div_255(u32::from(sample[3]) * u32::from(coverage));
            let mut composited = [0_u16; 3];
            for (channel, output) in sample[..3].iter().zip(&mut composited) {
                let covered = div_255(u32::from(*channel) * u32::from(coverage));
                *output = u16::from(covered) + u16::from(255 - alpha);
            }
            canvas.put_pixel(x, y, pack_android_rgb565(composited));
        }
    }

    Ok(canvas)
}

fn sample_android_bilinear(source: &RgbaImage, x: f32, y: f32) -> [u8; 4] {
    let (left_x, x_fraction) = android_sample_coordinate(x);
    let (top_y, y_fraction) = android_sample_coordinate(y);
    let right_x = left_x + 1;
    let bottom_y = top_y + 1;
    let max_x = source.width() as i32 - 1;
    let max_y = source.height() as i32 - 1;
    let top_left = premultiply_rgba(
        source.get_pixel(left_x.clamp(0, max_x) as u32, top_y.clamp(0, max_y) as u32),
    );
    let top_right = premultiply_rgba(
        source.get_pixel(right_x.clamp(0, max_x) as u32, top_y.clamp(0, max_y) as u32),
    );
    let bottom_left = premultiply_rgba(source.get_pixel(
        left_x.clamp(0, max_x) as u32,
        bottom_y.clamp(0, max_y) as u32,
    ));
    let bottom_right = premultiply_rgba(source.get_pixel(
        right_x.clamp(0, max_x) as u32,
        bottom_y.clamp(0, max_y) as u32,
    ));

    std::array::from_fn(|channel| {
        let top = android_horizontal_lerp(
            i32::from(top_left[channel]),
            i32::from(top_right[channel]),
            x_fraction,
        );
        let bottom = android_horizontal_lerp(
            i32::from(bottom_left[channel]),
            i32::from(bottom_right[channel]),
            x_fraction,
        );
        ((android_scaled_mult(y_fraction, bottom - top) + bottom + top + 128) >> 8) as u8
    })
}

fn android_sample_coordinate(coordinate: f32) -> (i32, i32) {
    let fixed = ((coordinate * 65_536.0 + 0.5).floor() as i32) - 32_768;
    (fixed >> 16, (fixed & 0xFFFF) - 32_768)
}

fn android_horizontal_lerp(left: i32, right: i32, fraction: i32) -> i32 {
    let width = (right - left) << 7;
    let middle = (right + left) << 7;
    (android_scaled_mult(fraction, width) + middle + 1) >> 1
}

fn android_scaled_mult(left: i32, right: i32) -> i32 {
    ((2_i64 * i64::from(left) * i64::from(right) + 32_768) >> 16)
        .clamp(i64::from(i16::MIN), i64::from(i16::MAX)) as i32
}

fn premultiply_rgba(pixel: &image::Rgba<u8>) -> [u8; 4] {
    let alpha = pixel[3];
    [
        div_255(u32::from(pixel[0]) * u32::from(alpha)),
        div_255(u32::from(pixel[1]) * u32::from(alpha)),
        div_255(u32::from(pixel[2]) * u32::from(alpha)),
        alpha,
    ]
}

fn div_255(value: u32) -> u8 {
    let biased = value + 128;
    ((biased + (biased >> 8)) >> 8) as u8
}

fn android_fixed_dot8(value: f32) -> i32 {
    let fixed = (value * 65_536.0) as i64;
    ((fixed + 128) >> 8) as i32
}

fn android_axis_coverage(start: i32, end: i32, position: u32) -> u16 {
    let pixel_start = (position as i32) << 8;
    let pixel_end = pixel_start + 256;
    (end.min(pixel_end) - start.max(pixel_start)).clamp(0, 256) as u16
}

fn android_rect_coverage(left: i32, top: i32, right: i32, bottom: i32, x: u32, y: u32) -> u8 {
    let horizontal = android_axis_coverage(left, right, x);
    let vertical = android_axis_coverage(top, bottom, y);
    if horizontal == 256 && vertical == 256 {
        255
    } else if horizontal == 256 {
        vertical.min(255) as u8
    } else if vertical == 256 {
        horizontal.min(255) as u8
    } else {
        ((horizontal * vertical) >> 8).min(255) as u8
    }
}

fn pack_android_rgb565(color: [u16; 3]) -> Rgb<u8> {
    let red = ((color[0].min(255) * 9 + 36) / 74) as u8;
    let green = ((color[1].min(255) * 21 + 42) / 85) as u8;
    let blue = ((color[2].min(255) * 9 + 36) / 74) as u8;
    Rgb([red << 3, green << 2, blue << 3])
}

fn wrap_text(font: &FontRef<'_>, scale: PxScale, text: &str, max_width: f32) -> Vec<String> {
    let scaled_font = font.as_scaled(scale);
    let mut lines = Vec::new();
    let mut current_line = String::new();
    let mut line_width = 0.0;
    let mut previous_glyph = None;

    for character in text.chars() {
        if character == '\r' {
            continue;
        }
        if character == '\n' {
            lines.push(std::mem::take(&mut current_line));
            line_width = 0.0;
            previous_glyph = None;
            continue;
        }

        let glyph_id = scaled_font.glyph_id(character);
        let advance_width = scaled_font.h_advance(glyph_id);
        let kerning = previous_glyph
            .map(|previous| scaled_font.kern(previous, glyph_id))
            .unwrap_or(0.0);

        if !current_line.is_empty() && line_width + kerning + advance_width > max_width {
            lines.push(std::mem::take(&mut current_line));
            line_width = 0.0;
            previous_glyph = None;
        }

        let active_kerning = previous_glyph
            .map(|previous| scaled_font.kern(previous, glyph_id))
            .unwrap_or(0.0);
        current_line.push(character);
        line_width += active_kerning + advance_width;
        previous_glyph = Some(glyph_id);
    }

    lines.push(current_line);
    lines
}

fn render_photo_mode_zero(source: &RgbImage) -> RgbImage {
    let grayscale: Vec<u8> = source.pixels().map(android_rgb565_bgra_luminance).collect();
    let mean = grayscale.iter().map(|&value| u64::from(value)).sum::<u64>() as f64
        / grayscale.len() as f64;
    let gamma = gamma_for_mean(mean);
    let levels = grayscale
        .into_iter()
        .map(|value| gamma_correct(value, gamma))
        .collect();

    diffuse_android_error(levels, source.width(), source.height())
}

fn android_rgb565_bgra_luminance(pixel: &Rgb<u8>) -> u8 {
    let [red, green, blue] = pixel.0;
    let red = u32::from(red & 0xF8);
    let green = u32::from(green & 0xFC);
    let blue = u32::from(blue & 0xF8);
    let weighted = 3_735 * red + 19_235 * green + 9_798 * blue + 16_384;
    (weighted >> 15) as u8
}

fn gamma_for_mean(mean: f64) -> f32 {
    if mean < 120.0 {
        1.8
    } else if mean < 130.0 {
        1.7
    } else if mean < 140.0 {
        1.5
    } else if mean < 150.0 {
        1.4
    } else if mean < 160.0 {
        1.3
    } else if mean < 170.0 {
        1.2
    } else if mean < 180.0 {
        1.0
    } else if mean < 190.0 {
        0.9
    } else if mean < 200.0 {
        0.8
    } else if mean < 210.0 {
        0.7
    } else if mean < 220.0 {
        0.6
    } else if mean < 230.0 {
        0.5
    } else if mean < 240.0 {
        0.4
    } else if mean < 250.0 {
        0.3
    } else {
        0.2
    }
}

fn gamma_correct(gray: u8, gamma: f32) -> i32 {
    if gamma == 1.0 {
        return i32::from(gray);
    }

    let normalized = f64::from(gray) / 255.0;
    let exponent = f64::from(1.0f32 / gamma);
    (255.0 * normalized.powf(exponent))
        .round_ties_even()
        .clamp(0.0, 255.0) as i32
}

fn diffuse_android_error(mut levels: Vec<i32>, width: u32, height: u32) -> RgbImage {
    debug_assert_eq!(levels.len(), width as usize * height as usize);

    let mut output = RgbImage::new(width, height);
    let row_stride = width as usize;
    for y in 0..height {
        for x in 0..width {
            let index = y as usize * row_stride + x as usize;
            let old = levels[index];
            let quantized = if old > 127 { 255 } else { 0 };
            output.put_pixel(x, y, Rgb([quantized as u8; 3]));

            let error = old.wrapping_sub(quantized);
            for &(delta_x, delta_y, base_weight) in &ERROR_DIFFUSION_KERNEL {
                let neighbor_x = i64::from(x) + i64::from(delta_x);
                let neighbor_y = i64::from(y) + i64::from(delta_y);
                if neighbor_x < 0
                    || neighbor_x >= i64::from(width)
                    || neighbor_y >= i64::from(height)
                {
                    continue;
                }

                let weight = if delta_x == 0
                    && delta_y == 1
                    && width >= 4
                    && (x <= 1 || x >= width - 2 || y == height - 2)
                {
                    5
                } else {
                    base_weight
                };
                let neighbor_index = neighbor_y as usize * row_stride + neighbor_x as usize;
                let adjustment = error.wrapping_mul(weight) / ERROR_DIFFUSION_DIVISOR;
                levels[neighbor_index] = levels[neighbor_index].wrapping_add(adjustment);
            }
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, ImageFormat, Rgb, Rgba, RgbaImage};
    use std::fs;
    use std::io::Cursor;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);

    struct TempImage {
        path: PathBuf,
    }

    impl TempImage {
        fn save(image: &RgbaImage) -> Self {
            let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "rs-luck-jingle-render-{}-{}.png",
                std::process::id(),
                sequence
            ));
            image.save(&path).expect("Temporary image should save");
            Self { path }
        }
    }

    impl Drop for TempImage {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
        }
    }

    fn encode_png(image: &RgbaImage) -> Vec<u8> {
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(image.clone())
            .write_to(&mut bytes, ImageFormat::Png)
            .expect("PNG should encode");
        bytes.into_inner()
    }

    #[test]
    fn text_render_has_fixed_width_and_white_background() {
        let image = render_text("").expect("Empty text should render");

        assert_eq!(image.width(), CANVAS_WIDTH);
        assert!(image.height() > 0);
        assert!(image.pixels().all(|pixel| pixel.0 == [255, 255, 255]));
    }

    #[test]
    fn text_wrapping_is_stable_and_increases_height() {
        let short = render_text("Short line").expect("Short text should render");
        let content = "Wide text ".repeat(80);
        let first = render_text(&content).expect("Long text should render");
        let second = render_text(&content).expect("Long text should render consistently");

        assert_eq!(first, second);
        assert_eq!(first.width(), CANVAS_WIDTH);
        assert!(first.height() > short.height());
    }

    #[test]
    fn explicit_line_break_produces_non_zero_height() {
        let image = render_text("First line\nSecond line").expect("Multiline text should render");

        assert_eq!(image.width(), CANVAS_WIDTH);
        assert!(image.height() > VERTICAL_PADDING * 2);
    }

    #[test]
    fn fully_transparent_image_matches_android_white_canvas_dithering() {
        let source = RgbaImage::from_pixel(2, 1, Rgba([10, 20, 30, 0]));
        let temp = TempImage::save(&source);

        let image = load_image(&temp.path).expect("Temporary image should load");
        let black_pixels = image.pixels().filter(|pixel| pixel.0 == [0, 0, 0]).count();

        assert_eq!(image.dimensions(), (CANVAS_WIDTH, CANVAS_WIDTH / 2));
        assert_eq!(black_pixels, 3_515);
        assert!(
            image
                .pixels()
                .all(|pixel| pixel.0 == [0, 0, 0] || pixel.0 == [255, 255, 255])
        );
    }

    #[test]
    fn wide_image_is_scaled_without_changing_aspect_ratio() {
        let source = RgbaImage::from_pixel(768, 200, Rgba([20, 40, 60, 255]));
        let temp = TempImage::save(&source);

        let image = load_image(&temp.path).expect("Temporary image should load");

        assert_eq!(image.dimensions(), (CANVAS_WIDTH, 100));
    }

    #[test]
    fn image_bytes_are_scaled_and_composited_on_white() {
        let mut source = RgbaImage::from_pixel(768, 200, Rgba([20, 40, 60, 255]));
        source.put_pixel(0, 0, Rgba([0, 0, 0, 0]));
        let bytes = encode_png(&source);

        let image = load_image_bytes(&bytes).expect("Image bytes should load");

        assert_eq!(image.dimensions(), (CANVAS_WIDTH, 100));
        assert!(
            image
                .pixels()
                .all(|pixel| pixel.0 == [0, 0, 0] || pixel.0 == [255, 255, 255])
        );
    }

    #[test]
    fn android_scaler_uses_integer_vertical_center_and_rgb565_storage() {
        let source = RgbaImage::from_pixel(768, 1, Rgba([0, 0, 0, 255]));

        let image = scale_android_rgb565(&source, 1).expect("Image should scale");

        assert_eq!(image.dimensions(), (CANVAS_WIDTH, 1));
        assert!(image.pixels().all(|pixel| pixel.0 == [184, 188, 184]));
    }

    #[test]
    fn android_scaler_premultiplies_before_filtering() {
        let hidden_color = premultiply_rgba(&Rgba([255, 0, 255, 0]));
        let translucent = premultiply_rgba(&Rgba([200, 100, 50, 128]));

        assert_eq!(hidden_color, [0, 0, 0, 0]);
        assert_eq!(translucent, [100, 50, 25, 128]);
    }

    #[test]
    fn android_bilinear_sampler_uses_q15_rounding() {
        let mut source = RgbaImage::new(2, 2);
        source.put_pixel(0, 0, Rgba([0, 0, 0, 255]));
        source.put_pixel(1, 0, Rgba([255, 0, 0, 255]));
        source.put_pixel(0, 1, Rgba([0, 255, 0, 255]));
        source.put_pixel(1, 1, Rgba([0, 0, 255, 255]));

        assert_eq!(
            sample_android_bilinear(&source, 1.0, 1.0),
            [64, 64, 64, 255]
        );
    }

    #[test]
    fn android_coverage_and_rgb565_quantization_match_skia() {
        assert_eq!(android_fixed_dot8(0.25), 64);
        assert_eq!(android_rect_coverage(0, 0, 256, 64, 0, 0), 64);
        assert_eq!(android_rect_coverage(108, 0, 98_196, 65_152, 0, 0), 148);
        assert_eq!(pack_android_rgb565([255, 255, 255]).0, [248, 252, 248]);
        assert_eq!(pack_android_rgb565([192, 192, 192]).0, [184, 188, 184]);
    }

    #[test]
    fn gamma_selection_changes_at_android_boundaries() {
        let cases = [
            (0.0, 1.8),
            (119.0, 1.8),
            (120.0, 1.7),
            (129.0, 1.7),
            (130.0, 1.5),
            (139.0, 1.5),
            (140.0, 1.4),
            (149.0, 1.4),
            (150.0, 1.3),
            (159.0, 1.3),
            (160.0, 1.2),
            (169.0, 1.2),
            (170.0, 1.0),
            (179.0, 1.0),
            (180.0, 0.9),
            (189.0, 0.9),
            (190.0, 0.8),
            (199.0, 0.8),
            (200.0, 0.7),
            (209.0, 0.7),
            (210.0, 0.6),
            (219.0, 0.6),
            (220.0, 0.5),
            (229.0, 0.5),
            (230.0, 0.4),
            (239.0, 0.4),
            (240.0, 0.3),
            (249.0, 0.3),
            (250.0, 0.2),
            (255.0, 0.2),
        ];

        for (mean, expected) in cases {
            assert_eq!(gamma_for_mean(mean), expected, "mean {mean}");
        }
    }

    #[test]
    fn rgb565_bgra_luminance_and_gamma_rounding_match_android() {
        assert_eq!(android_rgb565_bgra_luminance(&Rgb([255, 0, 0])), 28);
        assert_eq!(android_rgb565_bgra_luminance(&Rgb([0, 255, 0])), 148);
        assert_eq!(android_rgb565_bgra_luminance(&Rgb([0, 0, 255])), 74);
        assert_eq!(android_rgb565_bgra_luminance(&Rgb([255, 255, 255])), 250);
        assert_eq!(android_rgb565_bgra_luminance(&Rgb([250, 0, 0])), 28);
        assert_eq!(gamma_correct(13, 1.8), 49);
    }

    #[test]
    fn photo_mode_matches_android_color_channel_behavior() {
        let red = RgbImage::from_pixel(5, 1, Rgb([255, 0, 0]));
        let blue = RgbImage::from_pixel(5, 1, Rgb([0, 0, 255]));

        let red_output = render_photo_mode_zero(&red);
        let blue_output = render_photo_mode_zero(&blue);

        assert!(red_output.pixels().all(|pixel| pixel.0 == [0, 0, 0]));
        let blue_levels: Vec<u8> = blue_output.pixels().map(|pixel| pixel[0]).collect();
        assert_eq!(blue_levels, [255, 0, 255, 0, 255]);
    }

    #[test]
    fn pure_black_and_white_are_idempotent() {
        let mut source = RgbImage::new(8, 4);
        for (x, y, pixel) in source.enumerate_pixels_mut() {
            let value = if (x + y) % 2 == 0 { 0 } else { 255 };
            *pixel = Rgb([value; 3]);
        }

        let output = render_photo_mode_zero(&source);

        assert_eq!(output, source);
    }

    #[test]
    fn android_error_diffusion_matches_representative_golden() {
        let levels = vec![
            32, 64, 96, 128, 160, 192, 224, 200, 176, 152, 128, 104, 80, 112, 144, 176, 208, 240,
            15, 75, 135, 195, 225, 250,
        ];
        let expected = [
            [0, 0, 0, 255, 255, 255],
            [255, 255, 255, 0, 0, 0],
            [0, 255, 255, 255, 255, 255],
            [0, 0, 0, 255, 255, 255],
        ];

        let image = diffuse_android_error(levels, 6, 4);

        for (y, row) in expected.iter().enumerate() {
            for (x, &value) in row.iter().enumerate() {
                assert_eq!(image.get_pixel(x as u32, y as u32).0, [value; 3]);
            }
        }
    }

    #[test]
    fn android_error_diffusion_uses_edge_compensation() {
        let image = diffuse_android_error(vec![64; 10], 5, 2);
        let expected = [[0, 0, 0, 0, 0], [0, 0, 255, 0, 0]];

        for (y, row) in expected.iter().enumerate() {
            for (x, &value) in row.iter().enumerate() {
                assert_eq!(image.get_pixel(x as u32, y as u32).0, [value; 3]);
            }
        }
    }

    #[test]
    fn android_error_diffusion_truncates_negative_division_toward_zero() {
        let image = diffuse_android_error(vec![128, 147, 0, 0], 4, 1);

        assert_eq!(image.get_pixel(0, 0).0, [255, 255, 255]);
        assert_eq!(image.get_pixel(1, 0).0, [255, 255, 255]);
    }

    #[test]
    fn small_image_is_enlarged_to_paper_width_with_floor_height() {
        let source = RgbaImage::from_pixel(10, 3, Rgba([0, 0, 0, 255]));
        let bytes = encode_png(&source);

        let image = load_image_bytes(&bytes).expect("Small image should load");

        assert_eq!(image.dimensions(), (CANVAS_WIDTH, 115));
    }

    #[test]
    fn photo_output_contains_only_black_and_white() {
        let mut source = RgbaImage::new(24, 8);
        for (x, y, pixel) in source.enumerate_pixels_mut() {
            let red = (x * 255 / 23) as u8;
            let green = (y * 255 / 7) as u8;
            *pixel = Rgba([red, green, 255 - red, 255]);
        }
        let bytes = encode_png(&source);

        let image = load_image_bytes(&bytes).expect("Gradient image should load");

        assert!(
            image
                .pixels()
                .all(|pixel| pixel.0 == [0, 0, 0] || pixel.0 == [255, 255, 255])
        );
    }

    #[test]
    fn bright_midtones_retain_detail_through_error_diffusion() {
        let source = RgbaImage::from_pixel(24, 4, Rgba([180, 180, 180, 255]));
        let bytes = encode_png(&source);

        let image = load_image_bytes(&bytes).expect("Midtone image should load");
        let black_pixels = image.pixels().filter(|pixel| pixel.0 == [0, 0, 0]).count();

        assert!(black_pixels > 0);
        assert!(black_pixels < image.width() as usize * image.height() as usize);
    }

    #[test]
    fn oversized_encoded_image_is_rejected_before_decoding() {
        let bytes = vec![0; MAX_ENCODED_IMAGE_BYTES + 1];

        let error = load_image_bytes(&bytes).expect_err("Oversized input should be rejected");

        assert_eq!(error.to_string(), "Encoded image exceeds the size limit");
    }

    #[test]
    fn excessive_source_dimensions_and_pixels_are_rejected() {
        let dimension_error = validate_source_dimensions(MAX_SOURCE_DIMENSION + 1, 1)
            .expect_err("Oversized dimension should be rejected");
        let pixel_error = validate_source_dimensions(4097, 4097)
            .expect_err("Excessive pixel count should be rejected");

        assert_eq!(
            dimension_error.to_string(),
            "Image dimensions exceed the limit"
        );
        assert_eq!(pixel_error.to_string(), "Image contains too many pixels");
    }

    #[test]
    fn image_path_applies_decode_limits_before_loading_pixels() {
        let source = RgbaImage::new(MAX_SOURCE_DIMENSION + 1, 1);
        let temp = TempImage::save(&source);

        let error = load_image(&temp.path).expect_err("Oversized image should be rejected");

        assert_eq!(error.to_string(), "Failed to inspect image dimensions");
    }

    #[test]
    fn vertical_stack_centers_sections_and_adds_white_spacing() {
        let top = RgbImage::from_pixel(CANVAS_WIDTH, 2, Rgb([0, 0, 0]));
        let bottom = RgbImage::from_pixel(2, 1, Rgb([255, 0, 0]));

        let image = stack_vertical(&[top, bottom]).expect("Images should stack");
        let bottom_x = (CANVAS_WIDTH - 2) / 2;
        let bottom_y = 2 + STACK_SPACING;

        assert_eq!(image.dimensions(), (CANVAS_WIDTH, bottom_y + 1));
        assert_eq!(image.get_pixel(0, 0).0, [0, 0, 0]);
        assert_eq!(image.get_pixel(0, 2).0, [255, 255, 255]);
        assert_eq!(image.get_pixel(bottom_x, bottom_y).0, [255, 0, 0]);
        assert_eq!(image.get_pixel(bottom_x - 1, bottom_y).0, [255, 255, 255]);
    }

    #[test]
    fn vertical_stack_rejects_empty_input_and_protocol_height_overflow() {
        let empty_error = stack_vertical(&[]).expect_err("Empty stack should be rejected");
        let tall = RgbImage::new(1, MAX_CANVAS_HEIGHT);
        let tail = RgbImage::new(1, 1);
        let height_error =
            stack_vertical(&[tall, tail]).expect_err("Protocol height overflow should fail");

        assert_eq!(
            empty_error.to_string(),
            "At least one image section is required"
        );
        assert_eq!(height_error.to_string(), "Combined image is too tall");
    }

    #[test]
    fn zero_sized_image_is_rejected() {
        let source = RgbaImage::new(0, 1);

        let error = scale_android_rgb565(&source, 1).expect_err("Empty image should be rejected");

        assert_eq!(error.to_string(), "Image dimensions must be non-zero");
    }
}
