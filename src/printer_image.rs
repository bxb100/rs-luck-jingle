pub use image::{self, DynamicImage, GrayImage, imageops::*};
use lebe::prelude::*;

use crate::instruction::*;

pub trait ImageEffect {
    fn black_and_white_scale_effect(&self) -> GrayImage;
}

impl ImageEffect for DynamicImage {
    fn black_and_white_scale_effect(&self) -> GrayImage {
        let img = self.resize(PRINTER_WIDTH, self.height(), Gaussian);
        let mut img = img.grayscale().to_luma8();
        dither(&mut img, &BiLevel);
        img.to_owned()
    }
}

pub fn create_printer_command(image: DynamicImage) -> Vec<BLEMessage> {
    let mut commands = vec![
        BLEMessage { payload: ENABLE_PRINTER.clone() },
        BLEMessage { payload: SET_THICKNESS.clone() },
    ];
    commands.append(&mut prep_image_data(image));
    commands.push(BLEMessage { payload: PRINTER_WAKE_MAGIC_END.clone() });
    commands.push(BLEMessage { payload: PRINT_LINE_DOTS.clone() });
    commands.push(BLEMessage { payload: STOP_PRINT_JOBS.clone() });
    commands
}

pub fn prep_image_data(img: DynamicImage) -> Vec<BLEMessage> {
    let mut image_command: Vec<BLEMessage> = vec![
        BLEMessage {
            payload: IMAGE_COMMAND_HEADER.clone()
        }
    ];

    let img = img.black_and_white_scale_effect();

    let (w, h) = img.dimensions();

    {
        let mut header: Vec<u8> = Vec::new();
        let mode: u8 = 0x0;
        // mode
        header.push(mode.from_current_into_little_endian());
        // width
        header.push(((w / 8) as u16)
            .from_current_into_little_endian()
            .try_into().unwrap_or(0)
        );
        // height
        header.push((h).from_current_into_little_endian().try_into().unwrap_or(0));

        image_command.push(BLEMessage { payload: header });
    }

    for y in 0..h {
        let mut line_command: Vec<u8> = Vec::new();
        for scanner in 0..(w / 8) {
            let head = scanner * 8;
            let mut this_byte: u8 = 0;
            for i in 0..8 {
                let x = head + i;
                if x > w {
                    continue;
                }
                let color = img.get_pixel(x, y);
                if color.0.get(0).map_or(0_f64, |it| (*it as f64) / 255_f64) < 0.5 {
                    this_byte |= 1;
                }
                if i < 7 { this_byte <<= 1; }
            }
            line_command.push(this_byte.from_current_into_little_endian());
        }
        image_command.push(BLEMessage {
            payload: line_command
        });
    }
    image_command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_gray() {
        let img = image::open("./res/fox.png").unwrap();
        let image_buffer = img.black_and_white_scale_effect();
        image_buffer.save("./res/ok.png").unwrap();
    }
}