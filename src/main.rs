#![allow(unused)]

use std::error::Error;
use std::num::ParseIntError;
use std::time::Duration;

use ble_example::dither::DitherApply;
use ble_example::hex::decode_hex;
use ble_example::image::generate_image;
use btleplug::api::{
    Central, Manager as _, Peripheral as _, ScanFilter, ValueNotification, WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
pub use futures::stream::StreamExt;
use image::{DynamicImage, GrayImage, Luma};
use imageproc::drawing::{draw_text_mut, text_size};
use rusttype::{Font, Scale};
use tokio::time;
use uuid::Uuid;

use ble_example::instruction::*;

async fn get_central(manager: &Manager) -> Adapter {
    let adapters = manager.adapters().await.unwrap();
    adapters.into_iter().next().unwrap()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let manager = Manager::new().await.unwrap();

    // get the first bluetooth adapter
    // connect to the adapter
    let central = get_central(&manager).await;

    // start scanning for devices
    central.start_scan(ScanFilter::default()).await?;
    // instead of waiting, you can use central.events() to get a stream which will
    // notify you of new devices, for an example of that see examples/event_driven_discovery.rs
    time::sleep(Duration::from_secs(2)).await;

    let peripherals = central.peripherals().await?;
    if peripherals.is_empty() {
        eprintln!("->>> BLE peripheral devices were not found, sorry. Exiting...");
    } else {
        let printer = find_printer(peripherals).await.unwrap();
        println!("{:?}", printer);

        // connect to the device
        printer.connect().await?;

        // discover services and characteristics
        printer.discover_services().await?;

        // find the characteristic we want
        let chars = printer.characteristics();

        let find_char = |uuid: Uuid| {
            chars
                .iter()
                .find(|c| c.uuid == uuid)
                .expect("unable to find characteristics")
        };

        // let cmd_char = find_char(WRITE_UUID);
        // let commands = create_printer_command(image::open("./res/fox.png").unwrap());
        // printer
        //     .write(cmd_char, &write(commands), WriteType::WithResponse)
        //     .await?;
        // let a = String::from("hello");
        // let b = String::from(" world");
        // let c = a + &b;
        // let string = "A".to_string() + "b";
        //
        // time::sleep(Duration::from_secs(2)).await;

        let cmd_char = find_char(WRITE_UUID);

        printer
            .write(
                cmd_char,
                DISABLE_SHUTDOWN.as_slice(),
                WriteType::WithResponse,
            )
            .await?;

        printer
            .write(cmd_char, SET_THICKNESS.as_slice(), WriteType::WithResponse)
            .await?;

        let buffer = generate_image(None, Some("你有新的报道")).unwrap();
        // let buffer = generate_image(Some("./res/img.png"), None).unwrap();
        // let buffer = generate_image(None, Some("哇哈哈哈哈哈哈哈哈哈哈哈哈哈啊哈哈哈哈 Molestiae et voluptatem quos maxime eius reiciendis. Ullam deleniti aspernatur deleniti qui dolorem minus voluptatum non beatae. Consequatur quia eos quidem magni dolorem velit et dolores eum a enim. Libero et rerum voluptatem placeat vitae similique nemo aut id dolores. Dolorum consequatur doloribus perspiciatis. Et omnis eius quam deserunt dicta laborum repudiandae. Voluptates quam et occaecati et dolorum temporibus. rem Officia Impedit Eum Voluptas Ut Similique")).unwrap();

        let mut dither_apply = DitherApply::new(buffer);
        let image_hex_str = dither_apply.make_image_hex_str();

        let hex_len = format!("{:X}", (image_hex_str.len() / 96) + 3);
        let mut front_hex = hex_len.clone();
        let mut end_hex = String::from("0");

        if hex_len.len() > 2 {
            front_hex = hex_len[1..3].to_string();
            end_hex += hex_len[0..1].to_string().as_str();
        } else {
            end_hex += "0";
        }

        let mut data = format!(
            "{:0<32}",
            String::from("1D7630003000") + &*front_hex + &*end_hex
        );
        data += &image_hex_str[0..224];

        printer
            .write(
                cmd_char,
                decode_hex(data.as_str()).unwrap().as_slice(),
                WriteType::WithResponse,
            )
            .await?;

        // send image data in chunks
        for i in (224..image_hex_str.len()).step_by(256) {
            let mut str = &*format!("{:0<256}", unsafe {
                image_hex_str.get_unchecked(i..i + 256)
            });
            unsafe {
                printer
                    .write(
                        cmd_char,
                        decode_hex(str).unwrap().as_slice(),
                        WriteType::WithResponse,
                    )
                    .await?;
            }
        }

        printer
            .write(
                cmd_char,
                PRINTER_WAKE_MAGIC_END.as_slice(),
                WriteType::WithResponse,
            )
            .await?;
        printer
            .write(
                cmd_char,
                STOP_PRINT_JOBS.as_slice(),
                WriteType::WithResponse,
            )
            .await?;

        time::sleep(Duration::from_secs(2)).await;
        // let read_char = find_char(READ_UUID_1);
        //
        // printer.subscribe(&read_char).await?;

        // let mut notifications = printer.notifications().await?;
        // while let Some(data) = notifications.next().await {
        //     notification_handler(data).unwrap();
        // };
    }

    Ok(())
}

fn write(commands: Vec<BLEMessage>) -> Vec<u8> {
    commands
        .iter()
        .flat_map(|msg| msg.payload.clone())
        .collect()
}

fn notification_handler(data: ValueNotification) -> Result<(), Box<dyn Error>> {
    match data.uuid {
        READ_UUID_1 => {
            println!("feature 1: {:x?}", data.value);
        }
        READ_UUID_2 => {
            println!("feature 2: {:x?}", data.value);
        }
        _ => Err("wtf").unwrap(),
    }

    Ok(())
}

async fn find_printer(peripherals: Vec<Peripheral>) -> Option<Peripheral> {
    for p in peripherals {
        if p.properties()
            .await
            .unwrap()
            .unwrap()
            .local_name
            .iter()
            .any(|name| name.contains(PRINTER_NAME_PREFIX))
        {
            return Some(p);
        }
    }
    None
}
