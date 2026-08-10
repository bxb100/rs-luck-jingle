use anyhow::{Context, Result, bail};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use reqwest::Url;
use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, ACCEPT_ENCODING, CONTENT_TYPE, HeaderValue};
use reqwest::redirect;
use std::env;
use std::io::Read;
use std::time::Duration;

const MAX_MARKDOWN_IMAGES: usize = 4;
const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedMarkdown {
    pub text: String,
    pub image_urls: Vec<String>,
    pub has_omitted_images: bool,
}

pub struct MarkdownImageFetcher {
    client: Client,
}

impl MarkdownImageFetcher {
    pub fn from_env() -> Result<Self> {
        let client = build_image_client()?;

        Ok(Self { client })
    }

    pub fn fetch(&self, value: &str) -> Result<Vec<u8>> {
        let url = parse_image_url(value)?;
        let request = self
            .client
            .get(url)
            .header(ACCEPT, "image/*")
            .header(ACCEPT_ENCODING, "identity");

        let mut response = request
            .send()
            .map_err(|error| anyhow::Error::new(error.without_url()))
            .context("Markdown image request failed")?;

        if !response.status().is_success() {
            bail!(
                "Markdown image server returned HTTP {}",
                response.status().as_u16()
            );
        }

        validate_image_content_type(response.headers().get(CONTENT_TYPE))?;

        if response
            .content_length()
            .is_some_and(|length| length > MAX_IMAGE_BYTES as u64)
        {
            bail!("Markdown image exceeds the download size limit");
        }

        read_limited(&mut response, MAX_IMAGE_BYTES)
            .context("failed to read Markdown image response")
    }
}

