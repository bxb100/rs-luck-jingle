#![allow(unused)]

use std::error::Error;
use std::time::Duration;

use btleplug::api::{Central, Manager as _, Peripheral as _, ScanFilter, ValueNotification, WriteType};
use btleplug::platform::{Adapter, Manager, Peripheral};
pub use futures::stream::StreamExt;
use tokio::time;
use uuid::Uuid;

use ble_example::instruction::*;
use ble_example::printer_image::create_printer_command;

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

        let cmd_char = find_char(WRITE_UUID);
        let commands = create_printer_command(image::open("./res/fox.png").unwrap());
        printer.write(&cmd_char, &write(commands), WriteType::WithResponse).await?;

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
    commands.iter()
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
        _ => Err("wtf").unwrap()
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

