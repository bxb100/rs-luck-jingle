use image::{Rgb, RgbImage};
use rs_luck_jingle::protocol::{
    Density, PRINT_WIDTH_DOTS, PrinterStatus, enable_printer, encode_raster, feed_dots,
    is_ok_response, is_stop_ack, parse_status, query_status, set_density, stop_print_job,
    wake_printer,
};
use rs_luck_jingle::session::SessionConfig;
use rs_luck_jingle::transport::{MAX_WRITE_CHUNK, SPP_UUID};
use serde_json::Value;

fn fixture() -> Value {
    serde_json::from_str(include_str!("../spec/fixtures/d1x-classic-vectors.json"))
        .expect("protocol fixture must be valid JSON")
}

fn decode_hex(value: &str) -> Vec<u8> {
    value
        .split_ascii_whitespace()
        .map(|octet| {
            u8::from_str_radix(octet, 16)
                .unwrap_or_else(|error| panic!("invalid hex octet {octet:?}: {error}"))
        })
        .collect()
}

fn hex_field(value: &Value, field: &str) -> Vec<u8> {
    decode_hex(
        value[field]
            .as_str()
            .unwrap_or_else(|| panic!("{field} must be a hex string")),
    )
}

fn u8_field(value: &Value, field: &str) -> u8 {
    value[field]
        .as_u64()
        .and_then(|number| u8::try_from(number).ok())
        .unwrap_or_else(|| panic!("{field} must fit in a byte"))
}

fn u32_field(value: &Value, field: &str) -> u32 {
    value[field]
        .as_u64()
        .and_then(|number| u32::try_from(number).ok())
        .unwrap_or_else(|| panic!("{field} must fit in a u32"))
}

fn raster_image(vector: &Value) -> RgbImage {
    let name = vector["name"]
        .as_str()
        .expect("raster vector must have a name");
    let width = u32_field(vector, "width");
    let height = u32_field(vector, "height");
    let rows = vector["pixels"]
        .as_array()
        .expect("pixels must be an array of rows");

    assert_eq!(rows.len(), height as usize, "{name}");

    let mut image = RgbImage::new(width, height);
    for (y, row) in rows.iter().enumerate() {
        let pixels = row.as_array().expect("each pixel row must be an array");
        assert_eq!(pixels.len(), width as usize, "{name}");

        for (x, pixel) in pixels.iter().enumerate() {
            let channel = match pixel.as_str() {
                Some("black") => 0,
                Some("white") => 255,
                other => panic!("unsupported pixel {other:?} in {name}"),
            };
            image.put_pixel(x as u32, y as u32, Rgb([channel, channel, channel]));
        }
    }

    image
}

#[test]
fn fixture_commands_match_protocol_bytes() {
    let fixture = fixture();
    let commands = &fixture["commands"];

    assert_eq!(fixture["schema_version"].as_u64(), Some(1));
    assert_eq!(fixture["protocol"].as_str(), Some("luckp-d1x-classic"));
    assert_eq!(
        u32_field(&fixture["profile"], "print_width_dots"),
        PRINT_WIDTH_DOTS
    );
    assert_eq!(
        enable_printer().as_slice(),
        hex_field(&commands["enable"], "request_hex")
    );
    assert_eq!(
        wake_printer().as_slice(),
        hex_field(&commands["wake"], "request_hex")
    );
    assert_eq!(
        query_status().as_slice(),
        hex_field(&commands["status"], "request_hex")
    );
    assert_eq!(
        feed_dots(u8_field(&commands["feed_dots"], "default_dots")).as_slice(),
        hex_field(&commands["feed_dots"], "default_request_hex")
    );
    assert_eq!(
        stop_print_job().as_slice(),
        hex_field(&commands["stop_job"], "request_hex")
    );
}

#[test]
fn fixture_transport_and_timeouts_match_runtime_defaults() {
    let fixture = fixture();
    let transport = &fixture["transport"];
    let commands = &fixture["commands"];
    let config = SessionConfig::default();

    assert_eq!(transport["kind"].as_str(), Some("bluetooth-classic-rfcomm"));
    assert_eq!(transport["service_uuid"].as_str(), Some(SPP_UUID));
    assert_eq!(transport["observed_rfcomm_channel"].as_u64(), Some(1));
    assert_eq!(transport["channel_source"].as_str(), Some("sdp"));
    assert_eq!(
        transport["logical_write_chunk_bytes"].as_u64(),
        Some(MAX_WRITE_CHUNK as u64)
    );
    assert_eq!(
        transport["write_delay_ms"].as_u64(),
        Some(config.command_delay.as_millis() as u64)
    );
    assert_eq!(
        commands["status"]["timeout_ms"].as_u64(),
        Some(config.response_timeout.as_millis() as u64)
    );
    assert_eq!(
        commands["set_density"]["timeout_ms"].as_u64(),
        Some(config.response_timeout.as_millis() as u64)
    );
    assert_eq!(
        commands["stop_job"]["timeout_ms"].as_u64(),
        Some(config.stop_timeout.as_millis() as u64)
    );
}