fn build_image_client() -> Result<Client> {
    Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .redirect(redirect::Policy::default())
        .referer(false)
        .no_gzip()
        .no_brotli()
        .no_zstd()
        .no_deflate()
        .user_agent(concat!("rs-luck-jingle/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("failed to build the Markdown image HTTP client")
}

pub fn parse_markdown(value: &str) -> Result<ParsedMarkdown> {
    let parser = Parser::new_ext(value, Options::all());
    let mut text = String::new();
    let mut image_urls = Vec::new();
    let mut active_image: Option<ImageCapture> = None;
    let mut html_state = HtmlScanState::default();
    let mut has_omitted_images = false;

    for event in parser {
        if active_image.is_some() {
            match event {
                Event::End(TagEnd::Image) => {
                    let image = active_image
                        .take()
                        .expect("Active image should exist until its closing event");
                    if image.omitted {
                        has_omitted_images = true;
                        push_omitted_image(&mut text, &image.alt);
                    }
                }
                Event::Text(value)
                | Event::Code(value)
                | Event::InlineMath(value)
                | Event::DisplayMath(value) => active_image
                    .as_mut()
                    .expect("Active image should exist while collecting alt text")
                    .alt
                    .push_str(&value),
                Event::SoftBreak | Event::HardBreak => active_image
                    .as_mut()
                    .expect("Active image should exist while collecting alt text")
                    .alt
                    .push(' '),
                _ => {}
            }
            continue;
        }

        match event {
            Event::Start(Tag::Image { dest_url, .. }) => {
                let url = parse_image_url(dest_url.as_ref()).ok();
                let omitted = match url {
                    Some(url) if image_urls.len() < MAX_MARKDOWN_IMAGES => {
                        image_urls.push(url.into());
                        false
                    }
                    Some(_) | None => true,
                };
                active_image = Some(ImageCapture {
                    alt: String::new(),
                    omitted,
                });
            }
            Event::Text(value)
            | Event::Code(value)
            | Event::InlineMath(value)
            | Event::DisplayMath(value) => text.push_str(&value),
            Event::SoftBreak | Event::HardBreak => push_line_break(&mut text),
            Event::Rule => {
                push_line_break(&mut text);
                text.push_str("---");
                push_line_break(&mut text);
            }
            Event::TaskListMarker(checked) => {
                text.push_str(if checked { "[x] " } else { "[ ] " });
            }
            Event::FootnoteReference(name) => {
                text.push('[');
                text.push_str(&name);
                text.push(']');
            }
            Event::Start(Tag::Item) => text.push_str("- "),
            Event::Html(value) | Event::InlineHtml(value) => {
                for image in extract_html_images(&value, &mut html_state) {
                    match image
                        .src
                        .as_deref()
                        .and_then(|url| parse_image_url(url).ok())
                    {
                        Some(url) if image_urls.len() < MAX_MARKDOWN_IMAGES => {
                            image_urls.push(url.into());
                        }
                        Some(_) | None => {
                            has_omitted_images = true;
                            push_omitted_image(&mut text, &image.alt);
                        }
                    }
                }
            }
            Event::End(
                TagEnd::Paragraph
                | TagEnd::Heading(_)
                | TagEnd::CodeBlock
                | TagEnd::Item
                | TagEnd::FootnoteDefinition
                | TagEnd::TableRow,
            ) => push_line_break(&mut text),
            Event::End(TagEnd::TableCell) => text.push_str(" | "),
            Event::Start(_) | Event::End(_) => {}
        }
    }

    if let Some(image) = active_image
        && image.omitted
    {
        has_omitted_images = true;
        push_omitted_image(&mut text, &image.alt);
    }

    Ok(ParsedMarkdown {
        text: text.trim().to_owned(),
        image_urls,
        has_omitted_images,
    })
}

fn parse_image_url(value: &str) -> Result<Url> {
    Url::parse(value).context("invalid Markdown image URL")
}

struct ImageCapture {
    alt: String,
    omitted: bool,
}

#[derive(Default)]
struct HtmlScanState {
    suppression: Option<HtmlSuppression>,
}

#[derive(Clone, Copy)]
enum HtmlSuppression {
    Comment,
    Cdata,
    ProcessingInstruction,
    RawText(&'static str),
    Plaintext,
}

struct HtmlImage {
    src: Option<String>,
    alt: String,
}

struct HtmlTag<'a> {
    name: &'a str,
    attributes: &'a str,
    closing: bool,
    end: usize,
}

fn extract_html_images(value: &str, state: &mut HtmlScanState) -> Vec<HtmlImage> {
    let mut images = Vec::new();
    let mut cursor = 0;

    while cursor < value.len() {
        if let Some(suppression) = state.suppression {
            let Some(end) = find_html_suppression_end(value, cursor, suppression) else {
                break;
            };
            state.suppression = None;
            cursor = end;
            continue;
        }

        let Some(relative_start) = value[cursor..].find('<') else {
            break;
        };
        let start = cursor + relative_start;
        let remaining = &value[start..];

        if remaining.starts_with("<!--") {
            state.suppression = Some(HtmlSuppression::Comment);
            cursor = start + "<!--".len();
            continue;
        }
        if remaining.starts_with("<![CDATA[") {
            state.suppression = Some(HtmlSuppression::Cdata);
            cursor = start + "<![CDATA[".len();
            continue;
        }
        if remaining.starts_with("<?") {
            state.suppression = Some(HtmlSuppression::ProcessingInstruction);
            cursor = start + 2;
            continue;
        }
        if remaining.starts_with("<!") {
            cursor = find_html_tag_end(value, start + 2)
                .map(|end| end + 1)
                .unwrap_or(value.len());
            continue;
        }

        let Some(tag) = parse_html_tag(value, start) else {
            cursor = start + 1;
            continue;
        };
        cursor = tag.end;
        if tag.closing {
            continue;
        }

        if let Some(suppression) = raw_text_suppression(tag.name) {
            state.suppression = Some(suppression);
            continue;
        }
        if tag.name.eq_ignore_ascii_case("img") {
            images.push(parse_html_image_attributes(tag.attributes));
        }
    }

    images
}

fn find_html_suppression_end(
    value: &str,
    start: usize,
    suppression: HtmlSuppression,
) -> Option<usize> {
    match suppression {
        HtmlSuppression::Comment => value[start..].find("-->").map(|offset| start + offset + 3),
        HtmlSuppression::Cdata => value[start..].find("]]>").map(|offset| start + offset + 3),
        HtmlSuppression::ProcessingInstruction => {
            value[start..].find("?>").map(|offset| start + offset + 2)
        }
        HtmlSuppression::RawText(name) => find_raw_text_end(value, start, name),
        HtmlSuppression::Plaintext => None,
    }
}

fn find_raw_text_end(value: &str, start: usize, name: &str) -> Option<usize> {
    let mut cursor = start;
    while cursor < value.len() {
        let relative_start = value[cursor..].find('<')?;
        let tag_start = cursor + relative_start;
        if let Some(tag) = parse_html_tag(value, tag_start)
            && tag.closing
            && tag.name.eq_ignore_ascii_case(name)
        {
            return Some(tag.end);
        }
        cursor = tag_start + 1;
    }
    None
}

fn raw_text_suppression(name: &str) -> Option<HtmlSuppression> {
    for raw_text_name in [
        "script", "style", "textarea", "title", "xmp", "iframe", "noembed", "noframes",
    ] {
        if name.eq_ignore_ascii_case(raw_text_name) {
            return Some(HtmlSuppression::RawText(raw_text_name));
        }
    }
    name.eq_ignore_ascii_case("plaintext")
        .then_some(HtmlSuppression::Plaintext)
}

fn parse_html_tag(value: &str, start: usize) -> Option<HtmlTag<'_>> {
    let bytes = value.as_bytes();
    if bytes.get(start) != Some(&b'<') {
        return None;
    }

    let mut cursor = start + 1;
    let closing = bytes.get(cursor) == Some(&b'/');
    if closing {
        cursor += 1;
    }
    if !bytes.get(cursor).is_some_and(u8::is_ascii_alphabetic) {
        return None;
    }

    let name_start = cursor;
    while bytes
        .get(cursor)
        .is_some_and(|byte| !byte.is_ascii_whitespace() && !matches!(byte, b'/' | b'>'))
    {
        cursor += 1;
    }
    let name = &value[name_start..cursor];
    let tag_end = find_html_tag_end(value, cursor)?;

    Some(HtmlTag {
        name,
        attributes: &value[cursor..tag_end],
        closing,
        end: tag_end + 1,
    })
}

fn find_html_tag_end(value: &str, start: usize) -> Option<usize> {
    let bytes = value.as_bytes();
    let mut cursor = start;
    let mut quote = None;
    while let Some(&byte) = bytes.get(cursor) {
        match quote {
            Some(expected) if byte == expected => quote = None,
            Some(_) => {}
            None if matches!(byte, b'\'' | b'"') => quote = Some(byte),
            None if byte == b'>' => return Some(cursor),
            None => {}
        }
        cursor += 1;
    }
    None
}

fn parse_html_image_attributes(value: &str) -> HtmlImage {
    let bytes = value.as_bytes();
    let mut cursor = 0;
    let mut src = None;
    let mut alt = None;

    while cursor < bytes.len() {
        while bytes
            .get(cursor)
            .is_some_and(|byte| byte.is_ascii_whitespace() || *byte == b'/')
        {
            cursor += 1;
        }
        if cursor >= bytes.len() {
            break;
        }

        let name_start = cursor;
        while bytes
            .get(cursor)
            .is_some_and(|byte| !byte.is_ascii_whitespace() && !matches!(byte, b'=' | b'/'))
        {
            cursor += 1;
        }
        if name_start == cursor {
            cursor += 1;
            continue;
        }
        let name = &value[name_start..cursor];

        while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
            cursor += 1;
        }
        let attribute_value = if bytes.get(cursor) == Some(&b'=') {
            cursor += 1;
            while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
                cursor += 1;
            }
            parse_html_attribute_value(value, &mut cursor)
        } else {
            ""
        };

        if name.eq_ignore_ascii_case("src") && src.is_none() {
            src = Some(decode_html_entities(attribute_value));
        } else if name.eq_ignore_ascii_case("alt") && alt.is_none() {
            alt = Some(decode_html_entities(attribute_value));
        }
    }

    HtmlImage {
        src,
        alt: alt.unwrap_or_default(),
    }
}

