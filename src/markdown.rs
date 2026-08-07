use anyhow::{Context, Result, bail};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use reqwest::StatusCode;
use reqwest::Url;
use reqwest::blocking::Client;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::header::{
    ACCEPT, ACCEPT_ENCODING, AUTHORIZATION, CONTENT_TYPE, HeaderValue, LOCATION,
};
use reqwest::redirect;
use std::env;
use std::error::Error;
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

const MAX_MARKDOWN_IMAGES: usize = 4;
const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;
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
    authorization: Option<HeaderValue>,
}

#[derive(Debug)]
struct PublicDnsResolver;

impl Resolve for PublicDnsResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addresses = tokio::task::spawn_blocking(move || resolve_public_addresses(&host))
                .await
                .map_err(|error| Box::new(error) as Box<dyn Error + Send + Sync>)?
                .map_err(|error| Box::new(error) as Box<dyn Error + Send + Sync>)?;
            Ok(Box::new(addresses.into_iter()) as Addrs)
        })
    }
}

impl MarkdownImageFetcher {
    pub fn from_env() -> Result<Self> {
        let authorization = optional_authorization()?;
        let client = build_image_client()?;

        Ok(Self {
            client,
            authorization,
        })
    }

    pub fn fetch(&self, value: &str) -> Result<Vec<u8>> {
        let mut url = parse_image_url(value)?;
        let mut redirect_count = 0;

        loop {
            let mut request = self
                .client
                .get(url.clone())
                .header(ACCEPT, "image/*")
                .header(ACCEPT_ENCODING, "identity");
            if should_send_authorization(&url, redirect_count)
                && let Some(authorization) = &self.authorization
            {
                request = request.header(AUTHORIZATION, authorization.clone());
            }

            let mut response = request
                .send()
                .map_err(|error| anyhow::Error::new(error.without_url()))
                .context("Markdown image request failed")?;
            validate_image_url(response.url())?;

            if is_followable_redirect(response.status()) {
                if redirect_count >= MAX_REDIRECTS {
                    bail!("Markdown image server returned too many redirects");
                }
                let location = response
                    .headers()
                    .get(LOCATION)
                    .context("Markdown image redirect is missing Location")?
                    .to_str()
                    .context("Markdown image redirect has an invalid Location")?;
                url = validated_redirect_target(response.url(), location)?;
                redirect_count += 1;
                continue;
            }

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

            return read_limited(&mut response, MAX_IMAGE_BYTES)
                .context("failed to read Markdown image response");
        }
    }
}

fn build_image_client() -> Result<Client> {
    Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .redirect(redirect::Policy::none())
        .retry(reqwest::retry::never())
        .referer(false)
        .https_only(true)
        .no_proxy()
        .dns_resolver(Arc::new(PublicDnsResolver))
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

fn optional_authorization() -> Result<Option<HeaderValue>> {
    let token = match env::var("LUCK_JINGLE_GITHUB_TOKEN") {
        Ok(value) if value.trim().is_empty() => return Ok(None),
        Ok(value) => value,
        Err(env::VarError::NotPresent) => return Ok(None),
        Err(error) => return Err(error).context("failed to read LUCK_JINGLE_GITHUB_TOKEN"),
    };
    let mut value = HeaderValue::from_str(&format!("Bearer {}", token.trim()))
        .context("LUCK_JINGLE_GITHUB_TOKEN cannot be used as an HTTP header")?;
    value.set_sensitive(true);
    Ok(Some(value))
}

fn validate_image_url(url: &Url) -> Result<()> {
    if url.scheme() != "https" {
        bail!("Markdown image URL must use HTTPS");
    }
    if !url.username().is_empty() || url.password().is_some() || url.authority().contains('@') {
        bail!("Markdown image URL must not contain credentials");
    }
    if url.port().is_some_and(|port| port != 443) {
        bail!("Markdown image URL must use the default HTTPS port");
    }

    let host = url
        .host_str()
        .context("Markdown image URL is missing a host")?;
    if url.domain().is_none() {
        let literal = host
            .strip_prefix('[')
            .and_then(|value| value.strip_suffix(']'))
            .unwrap_or(host)
            .parse::<IpAddr>()
            .context("Markdown image URL has an invalid IP literal")?;
        if !is_public_ip(literal) {
            bail!("Markdown image URL IP address is not publicly routable");
        }
    }
    Ok(())
}

fn parse_image_url(value: &str) -> Result<Url> {
    if authority_has_userinfo(value) {
        bail!("Markdown image URL must not contain credentials");
    }
    let url = Url::parse(value).context("invalid Markdown image URL")?;
    validate_image_url(&url)?;
    Ok(url)
}

fn authority_has_userinfo(value: &str) -> bool {
    let authority = value
        .split_once("://")
        .map(|(_, authority)| authority)
        .or_else(|| value.strip_prefix("//"));
    authority
        .and_then(|authority| authority.split(['/', '?', '#']).next())
        .is_some_and(|authority| authority.contains('@'))
}

fn validated_redirect_target(base: &Url, location: &str) -> Result<Url> {
    if authority_has_userinfo(location) {
        bail!("Markdown image URL must not contain credentials");
    }
    let target = base
        .join(location)
        .context("Markdown image redirect has an invalid Location")?;
    validate_image_url(&target)?;
    Ok(target)
}

fn is_followable_redirect(status: StatusCode) -> bool {
    matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308)
}

