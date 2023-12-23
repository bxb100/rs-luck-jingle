use image::GrayImage;

pub struct DitherApply {
    size: (u32, u32),
    buff: GrayImage,
}

impl DitherApply {
    pub fn new(buff: GrayImage) -> Self {
        DitherApply {
            size: buff.dimensions(),
            buff,
        }
    }

    pub fn make_image_hex_str(&mut self) -> String {
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