fn parse_html_attribute_value<'a>(value: &'a str, cursor: &mut usize) -> &'a str {
    let bytes = value.as_bytes();
    let Some(&first) = bytes.get(*cursor) else {
        return "";
    };

    if matches!(first, b'\'' | b'"') {
        *cursor += 1;
        let start = *cursor;
        while bytes.get(*cursor).is_some_and(|byte| *byte != first) {
            *cursor += 1;
        }
        let result = &value[start..*cursor];
        if *cursor < bytes.len() {
            *cursor += 1;
        }
        result
    } else {
        let start = *cursor;
        while bytes
            .get(*cursor)
            .is_some_and(|byte| !byte.is_ascii_whitespace())
        {
            *cursor += 1;
        }
        &value[start..*cursor]
    }
}

fn decode_html_entities(value: &str) -> String {
    let mut decoded = String::with_capacity(value.len());
    let mut cursor = 0;

    while let Some(relative_ampersand) = value[cursor..].find('&') {
        let ampersand = cursor + relative_ampersand;
        decoded.push_str(&value[cursor..ampersand]);
        let entity_start = ampersand + 1;
        let Some(relative_semicolon) = value[entity_start..].find(';') else {
            decoded.push_str(&value[ampersand..]);
            return decoded;
        };
        if relative_semicolon > 16 {
            decoded.push('&');
            cursor = entity_start;
            continue;
        }

        let semicolon = entity_start + relative_semicolon;
        let entity = &value[entity_start..semicolon];
        if let Some(character) = decode_html_entity(entity) {
            decoded.push(character);
            cursor = semicolon + 1;
        } else {
            decoded.push('&');
            cursor = entity_start;
        }
    }

    decoded.push_str(&value[cursor..]);
    decoded
}

