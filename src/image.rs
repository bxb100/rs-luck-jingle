use image::imageops::Gaussian;
use image::{Rgb, RgbImage};
use imageproc::definitions::HasBlack;
use imageproc::drawing::draw_text_mut;
use rusttype::{Font, Scale};

/// https://github.com/yihong0618/blue
/// https://github.com/andelf/text-image
pub fn generate_image(src: Option<&str>, text: Option<&str>) -> Result<RgbImage, String> {
    if let Some(src) = src {
        return image::open(src)
            .map(|img| img.resize(384, img.height(), Gaussian))
            .map(|img| img.to_rgb8())
            .map_err(|e| e.to_string());
    } else if let Some(text) = text {
        // add --- in front and behind
        let mut text = format!("{}\n{}\n", "-".repeat(27), text);
        text.push_str(&" ".repeat(27 * 5));
        let font_size = 24.0;
        let font_raw = std::fs::read("res/zpix.ttf").expect("Can not read font file");
        let font = Font::try_from_vec(font_raw).unwrap();
        let scale = Scale {
            x: font_size,
            y: font_size,
        };

        // fixme: use 30 as line height?
        // let metric = font.v_metrics(scale);
        // let line_height = (metric.ascent - metric.descent + metric.line_gap)
        //     .abs()
        //     .ceil() as i32;

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
            if (line_length + l) > (384f32 / font_size).floor() - 2f32 {
                content.push('\n');
                line_length = 0f32;
            }
            line_length += l;
            content.push(c);
        }
        log::debug!("content: {}", content);

        let line_cnt = (content.lines().count() + 1) as u32;
        let mut image = RgbImage::new(384, (font_size as u32 + 2) * line_cnt);
        // background is white
        image.fill(0xFF);
        for (i, line) in content.lines().enumerate() {
            // 1 px offset for blending
            draw_text_mut(
                &mut image,
                Rgb::black(),
                1,
                (30 * i) as i32,
                scale,
                &font,
                line,
            );
        }
        return Ok(image);
    }

    Err("Either src or text must be provided".to_string())
}

#[test]
fn test_generate_text_image() {
    let text = "2023-12-23 17:48:33\n\
    REPO: bxb100/ble-example\n\
    新的 ISSUE 来了来了来了！\n\
    ISSUE Title: CS2\n\
    Content:\n\
     带图片\n\
    ![fox](https://github.com/bx\n\
    b100/ble-example/assets/2068\n\
    5961/9c98eba0-37aa-4844-bbd4\n\
    -621a5bc278f4)\n";

    let image_buffer = generate_image(None, Some(text)).unwrap();

    image_buffer
        .save("./res/test_generate_text_image.png")
        .unwrap();
}
#[test]
fn test_image() {
    let image_buffer = generate_image(Some("./res/fox.png"), None).unwrap();

    image_buffer.save("./res/test_image.png").unwrap();
}