#[test]
fn fixture_density_vectors_match_supported_levels_and_ack_rules() {
    let fixture = fixture();
    let profile = &fixture["profile"];
    let command = &fixture["commands"]["set_density"];
    let min = u8_field(&command["parameter"], "min");
    let max = u8_field(&command["parameter"], "max");

    assert_eq!(min, u8_field(profile, "density_min"));
    assert_eq!(max, u8_field(profile, "density_max"));

    let examples = command["examples"]
        .as_array()
        .expect("density examples must be an array");
    assert_eq!(examples.len(), usize::from(max - min + 1));

    let prefix = hex_field(command, "request_prefix_hex");
    for example in examples {
        let level = u8_field(example, "level");
        let density = Density::try_from(level)
            .unwrap_or_else(|error| panic!("invalid fixture density {level}: {error}"));
        let request = set_density(density);

        assert_eq!(
            request.as_slice(),
            hex_field(example, "request_hex"),
            "{level}"
        );
        assert_eq!(&request[..prefix.len()], prefix.as_slice(), "{level}");
    }

    assert!(Density::try_from(max.checked_add(1).expect("density max must not be 255")).is_err());

    let mut saw_accepted = false;
    let mut saw_rejected = false;
    for vector in fixture["response_vectors"]
        .as_array()
        .expect("response_vectors must be an array")
    {
        if vector["parser"].as_str() != Some("exact-ok") {
            continue;
        }

        let accepted = vector["accepted"]
            .as_bool()
            .expect("exact-ok vectors must declare acceptance");
        assert_eq!(
            is_ok_response(&hex_field(vector, "response_hex")),
            accepted,
            "{}",
            vector["name"]
        );
        saw_accepted |= accepted;
        saw_rejected |= !accepted;
    }

    assert!(saw_accepted, "fixture must include an accepted density ack");
    assert!(saw_rejected, "fixture must include a rejected density ack");
}

#[test]
fn fixture_status_vectors_map_documented_flags() {
    let fixture = fixture();
    let mut vector_count = 0;

    for vector in fixture["response_vectors"]
        .as_array()
        .expect("response_vectors must be an array")
    {
        if vector["parser"].as_str() != Some("status-byte") {
            continue;
        }

        vector_count += 1;
        let response = hex_field(vector, "response_hex");
        let expected = &vector["expected"];
        let status = parse_status(&response).unwrap_or_else(|error| {
            panic!("failed to parse status vector {}: {error}", vector["name"])
        });

        assert_eq!(
            status,
            PrinterStatus {
                raw: response[0],
                printing: expected["printing"]
                    .as_bool()
                    .expect("printing must be a boolean"),
                cover_open: expected["cover_open"]
                    .as_bool()
                    .expect("cover_open must be a boolean"),
                paper_out: expected["paper_out"]
                    .as_bool()
                    .expect("paper_out must be a boolean"),
                low_battery: expected["low_battery"]
                    .as_bool()
                    .expect("low_battery must be a boolean"),
                charging: expected["charging"]
                    .as_bool()
                    .expect("charging must be a boolean"),
                overheated: expected["overheated"]
                    .as_bool()
                    .expect("overheated must be a boolean"),
            },
            "{}",
            vector["name"]
        );
    }

    assert!(
        vector_count >= 3,
        "fixture must exercise status flag mapping"
    );
}

#[test]
fn fixture_stop_vectors_cover_acceptance_and_rejection() {
    let fixture = fixture();
    let mut saw_accepted = false;
    let mut saw_rejected = false;

    for vector in fixture["response_vectors"]
        .as_array()
        .expect("response_vectors must be an array")
    {
        if vector["parser"].as_str() != Some("first-aa-or-gb2312-ok-prefix") {
            continue;
        }

        let accepted = vector["accepted"]
            .as_bool()
            .expect("stop ack vectors must declare acceptance");
        assert_eq!(
            is_stop_ack(&hex_field(vector, "response_hex")),
            accepted,
            "{}",
            vector["name"]
        );
        saw_accepted |= accepted;
        saw_rejected |= !accepted;
    }

    assert!(saw_accepted, "fixture must include an accepted stop ack");
    assert!(saw_rejected, "fixture must include a rejected stop ack");
}

#[test]
fn fixture_raster_vectors_encode_exact_frames() {
    let fixture = fixture();
    let profile = &fixture["profile"];
    let vectors = fixture["raster_vectors"]
        .as_array()
        .expect("raster_vectors must be an array");

    assert_eq!(u8_field(profile, "raster_mode"), 0);
    assert_eq!(u8_field(profile, "black_threshold"), 128);
    assert_eq!(profile["bit_order"].as_str(), Some("msb-first"));
    assert_eq!(u8_field(profile, "row_padding_bit"), 0);
    assert!(
        vectors.len() >= 2,
        "fixture must contain at least two raster vectors"
    );

    for vector in vectors {
        let name = vector["name"]
            .as_str()
            .expect("raster vector must have a name");
        let width = u32_field(vector, "width");
        let bytes_per_row = u32_field(vector, "bytes_per_row");

        assert_eq!(
            u8_field(vector, "mode"),
            u8_field(profile, "raster_mode"),
            "{name}"
        );
        assert_eq!(bytes_per_row, width.div_ceil(8), "{name}");
        if let Some(right_padding_bits) = vector["right_padding_bits"].as_u64() {
            assert_eq!(right_padding_bits, u64::from((8 - width % 8) % 8), "{name}");
        }

        let encoded = encode_raster(&raster_image(vector))
            .unwrap_or_else(|error| panic!("failed to encode {name}: {error}"));
        let header = hex_field(vector, "header_hex");
        let payload = hex_field(vector, "payload_hex");

        assert_eq!(&encoded[..header.len()], header.as_slice(), "{name}");
        assert_eq!(&encoded[header.len()..], payload.as_slice(), "{name}");
        assert_eq!(encoded, hex_field(vector, "frame_hex"), "{name}");
    }
}
