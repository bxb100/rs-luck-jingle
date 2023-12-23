use image::imageops::{dither, ColorMap};
use image::{Rgb, RgbImage};

pub struct DitherApply {
    size: (u32, u32),
    buff: RgbImage,
}
struct BiLevel2;

const CONTRAST: f64 = 1.45 * 1.45;
const BRIGHTNESS: f64 = 0.35;
impl ColorMap for BiLevel2 {
    type Color = Rgb<u8>;

    fn index_of(&self, color: &Self::Color) -> usize {
        let [r, g, b] = color.0;

        let old = (r as i16 + g as i16 + b as i16) / 3;
        if old < 128 {
            0
        } else {
            255
        }
    }

    #[inline]
    fn map_color(&self, color: &mut Self::Color) {
        let mut new_color = [0u8; 3];
        for (i, x) in color.0.iter().enumerate() {
            let mut t = *x as f64;
            t += (BRIGHTNESS - 0.5) * 256f64;
            t = (t - 128f64) * CONTRAST + 128f64;
            new_color[i] = t.clamp(0f64, 255f64) as u8;
        }
        let n = self.index_of(&Rgb(new_color));

        *color = Rgb([n as u8, n as u8, n as u8]);
    }
}

impl DitherApply {
    pub fn new(buff: RgbImage) -> Self {
        DitherApply {
            size: buff.dimensions(),
            buff,
        }
    }

    pub fn make_image_hex_str(&mut self) -> String {
        dither(&mut self.buff, &BiLevel2);
        let (x, y) = self.size;
        let mut image_bin_str: String = String::new();

        for y in 0..y {
            for x in 0..x {
                let [r, g, b] = self.buff.get_pixel(x, y).0;
                image_bin_str.push(if r as u16 + g as u16 + b as u16 > 600 {
                    '0'
                } else {
                    '1'
                });
            }
        }

        // start bits
        let offset = 4 - (image_bin_str.len() + 1) % 4;
        image_bin_str = format!("1{}{image_bin_str}", "0".repeat(316 + offset));

        // binary to hex
        let mut hex_string = String::new();
        for counter in (0..image_bin_str.len()).step_by(4) {
            let chunk = image_bin_str.get(counter..counter + 4).unwrap();
            hex_string.push_str(to_hex(chunk));
        }
        hex_string
    }
}

fn to_hex(b: &str) -> &str {
    match b {
        "0000" => "0",
        "0001" => "1",
        "0010" => "2",
        "0011" => "3",
        "0100" => "4",
        "0101" => "5",
        "0110" => "6",
        "0111" => "7",
        "1000" => "8",
        "1001" => "9",
        "1010" => "A",
        "1011" => "B",
        "1100" => "C",
        "1101" => "D",
        "1110" => "E",
        "1111" => "F",
        // catch
        _ => panic!("Invalid binary string"),
    }
}