fn decode_html_entity(value: &str) -> Option<char> {
    match value {
        "amp" | "AMP" => Some('&'),
        "quot" | "QUOT" => Some('"'),
        "apos" => Some('\''),
        "lt" | "LT" => Some('<'),
        "gt" | "GT" => Some('>'),
        "nbsp" => Some('\u{00a0}'),
        _ => {
            let number = if let Some(value) = value
                .strip_prefix("#x")
                .or_else(|| value.strip_prefix("#X"))
            {
                u32::from_str_radix(value, 16).ok()
            } else {
                value.strip_prefix('#')?.parse::<u32>().ok()
            }?;
            if number == 0 {
                Some('\u{fffd}')
            } else {
                char::from_u32(number).or(Some('\u{fffd}'))
            }
        }
    }
}

fn is_image_content_type(value: &str) -> bool {
    let media_type = value.split(';').next().unwrap_or_default().trim();
    media_type.len() > "image/".len()
        && media_type
            .get(.."image/".len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("image/"))
}

fn validate_image_content_type(value: Option<&HeaderValue>) -> Result<()> {
    let Some(value) = value else {
        return Ok(());
    };
    let value = value
        .to_str()
        .context("Markdown image response has an invalid Content-Type")?;
    log::info!("Validating image content type: {}", value);
    let media_type = value.split(';').next().unwrap_or_default().trim();
    if is_image_content_type(value) || media_type.eq_ignore_ascii_case("application/octet-stream") {
        Ok(())
    } else {
        bail!("Markdown image response Content-Type is not an image or generic binary data")
    }
}

