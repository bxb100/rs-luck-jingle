use anyhow::anyhow;
use image::imageops::Gaussian;
use image::{GenericImage, GenericImageView, ImageBuffer, Pixel, Primitive, Rgb, RgbImage};
use imageproc::definitions::HasBlack;
use imageproc::drawing::draw_text_mut;
use rust_embed::RustEmbed;
use rusttype::{Font, Scale};

#[derive(RustEmbed)]
#[folder = "res"]
#[include = "*.ttf"]
struct Asset;

static IMAGE_WIDTH: u32 = 384;

/// https://github.com/yihong0618/blue
/// https://github.com/andelf/text-image
pub fn generate_image(src: Option<&str>, text: Option<&str>) -> anyhow::Result<RgbImage> {
    if let Some(src) = src {
        return image::open(src)
            .map(|img| img.resize(IMAGE_WIDTH, img.height(), Gaussian))
            .map(|img| img.to_rgb8())
            .map_err(|e| anyhow::anyhow!("Can not open image: {}", e));
    } else if let Some(text) = text {
        // add --- in front and behind
        let mut text = format!("{}\n{}", "-".repeat(27), text);
        text.push_str(&" ".repeat(27 * 5));
        let font_size = 24.0;
        let file = Asset::get("zpix.ttf").unwrap();
        // let font_raw = std::fs::read(file).expect("Can not read font file");
        let font = Font::try_from_bytes(&file.data).unwrap();
        let scale = Scale {
            x: font_size,
            y: font_size,
        };

        // fixme: use 30 as line height?
        let metric = font.v_metrics(scale);
        let line_height = (metric.ascent - metric.descent + metric.line_gap)
            .abs()
            .ceil() as u32;

        // w is 384 can show 13.5 chinese characters or 27 english characters based on font size 20
        // calculate text need how many lines
        // calculate total height: lines * 30
        let mut content = String::new();
        let mut line_length = 0f32;

        for c in text.chars() {
            let mut l = 0f32;

            if c == '\n' {
                line_length = 0f32;
                content.push('\n');
            } else if (c as u32) <= 256 {
                l = 0.5;
            } else {
                l = 1.0;
            }
            // 17 = floor(384 / 20) - 2
            if (line_length + l) > (IMAGE_WIDTH as f32 / font_size).floor() - 2f32 {
                content.push('\n');
                line_length = 0f32;
            }
            line_length += l;
            content.push(c);
        }
        log::debug!("content: {}", content);

        let line_cnt = (content.lines().count() + 1) as u32;
        let mut image = RgbImage::new(IMAGE_WIDTH, line_height * line_cnt);
        // background is white
        image.fill(0xFF);
        for (i, line) in content.lines().enumerate() {
            // 1 px offset for blending
            draw_text_mut(
                &mut image,
                Rgb::black(),
                1,
                ((line_height + 2) * i as u32) as i32,
                scale,
                &font,
                line,
            );
        }
        return Ok(image);
    }

    Err(anyhow!("Either src or text must be provided"))
}

#[allow(dead_code)]
/// https://github.com/image-rs/image/issues/1571
fn v_concat<I, P, S>(images: &[I]) -> ImageBuffer<P, Vec<S>>
where
    I: GenericImageView<Pixel = P>,
    P: Pixel<Subpixel = S> + 'static,
    S: Primitive + 'static,
{
    let img_width_out: u32 = images
        .iter()
        .map(|im| im.width())
        .max()
        .unwrap_or(IMAGE_WIDTH);
    let img_height_out: u32 = images.iter().map(|im| im.height()).sum();

    let mut imgbuf = image::ImageBuffer::new(img_width_out, img_height_out);
    let mut accumulated_h = 0;

    for img in images {
        imgbuf.copy_from(img, 0, accumulated_h).unwrap();
        accumulated_h += img.height();
    }

    imgbuf
}

#[test]
fn test_generate_text_image() {
    let text = "2023-12-23 17:48:33\n\
    REPO: bxb100/rs-luck-jingle\n\
    新的 ISSUE 来了来了来了！\n\
    ISSUE Title: CS2\n\
    Content:\n\
     带图片\n\
    ![fox](https://github.com/bx\
    b100/rs-luck-jingle/assets/2068\
    5961/9c98eba0-37aa-4844-bbd4\
    -621a5bc278f4)\n";

    let mut image_buffer = generate_image(None, Some(text)).unwrap();

    image::imageops::dither(&mut image_buffer, &crate::dither::BiLevel2);

    image_buffer
        .save("./res/test_generate_text_image.png")
        .unwrap();
}

#[test]
fn test_concat_image() {
    let text = "2023-12-23 17:48:33";

    let image_buffer = generate_image(None, Some(text)).unwrap();

    let image_buffer_2 = generate_image(Some("./res/fox.png"), None).unwrap();

    let mut imgbuf = v_concat(&[image_buffer_2, image_buffer]);

    image::imageops::dither(&mut imgbuf, &crate::dither::BiLevel2);

    imgbuf.save("./res/test_concat.png").unwrap();
}

#[test]
fn test_image() {
    let mut image_buffer = generate_image(Some("./res/fox.png"), None).unwrap();

    image::imageops::dither(&mut image_buffer, &crate::dither::BiLevel2);

    image_buffer.save("./res/test_image.png").unwrap();
}
