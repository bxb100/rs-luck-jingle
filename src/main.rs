#![allow(unused)]

use actix_web::http::header::Charset::Iso_8859_1;
use actix_web::http::header::HeaderValue;
use actix_web::rt::time;
use actix_web::web::Data;
use actix_web::{get, post, web, App, HttpRequest, HttpResponse, HttpServer, Responder};
use std::error::Error;
use std::io::ErrorKind;
use std::num::ParseIntError;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ble_example::dither::DitherApply;
use ble_example::hex::decode_hex;
use ble_example::image::generate_image;
use btleplug::api::{
    Central, Characteristic, Manager as _, Peripheral as _, ScanFilter, ValueNotification,
    WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use image::{DynamicImage, GrayImage, Luma};
use imageproc::drawing::{draw_text_mut, text_size};
use rusttype::{Font, Scale};
use serde::Deserialize;
use uuid::Uuid;

use anyhow::Result;
use ble_example::instruction::*;
use chrono::Utc;
use lazy_static::lazy_static;
use regex::Regex;

async fn get_central(manager: &Manager) -> Adapter {
    let adapters = manager.adapters().await.unwrap();
    adapters.into_iter().next().unwrap()
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let manager = Manager::new().await.unwrap();

    // get the first bluetooth adapter
    // connect to the adapter
    let central = get_central(&manager).await;

    // start scanning for devices
    central
        .start_scan(ScanFilter::default())
        .await
        .map_err(|e| std::io::Error::new(ErrorKind::Interrupted, e))?;
    // instead of waiting, you can use central.events() to get a stream which will
    // notify you of new devices, for an example of that see examples/event_driven_discovery.rs
    time::sleep(Duration::from_secs(2)).await;

    let peripherals = central
        .peripherals()
        .await
        .map_err(|e| std::io::Error::new(ErrorKind::Interrupted, e))?;
    if peripherals.is_empty() {
        panic!("->>> BLE peripheral devices were not found, sorry. Exiting...");
    }

    let printer = find_printer(peripherals)
        .await
        .expect("printer not start or it is linking other device");
    println!("{:?}", printer);

    // connect to the device
    printer
        .connect()
        .await
        .map_err(|e| std::io::Error::new(ErrorKind::Interrupted, e))?;

    // discover services and characteristics
    printer
        .discover_services()
        .await
        .map_err(|e| std::io::Error::new(ErrorKind::Interrupted, e))?;

    // find the characteristic we want
    let chars = printer.characteristics();

    let find_char = |uuid: Uuid| {
        chars
            .iter()
            .find(|c| c.uuid == uuid)
            .expect("unable to find characteristics")
    };
    let cmd_char = Data::new(find_char(WRITE_UUID).clone());

    printer
        .write(
            &cmd_char,
            DISABLE_SHUTDOWN.as_slice(),
            WriteType::WithResponse,
        )
        .await
        .map_err(|e| std::io::Error::new(ErrorKind::Interrupted, e))?;

    printer
        .write(&cmd_char, SET_THICKNESS.as_slice(), WriteType::WithResponse)
        .await
        .map_err(|e| std::io::Error::new(ErrorKind::Interrupted, e))?;

    let shared_printer = Data::new(Mutex::new(printer));

    env_logger::init_from_env(env_logger::Env::new().default_filter_or("info"));

    HttpServer::new(move || {
        App::new()
            .app_data(cmd_char.clone())
            .app_data(shared_printer.clone())
            .service(index)
            .service(github_webhooks)
    })
    .bind(("127.0.0.1", 5444))?
    .run()
    .await
}

#[get("/")]
async fn index() -> impl Responder {
    "Ok"
}

lazy_static! {
    static ref DEFAULT_HEADER: HeaderValue = HeaderValue::from_static("none");
    static ref LINK_REGEX: Regex = Regex::new(r"!?\[.*?]\(.*?\)").unwrap();
}
#[post("/github-webhooks")]
async fn github_webhooks(
    printer: Data<Mutex<Peripheral>>,
    cmd_char: Data<Characteristic>,
    hook: web::Json<GithubWebhook>,
    req: HttpRequest,
) -> impl Responder {
    let github_event = req
        .headers()
        .get("X-GitHub-Event")
        .unwrap_or(&DEFAULT_HEADER)
        .to_str()
        .unwrap();
    log::debug!("hook: {:?}", hook);

    let now = Utc::now().format("%Y-%m-%d %H:%M:%S");
    let str = if (github_event == "issues") {
        if (hook.0.action.unwrap() != "opened") {
            return HttpResponse::Ok().finish();
        }
        let issue = hook.0.issue.unwrap();
        format!(
            "{}\nREPO: {}\n新的 ISSUE 来了来了来了！\n\
            ISSUE Title: {}\nContent:\n {}",
            now,
            hook.0.repository.full_name,
            issue.title,
            truncate(
                LINK_REGEX
                    .replace_all(issue.body.unwrap_or("".to_string()).trim(), "")
                    .as_ref(),
                60
            )
        )
    } else if (github_event == "issue_comment") {
        if (hook.0.action.unwrap() != "created") {
            return HttpResponse::Ok().finish();
        }

        format!(
            "{}\nREPO: {}\nISSUE: {}\n{} 刚刚留下了评论",
            now,
            hook.0.repository.full_name,
            hook.0.issue.unwrap().title,
            hook.0.comment.unwrap().user.unwrap().login
        )
    } else if (github_event == "ping") {
        format!(
            "{}\nREPO: {}\n{}\n ---- SETUP DONE --- ",
            now,
            hook.0.repository.full_name,
            hook.0.zen.unwrap()
        )
    } else {
        log::error!("Unhandled event: {:?}", github_event);
        return HttpResponse::BadRequest().finish();
    };

    call_printer(str.as_str(), &printer, &cmd_char)
        .await
        .unwrap();

    HttpResponse::Ok().finish()
}

fn truncate(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        None => s,
        Some((idx, _)) => &s[..idx],
    }
}

#[derive(Debug, Deserialize)]
struct GithubWebhook {
    zen: Option<String>,
    action: Option<String>,
    issue: Option<Issue>,
    comment: Option<Comment>,
    repository: Repository,
}
#[derive(Debug, Deserialize)]
struct Repository {
    full_name: String,
}
#[derive(Debug, Deserialize)]
struct Comment {
    body: String,
    user: Option<User>,
}

#[derive(Debug, Deserialize)]
struct User {
    login: String,
}

#[derive(Debug, Deserialize)]
struct Issue {
    title: String,
    body: Option<String>,
}

#[allow(clippy::await_holding_lock)]
async fn call_printer(
    text: &str,
    printer: &Mutex<Peripheral>,
    cmd_char: &Characteristic,
) -> Result<(), Box<dyn Error>> {
    let printer = printer.lock().unwrap();

    let buffer = generate_image(None, Some(text)).unwrap();

    let mut dither_apply = DitherApply::new(buffer);
    let image_hex_str = dither_apply.make_image_hex_str();

    let hex_len = format!("{:X}", (image_hex_str.len() / 96) + 3);
    let mut front_hex = hex_len.clone();
    let mut end_hex = String::from("0");

    if hex_len.len() > 2 {
        front_hex = hex_len[1..].to_string();
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
            STOP_PRINT_JOBS.as_slice(),
            WriteType::WithResponse,
        )
        .await?;

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