fn resolve_public_addresses(host: &str) -> std::io::Result<Vec<SocketAddr>> {
    let addresses = (host, 0).to_socket_addrs()?;
    let addresses = filter_public_addresses(addresses);
    if addresses.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "DNS did not return a publicly routable address",
        ));
    }
    Ok(addresses)
}

fn filter_public_addresses(addresses: impl IntoIterator<Item = SocketAddr>) -> Vec<SocketAddr> {
    addresses
        .into_iter()
        .filter(|address| is_public_ip(address.ip()) || is_proxy_fake_ip(address.ip()))
        .collect()
}

fn is_proxy_fake_ip(address: IpAddr) -> bool {
    match address {
        // Transparent proxy DNS modes commonly use this benchmarking range as a
        // synthetic route while TLS still authenticates the original hostname.
        IpAddr::V4(address) => ipv4_in_prefix(address, Ipv4Addr::new(198, 18, 0, 0), 15),
        IpAddr::V6(_) => false,
    }
}

fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => is_public_ipv6(address),
    }
}

fn is_public_ipv4(address: Ipv4Addr) -> bool {
    let denied = [
        (Ipv4Addr::new(0, 0, 0, 0), 8),
        (Ipv4Addr::new(10, 0, 0, 0), 8),
        (Ipv4Addr::new(100, 64, 0, 0), 10),
        (Ipv4Addr::new(127, 0, 0, 0), 8),
        (Ipv4Addr::new(169, 254, 0, 0), 16),
        (Ipv4Addr::new(172, 16, 0, 0), 12),
        (Ipv4Addr::new(192, 0, 0, 0), 24),
        (Ipv4Addr::new(192, 0, 2, 0), 24),
        (Ipv4Addr::new(192, 88, 99, 0), 24),
        (Ipv4Addr::new(192, 168, 0, 0), 16),
        (Ipv4Addr::new(198, 18, 0, 0), 15),
        (Ipv4Addr::new(198, 51, 100, 0), 24),
        (Ipv4Addr::new(203, 0, 113, 0), 24),
        (Ipv4Addr::new(224, 0, 0, 0), 4),
        (Ipv4Addr::new(240, 0, 0, 0), 4),
    ];

    !denied
        .into_iter()
        .any(|(network, prefix)| ipv4_in_prefix(address, network, prefix))
}

fn is_public_ipv6(address: Ipv6Addr) -> bool {
    if let Some(address) = address.to_ipv4_mapped() {
        return is_public_ipv4(address);
    }

    if !ipv6_in_prefix(address, Ipv6Addr::new(0x2000, 0, 0, 0, 0, 0, 0, 0), 3) {
        return false;
    }

    let denied = [
        (Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0), 23),
        (Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 0), 32),
        (Ipv6Addr::new(0x2002, 0, 0, 0, 0, 0, 0, 0), 16),
        (Ipv6Addr::new(0x3fff, 0, 0, 0, 0, 0, 0, 0), 20),
    ];

    !denied
        .into_iter()
        .any(|(network, prefix)| ipv6_in_prefix(address, network, prefix))
}

