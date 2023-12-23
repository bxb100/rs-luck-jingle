use image::imageops::{dither, BiLevel, Gaussian};
use image::{GrayImage, Luma, Rgb, RgbImage};
use imageproc::definitions::HasBlack;
use imageproc::drawing::draw_text_mut;
use rusttype::{Font, Scale};

/// https://github.com/yihong0618/blue
/// https://github.com/andelf/text-image
pub fn generate_image(src: Option<&str>, text: Option<&str>) -> Result<GrayImage, String> {
    if let Some(src) = src {
        return image::open(src)
            .map(|img| img.resize(384, img.height(), Gaussian))
            .map(|img| img.grayscale().to_luma8())
            .map_err(|e| e.to_string());
    } else if let Some(text) = text {
        // add --- in front and behind
        let mut text = format!("{}\n{}\n", "-".repeat(27), text);
        text.push_str(&" ".repeat(27 * 5));
        let font_size = 20.0;
        let font_raw = std::fs::read("res/zpix.ttf").expect("Can not read font file");
        let font = Font::try_from_vec(font_raw).unwrap();
        let scale = Scale {
            x: font_size,
            y: font_size,
        };

        // use 30 as line height
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
            if (line_length + l) > 17f32 {
                content.push('\n');
                line_length = 0f32;
            }
            line_length += l;
            content.push(c);
        }
        content = dbg!(content);
        let line_cnt = (content.lines().count() + 1) as u32;
        let mut image = GrayImage::new(384, (font_size as u32 + 2) * line_cnt);
        // background is white
        image.fill(0xFF);
        for (i, line) in content.lines().enumerate() {
            // 1 px offset for blending
            draw_text_mut(
                &mut image,
                Luma::black(),
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
    let image_buffer
        = generate_image(None, Some("哇哈哈哈哈哈哈哈哈哈哈哈哈哈啊哈哈哈哈 Molestiae et voluptatem quos maxime eius reiciendis. Ullam deleniti aspernatur deleniti qui dolorem minus voluptatum non beatae. Consequatur quia eos quidem magni dolorem velit et dolores eum a enim. Libero et rerum voluptatem placeat vitae similique nemo aut id dolores. Dolorum consequatur doloribus perspiciatis. Et omnis eius quam deserunt dicta laborum repudiandae. Voluptates quam et occaecati et dolorum temporibus. rem Officia Impedit Eum Voluptas Ut Similique")).unwrap();

    image_buffer
        .save("./res/test_generate_text_image.png")
        .unwrap();
}
#[test]
fn test_image() {
    let image_buffer = generate_image(Some("./res/fox.png"), None).unwrap();

    image_buffer.save("./res/test_image.png").unwrap();
}