fn read_limited(reader: &mut impl Read, limit: usize) -> Result<Vec<u8>> {
    let read_limit = u64::try_from(limit)
        .context("image byte limit does not fit in u64")?
        .checked_add(1)
        .context("image byte limit is too large")?;
    let mut bytes = Vec::new();
    reader.take(read_limit).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        bail!("Markdown image exceeds the download size limit");
    }
    Ok(bytes)
}

fn push_line_break(text: &mut String) {
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
}

fn push_omitted_image(text: &mut String, alt: &str) {
    text.push_str("[image omitted");
    let alt = alt.trim();
    if !alt.is_empty() {
        text.push_str(": ");
        text.push_str(alt);
    }
    text.push(']');
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn parses_images_without_copying_alt_text_into_body() {
        let parsed = parse_markdown(
            "Before ![receipt](https://github.com/user-attachments/assets/123) after",
        )
        .unwrap();

        assert_eq!(parsed.text, "Before  after");
        assert_eq!(
            parsed.image_urls,
            ["https://github.com/user-attachments/assets/123"]
        );
        assert!(!parsed.has_omitted_images);
    }

    #[test]
    fn preserves_link_labels_and_basic_layout() {
        let parsed = parse_markdown(
            "# Heading\n\nSee [details](https://example.com/path).\n\n- First\n- Second",
        )
        .unwrap();

        assert_eq!(parsed.text, "Heading\nSee details.\n- First\n- Second");
        assert!(parsed.image_urls.is_empty());
    }

    #[test]
    fn replaces_images_over_the_limit_with_alt_text() {
        let markdown = (0..=MAX_MARKDOWN_IMAGES)
            .map(|index| format!("![{index}](https://github.com/user-attachments/assets/{index})"))
            .collect::<Vec<_>>()
            .join("\n");

        let parsed = parse_markdown(&markdown).unwrap();

        assert_eq!(parsed.image_urls.len(), MAX_MARKDOWN_IMAGES);
        assert!(parsed.text.contains("[image omitted: 4]"));
        assert!(parsed.has_omitted_images);
    }

    #[test]
    fn accepts_standard_https_images() {
        let parsed = parse_markdown("Before ![diagram](https://example.test/image.png) after")
            .expect("HTTPS images should be accepted without a host allowlist");

        assert_eq!(parsed.text, "Before  after");
        assert_eq!(parsed.image_urls, ["https://example.test/image.png"]);
    }

    #[test]
    fn parses_html_images_with_common_attribute_forms_and_entities() {
        let parsed = parse_markdown(
            r#"Before <IMG ALT='Receipt &amp; copy &#35;1' SRC="https://images.example.test/a.png?x=1&amp;y=2"><img src='https://images.example.test/b.png' alt='single quoted'><img alt=unquoted src=https://images.example.test/c.png> after"#,
        )
        .unwrap();

        assert_eq!(parsed.text, "Before  after");
        assert_eq!(
            parsed.image_urls,
            [
                "https://images.example.test/a.png?x=1&y=2",
                "https://images.example.test/b.png",
                "https://images.example.test/c.png",
            ]
        );
    }

    #[test]
    fn parses_multiple_html_images_from_one_html_block() {
        let parsed = parse_markdown(
            r#"<div>
<img src="https://images.example.test/one.png" alt="one">
<IMG SRC=https://images.example.test/two.png ALT=two>
</div>"#,
        )
        .unwrap();

        assert!(parsed.text.is_empty());
        assert_eq!(
            parsed.image_urls,
            [
                "https://images.example.test/one.png",
                "https://images.example.test/two.png",
            ]
        );
    }

    #[test]
    fn html_and_markdown_images_share_the_same_limit() {
        let parsed = parse_markdown(
            r#"![first](https://images.example.test/one.png)
<img src="https://images.example.test/two.png" alt="second">
<img src="https://images.example.test/three.png" alt="third">
<img src="https://images.example.test/four.png" alt="fourth">
<img src="https://images.example.test/five.png" alt="fifth">"#,
        )
        .unwrap();

        assert_eq!(parsed.image_urls.len(), MAX_MARKDOWN_IMAGES);
        assert!(parsed.text.contains("[image omitted: fifth]"));
    }

    #[test]
    fn ignores_img_text_inside_comments_and_raw_text_elements() {
        let parsed = parse_markdown(
            r#"Before <!-- <img src="https://images.example.test/comment.png"> -->
<ScRiPt>const fake = '<img src="https://images.example.test/script.png">';</sCrIpT>
<style>.x::after { content: '<img src="https://images.example.test/style.png">'; }</style>
<img src="https://images.example.test/real.png" alt="real"> after"#,
        )
        .unwrap();

        assert_eq!(parsed.image_urls, ["https://images.example.test/real.png"]);
        assert!(!parsed.text.contains("image omitted"));
    }

    #[test]
    fn decodes_decimal_and_hexadecimal_html_entities_without_panicking() {
        assert_eq!(
            decode_html_entities("A&amp;B &#38; &#x26; &#x1F5A8; &unknown;"),
            "A&B & & 🖨 &unknown;"
        );
        assert_eq!(decode_html_entities("trailing &amp"), "trailing &amp");
        assert_eq!(decode_html_entities("invalid &#x110000;"), "invalid �");
    }

    #[test]
    fn accepts_public_ip_literals_in_image_urls() {
        for value in [
            "https://8.8.8.8/image.png",
            "https://[2606:4700:4700::1111]/image.png",
            "https://[::ffff:8.8.8.8]/image.png",
        ] {
            parse_image_url(value).unwrap_or_else(|error| panic!("rejected {value}: {error:#}"));
        }
    }

    #[test]
    fn blocking_client_rejects_localhost_through_the_custom_resolver() {
        let fetcher = MarkdownImageFetcher {
            client: build_image_client().unwrap(),
        };

        let error = fetcher
            .fetch("https://localhost/image.png")
            .expect_err("localhost must not reach the HTTP connector");

        assert!(error.to_string().contains("request failed"));
    }

    #[test]
    fn recognizes_image_content_types_case_insensitively() {
        assert!(is_image_content_type("image/png"));
        assert!(is_image_content_type("IMAGE/JPEG; charset=binary"));
        assert!(is_image_content_type("image/jpeg"));
        assert!(!is_image_content_type("image/"));
        assert!(!is_image_content_type("text/html"));
    }

    #[test]
    fn permits_missing_image_and_generic_binary_content_types() {
        validate_image_content_type(None).unwrap();
        for value in [
            "image/png",
            "IMAGE/WEBP; charset=binary",
            "application/octet-stream",
            "APPLICATION/OCTET-STREAM; charset=binary",
        ] {
            let value = HeaderValue::from_str(value).unwrap();
            validate_image_content_type(Some(&value)).unwrap();
        }
    }

    #[test]
    fn rejects_explicit_non_image_content_types() {
        for value in ["text/html", "text/plain", "application/json", "image/"] {
            let value = HeaderValue::from_str(value).unwrap();
            assert!(validate_image_content_type(Some(&value)).is_err());
        }

        let invalid = HeaderValue::from_bytes(&[0xff]).unwrap();
        assert!(validate_image_content_type(Some(&invalid)).is_err());
    }

    #[test]
    fn reads_a_response_at_the_exact_limit() {
        let mut reader = Cursor::new(vec![7; 8]);

        assert_eq!(read_limited(&mut reader, 8).unwrap(), vec![7; 8]);
    }

    #[test]
    fn rejects_a_response_over_the_limit() {
        let mut reader = Cursor::new(vec![7; 9]);

        let error = read_limited(&mut reader, 8).unwrap_err();

        assert!(error.to_string().contains("size limit"));
    }
}