fn ipv4_in_prefix(address: Ipv4Addr, network: Ipv4Addr, prefix: u32) -> bool {
    let shift = 32 - prefix;
    u32::from(address) >> shift == u32::from(network) >> shift
}

fn ipv6_in_prefix(address: Ipv6Addr, network: Ipv6Addr, prefix: u32) -> bool {
    let shift = 128 - prefix;
    u128::from(address) >> shift == u128::from(network) >> shift
}

fn should_authorize(url: &Url) -> bool {
    url.host_str() == Some("github.com") && url.path().starts_with("/user-attachments/assets/")
}

fn should_send_authorization(url: &Url, redirect_count: usize) -> bool {
    redirect_count == 0 && should_authorize(url)
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
    fn invalid_html_image_sources_use_decoded_alt_placeholders() {
        let parsed = parse_markdown(
            r#"Before <img src="http://example.test/image.png" alt="Unsafe &amp; local &#35;1"><img alt='missing source'> after"#,
        )
        .unwrap();

        assert_eq!(
            parsed.text,
            "Before [image omitted: Unsafe & local #1][image omitted: missing source] after"
        );
        assert!(parsed.image_urls.is_empty());
        assert!(parsed.has_omitted_images);
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
    fn accepts_https_domains_and_non_attachment_github_paths() {
        for value in [
            "https://github.com/user-attachments/assets/123",
            "https://github.com/owner/repository/blob/main/image.png",
            "https://example.com/image.png",
            "https://images.example.test/path/image.webp?width=384",
            "https://xn--bcher-kva.example/image.png",
        ] {
            let url = Url::parse(value).unwrap();
            validate_image_url(&url).unwrap();
        }
    }

    #[test]
    fn rejects_non_https_credentials_non_default_ports_and_private_literals() {
        for value in [
            "http://github.com/user-attachments/assets/123",
            "https://@example.com/image.png",
            "https://user@github.com/image.png",
            "https://github.com:8443/image.png",
            "https://127.0.0.1/image.png",
            "https://10.0.0.1/image.png",
            "https://192.0.2.1/image.png",
            "https://[::1]/image.png",
            "https://[fd00::1]/image.png",
            "https://[::ffff:127.0.0.1]/image.png",
        ] {
            assert!(parse_image_url(value).is_err(), "accepted {value}");
        }
    }

    #[test]
    fn omits_relative_file_data_and_http_images() {
        for value in [
            "image.png",
            "/assets/image.png",
            "file:///tmp/image.png",
            "data:image/png;base64,AA==",
            "http://example.com/image.png",
        ] {
            let parsed = parse_markdown(&format!("![diagram]({value})")).unwrap();

            assert!(parsed.image_urls.is_empty(), "accepted {value}");
            assert_eq!(parsed.text, "[image omitted: diagram]");
        }
    }

    #[test]
    fn rejects_non_public_ipv4_ranges() {
        for value in [
            "0.0.0.0",
            "0.255.255.255",
            "10.0.0.1",
            "100.64.0.1",
            "100.127.255.254",
            "127.0.0.1",
            "169.254.1.1",
            "172.16.0.1",
            "172.31.255.254",
            "192.0.0.1",
            "192.0.2.1",
            "192.88.99.1",
            "192.168.1.1",
            "198.18.0.1",
            "198.19.255.254",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "239.255.255.255",
            "240.0.0.1",
            "255.255.255.255",
        ] {
            let address = value.parse::<IpAddr>().unwrap();
            assert!(!is_public_ip(address), "accepted {value}");
        }
    }

    #[test]
    fn rejects_non_public_ipv6_ranges_and_mapped_private_ipv4() {
        for value in [
            "::",
            "::1",
            "::ffff:10.0.0.1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.1.1",
            "::ffff:192.0.2.1",
            "64:ff9b::1",
            "100::1",
            "2001::1",
            "2001:db8::1",
            "2002::1",
            "3fff::1",
            "fc00::1",
            "fd00::1",
            "fe80::1",
            "fec0::1",
            "ff02::1",
        ] {
            let address = value.parse::<IpAddr>().unwrap();
            assert!(!is_public_ip(address), "accepted {value}");
        }
    }

    #[test]
    fn accepts_public_ipv4_ipv6_and_mapped_ipv4_addresses() {
        for value in [
            "1.1.1.1",
            "8.8.8.8",
            "93.184.216.34",
            "100.63.255.255",
            "100.128.0.0",
            "2001:4860:4860::8888",
            "2606:4700:4700::1111",
            "::ffff:8.8.8.8",
        ] {
            let address = value.parse::<IpAddr>().unwrap();
            assert!(is_public_ip(address), "rejected {value}");
        }
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
    fn filters_dns_results_before_they_reach_the_http_connector() {
        let addresses = [
            "127.0.0.1:443".parse::<SocketAddr>().unwrap(),
            "10.0.0.1:443".parse::<SocketAddr>().unwrap(),
            "1.1.1.1:443".parse::<SocketAddr>().unwrap(),
            "198.18.8.93:443".parse::<SocketAddr>().unwrap(),
            "[fd00::1]:443".parse::<SocketAddr>().unwrap(),
            "[2606:4700:4700::1111]:443".parse::<SocketAddr>().unwrap(),
        ];

        assert_eq!(
            filter_public_addresses(addresses),
            [
                "1.1.1.1:443".parse::<SocketAddr>().unwrap(),
                "198.18.8.93:443".parse::<SocketAddr>().unwrap(),
                "[2606:4700:4700::1111]:443".parse::<SocketAddr>().unwrap(),
            ]
        );
    }

    #[test]
    fn proxy_fake_ip_is_allowed_only_after_dns_resolution() {
        let address = "198.18.8.93".parse::<IpAddr>().unwrap();

        assert!(!is_public_ip(address));
        assert!(is_proxy_fake_ip(address));
        assert!(parse_image_url("https://198.18.8.93/image.png").is_err());
    }

    #[test]
    fn validates_every_redirect_target() {
        let base = Url::parse("https://images.example.com/a/source.png").unwrap();

        assert_eq!(
            validated_redirect_target(&base, "../final.png")
                .unwrap()
                .as_str(),
            "https://images.example.com/final.png"
        );
        for value in [
            "http://images.example.com/final.png",
            "https://@images.example.com/final.png",
            "https://user@images.example.com/final.png",
            "https://images.example.com:8443/final.png",
            "https://127.0.0.1/final.png",
            "https://[::ffff:10.0.0.1]/final.png",
        ] {
            assert!(
                validated_redirect_target(&base, value).is_err(),
                "accepted {value}"
            );
        }
    }

    #[test]
    fn follows_only_standard_get_redirect_statuses() {
        for status in [301, 302, 303, 307, 308] {
            assert!(is_followable_redirect(
                StatusCode::from_u16(status).unwrap()
            ));
        }
        for status in [200, 300, 304, 305, 306, 400] {
            assert!(!is_followable_redirect(
                StatusCode::from_u16(status).unwrap()
            ));
        }
    }

    #[test]
    fn blocking_client_rejects_localhost_through_the_custom_resolver() {
        let fetcher = MarkdownImageFetcher {
            client: build_image_client().unwrap(),
            authorization: None,
        };

        let error = fetcher
            .fetch("https://localhost/image.png")
            .expect_err("localhost must not reach the HTTP connector");

        assert!(error.to_string().contains("request failed"));
    }

    #[test]
    fn sends_tokens_only_to_the_github_attachment_entrypoint() {
        let attachment = Url::parse("https://github.com/user-attachments/assets/123").unwrap();
        let legacy = Url::parse("https://user-images.githubusercontent.com/1/image.png").unwrap();
        let storage =
            Url::parse("https://github-production-user-asset-6210df.s3.amazonaws.com/object")
                .unwrap();

        assert!(should_authorize(&attachment));
        assert!(!should_authorize(&legacy));
        assert!(!should_authorize(&storage));
        assert!(should_send_authorization(&attachment, 0));
        assert!(!should_send_authorization(&attachment, 1));
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
