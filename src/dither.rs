use image::imageops::{dither, BiLevel};
use image::GrayImage;

pub struct DitherApply {
    size: (u32, u32),
    buff: GrayImage,
}

impl DitherApply {
    pub fn new(mut buff: GrayImage) -> Self {
        dither(&mut buff, &BiLevel);

        DitherApply {
            size: buff.dimensions(),
            buff,
        }
    }

    // pub fn get_value(&self, x: u32, y: u32) -> u8 {
    //     let [r, g, b] = self.buff.get_pixel(x, y).0;
    //     ((r as u16 + g as u16 + b as u16) / 3) as u8
    // }
    //
    // pub fn set_value(&mut self, x: u32, y: u32, value: u8) {
    //     self.buff.put_pixel(x, y, Rgb([value, value, value]));
    // }
    //
    // pub fn nudge_value(&mut self, x: u32, y: u32, value: i16) {
    //     let [r, g, b] = self.buff.get_pixel(x, y).0;
    //
    //     self.buff.put_pixel(
    //         x,
    //         y,
    //         Rgb([
    //             (r as i16 + value).clamp(0, 255) as u8,
    //             (g as i16 + value).clamp(0, 255) as u8,
    //             (b as i16 + value).clamp(0, 255) as u8,
    //         ]),
    //     );
    // }

    const DITHERBRIGHTNESS: f64 = 0.35;
    const DITHERCONTRAST: f64 = 3.55;
    // fn make_apply(&mut self) {
    //     let (w, h) = self.size;
    //     let brightness = Self::DITHERBRIGHTNESS;
    //     let contrast = Self::DITHERCONTRAST * Self::DITHERCONTRAST;
    //     for y in 0..h {
    //         for x in 0..w {
    //             let [r, g, b] = self.buff.get_pixel(x, y).0;
    //             let mut arr = [r, g, b];
    //
    //             for i in 0..3 {
    //                 let mut t = arr[i] as f64;
    //                 t += (brightness - 0.5) * 256f64;
    //                 let t = ((t - 128f64) * contrast + 128f64) as u8;
    //                 arr[i] = t.clamp(0, 255);
    //             }
    //             self.buff.put_pixel(x, y, Rgb(arr));
    //         }
    //     }
    //
    //     for y in 0..h {
    //         let bottom_row = y == h - 1;
    //         for x in 0..w {
    //             let left_edge = x == 0;
    //             let right_edge = x == w - 1;
    //             // let i = (y * w + x) * 4;
    //             let level = self.get_value(x, y);
    //             let new_level = if level < 128 { 0 } else { 255 };
    //
    //             self.set_value(x, y, new_level);
    //             let err = level as i16 - new_level as i16;
    //             if !right_edge {
    //                 self.nudge_value(x + 1, y, err * 7 / 16);
    //             }
    //             if !bottom_row && !left_edge {
    //                 self.nudge_value(x - 1, y + 1, err * 3 / 16);
    //             }
    //             if !bottom_row {
    //                 self.nudge_value(x, y + 1, err * 5 / 16);
    //             }
    //             if !bottom_row && !right_edge {
    //                 self.nudge_value(x + 1, y + 1, err / 16);
    //             }
    //         }
    //     }
    // }

    pub fn make_image_hex_str(&mut self) -> String {
        // self.make_apply();
        // let (x, y) = self.size;
        // let mut image_bin_str: String = String::new();
        //
        // for y in 0..y {
        //     for x in 0..x {
        //         let [r, g, b] = self.buff.get_pixel(x, y).0;
        //         image_bin_str.push(if r as u16 + g as u16 + b as u16 > 600 {
        //             '0'
        //         } else {
        //             '1'
        //         });
        //     }
        // }
        //
        // // start bits
        // image_bin_str = format!("1{}{image_bin_str}", "0".repeat(318));
        //
        // // binary to hex
        // if (image_bin_str.len() % 4) != 0 {
        //     image_bin_str = "0".repeat(4 - (image_bin_str.len() % 4)) + &image_bin_str;
        // }
        //
        // let mut hex_string = String::new();
        // for counter in (0..image_bin_str.len()).step_by(4) {
        //     let chunk = image_bin_str.get(counter..counter + 4).unwrap();
        //     hex_string.push_str(to_hex(chunk));
        // }
        // hex_string

        let (x, y) = self.size;
        let mut image_bin_str: String = String::new();

        for y in 0..y {
            for x in 0..x {
                let [s] = self.buff.get_pixel(x, y).0;
                image_bin_str.push(if s > 0 { '0' } else { '1' });
            }
        }

        // start bits
        image_bin_str = format!("1{}{image_bin_str}", "0".repeat(318));

        // binary to hex
        if (image_bin_str.len() % 4) != 0 {
            image_bin_str = "0".repeat(4 - (image_bin_str.len() % 4)) + &image_bin_str;
        }

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
