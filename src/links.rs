// SPDX-License-Identifier: GPL-3.0-or-later
//! Turns what the user pastes into something that can be downloaded.
//!
//! Nothing here touches the network: a short "share" link comes back as [`Target::Short`] and the
//! caller resolves it and parses the result again.

/// Who published a panorama: the map service itself or one of its users.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Street-level imagery taken by the service (the car with the camera).
    Official,
    /// A 360 photo uploaded by a user.
    User,
}

impl Kind {
    /// The number the service uses for this kind of image.
    pub fn code(self) -> u8 {
        match self {
            Kind::Official => 2,
            Kind::User => 10,
        }
    }

    pub fn from_code(code: u64) -> Kind {
        if code == 2 || code == 0 {
            Kind::Official
        } else {
            Kind::User
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    /// A panorama known by its identifier.
    Pano { id: String, kind: Kind },
    /// Only a position: the nearest street-level panorama is used.
    Near { lat: f64, lon: f64 },
    /// The address of a hosted image itself (a photo), without its size suffix.
    Image { id: String, base_url: String },
    /// A short "share" link that has to be followed first.
    Short(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkError {
    Empty,
    /// The text is not a link this app understands.
    NotRecognised,
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LinkError::Empty => write!(f, "the link is empty"),
            LinkError::NotRecognised => write!(
                f,
                "this is not a link to a panorama or a photo: open it in the map application and copy the address from the browser"
            ),
        }
    }
}

impl std::error::Error for LinkError {}

const SHORT_HOSTS: [&str; 4] = ["maps.app.goo.gl", "goo.gl", "g.co", "share.google"];

/// Splits pasted text (or the lines of a list file) into single links. Lines that start with `#`
/// are comments.
pub fn split_links(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        for word in line.split_whitespace() {
            let word = word.trim_matches(|c| c == '"' || c == '\'' || c == '<' || c == '>' || c == ',');
            if !word.is_empty() && !out.iter().any(|o| o == word) {
                out.push(word.to_string());
            }
        }
    }
    out
}

/// Decodes `%XX` sequences. Invalid sequences are kept as they are.
pub fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = |b: u8| (b as char).to_digit(16);
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn host_of(url: &str) -> Option<&str> {
    let rest = url.split_once("://")?.1;
    let end = rest.find(|c| c == '/' || c == '?' || c == '#').unwrap_or(rest.len());
    let host = &rest[..end];
    Some(host.rsplit_once('@').map_or(host, |(_, h)| h))
}

fn looks_like_id(text: &str) -> bool {
    text.len() >= 16
        && text.len() <= 160
        && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Identifiers of user uploads have recognisable beginnings; street-level ones are 22 characters.
fn kind_of_bare_id(id: &str) -> Kind {
    if id.starts_with("AF1Qip") || id.starts_with("CIHM0og") || id.starts_with("CAoS") || id.len() != 22 {
        Kind::User
    } else {
        Kind::Official
    }
}

fn query_value<'a>(url: &'a str, key: &str) -> Option<&'a str> {
    let query = url.split_once('?')?.1;
    let query = query.split('#').next().unwrap_or(query);
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then_some(v)
    })
}

fn parse_lat_lon(text: &str) -> Option<(f64, f64)> {
    let mut parts = text.split(',');
    let lat: f64 = parts.next()?.trim().parse().ok()?;
    let lon: f64 = parts.next()?.trim().parse().ok()?;
    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)).then_some((lat, lon))
}

/// The image a link's `data=` block points to: its identifier and the service's kind number.
fn image_ref(url: &str) -> Option<(String, u64)> {
    let start = url.find("data=")?;
    let data = &url[start + 5..];
    let data = data.split(|c| c == '?' || c == '&' || c == '#').next().unwrap_or(data);
    let tokens: Vec<&str> = data.split('!').filter(|t| !t.is_empty()).collect();
    for (i, token) in tokens.iter().enumerate() {
        let Some(id) = token.strip_prefix("1s") else { continue };
        let Some(kind_code) = tokens.get(i + 1).and_then(|t| t.strip_prefix("2e")).and_then(|v| v.parse().ok())
        else {
            continue;
        };
        let id = percent_decode(id);
        if id.starts_with("0x") || !looks_like_id(&id) {
            continue;
        }
        return Some((id, kind_code));
    }
    None
}

/// The address of the image that some photo links carry inside (`!6s...`), without its size
/// suffix.
pub fn image_address(link: &str) -> Option<String> {
    let start = link.find("!6s")? + 3;
    let rest = &link[start..];
    let address = percent_decode(rest.split('!').next().unwrap_or(rest));
    photo_base_url(&address)
}

/// The address of a hosted photo without the size suffix that follows the last `=`.
pub fn photo_base_url(address: &str) -> Option<String> {
    let host = host_of(address)?;
    if !(host.ends_with("googleusercontent.com") || host.ends_with("ggpht.com")) {
        return None;
    }
    let address = address.split('?').next().unwrap_or(address);
    let last_slash = address.rfind('/')?;
    let base = match address[last_slash..].find('=') {
        Some(eq) => &address[..last_slash + eq],
        None => address,
    };
    Some(base.to_string())
}

/// Works out what a pasted link points to.
pub fn parse(link: &str) -> Result<Target, LinkError> {
    let link = link.trim();
    if link.is_empty() {
        return Err(LinkError::Empty);
    }
    if !link.contains("://") {
        if looks_like_id(link) {
            return Ok(Target::Pano { id: link.to_string(), kind: kind_of_bare_id(link) });
        }
        if let Some((lat, lon)) = parse_lat_lon(link) {
            return Ok(Target::Near { lat, lon });
        }
        if link.contains('.') && !link.contains(' ') {
            return parse(&format!("https://{link}"));
        }
        return Err(LinkError::NotRecognised);
    }
    let host = host_of(link).unwrap_or("").to_ascii_lowercase();
    if SHORT_HOSTS.iter().any(|h| host == *h) {
        return Ok(Target::Short(link.to_string()));
    }
    // A direct link to a hosted photo.
    if let Some(base_url) = photo_base_url(link) {
        let id = base_url.rsplit('/').next().unwrap_or("photo").to_string();
        return Ok(Target::Image { id, base_url });
    }
    if !host.contains("google") {
        return Err(LinkError::NotRecognised);
    }
    // Whether it is a 360 image or a flat photo is the service's to say, not the link's.
    if let Some((id, kind_code)) = image_ref(link) {
        return Ok(Target::Pano { id, kind: Kind::from_code(kind_code) });
    }
    for key in ["panoid", "pano"] {
        if let Some(id) = query_value(link, key) {
            let id = percent_decode(id);
            if looks_like_id(&id) {
                let kind = kind_of_bare_id(&id);
                return Ok(Target::Pano { id, kind });
            }
        }
    }
    for key in ["viewpoint", "cbll", "ll", "q", "query"] {
        if let Some((lat, lon)) = query_value(link, key).map(percent_decode).as_deref().and_then(parse_lat_lon) {
            return Ok(Target::Near { lat, lon });
        }
    }
    if let Some(at) = link.find("/@") {
        let rest = &link[at + 2..];
        let end = rest.find(|c| c == '/' || c == '?').unwrap_or(rest.len());
        if let Some((lat, lon)) = parse_lat_lon(&rest[..end]) {
            return Ok(Target::Near { lat, lon });
        }
    }
    Err(LinkError::NotRecognised)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn street_view_link_gives_an_official_panorama() {
        let link = "https://www.google.com/maps/@40.4167338,-3.7033315,3a,75y,271.04h,90t/data=!3m7!1e1!3m5!1sAAAAAAAAAAAAAAAAAAAAAA!2e0!6shttps:%2F%2Fstreetviewpixels-pa.googleapis.com%2Fv1%2Fthumbnail%3Fpanoid%3DAAAAAAAAAAAAAAAAAAAAAA!7i16384!8i8192?entry=ttu";
        assert_eq!(
            parse(link),
            Ok(Target::Pano { id: "AAAAAAAAAAAAAAAAAAAAAA".into(), kind: Kind::Official })
        );
    }

    #[test]
    fn user_sphere_link_gives_a_user_panorama() {
        let link = "https://www.google.com/maps/@40.41,-3.70,3a,75y,90t/data=!3m8!1e1!3m6!1sCIHM0ogKEICAgIAAAAAAAA!2e10!3e11!6shttps:%2F%2Flh3.googleusercontent.com%2Fgpms-cs-s%2FXYZ%3Dw900-h600-k-no-pi0-ya0-ro0-fo100!7i3840!8i1920";
        assert_eq!(
            parse(link),
            Ok(Target::Pano { id: "CIHM0ogKEICAgIAAAAAAAA".into(), kind: Kind::User })
        );
    }

    #[test]
    fn place_link_skips_the_feature_id_and_finds_the_photo() {
        let link = "https://www.google.com/maps/place/Somewhere/@40.41,-3.70,3a,75y,90t/data=!3m8!1e2!3m6!1sAF1QipAAAAAAAAAAAAAAAAAAAA!2e10!3e12!6shttps:%2F%2Flh3.googleusercontent.com%2Fp%2FAF1QipAAAAAAAAAAAAAAAAAAAA%3Dw203-h152-k-no!7i4032!8i3024!4m7!3m6!1s0xd42287d6da3df33:0x4a1c1d1e1f1a1b1c!8m2!3d40.41!4d-3.70";
        assert_eq!(parse(link), Ok(Target::Pano { id: "AF1QipAAAAAAAAAAAAAAAAAAAA".into(), kind: Kind::User }));
        assert_eq!(
            image_address(link).as_deref(),
            Some("https://lh3.googleusercontent.com/p/AF1QipAAAAAAAAAAAAAAAAAAAA")
        );
        assert_eq!(image_address("https://www.google.com/maps/@1,2,3a/data=!3m4!1e1!3m2!1sAAAAAAAAAAAAAAAAAAAAAA!2e0"), None);
    }

    #[test]
    fn coordinates_only_ask_for_the_nearest_panorama() {
        assert_eq!(
            parse("https://www.google.com/maps/@40.416729,-3.703339,17z"),
            Ok(Target::Near { lat: 40.416729, lon: -3.703339 })
        );
        assert_eq!(parse("40.416729, -3.703339"), Ok(Target::Near { lat: 40.416729, lon: -3.703339 }));
        assert_eq!(
            parse("https://www.google.com/maps/@?api=1&map_action=pano&viewpoint=48.857832,2.295226"),
            Ok(Target::Near { lat: 48.857832, lon: 2.295226 })
        );
    }

    #[test]
    fn short_links_are_left_to_be_followed() {
        assert_eq!(
            parse("https://maps.app.goo.gl/abcDEF123"),
            Ok(Target::Short("https://maps.app.goo.gl/abcDEF123".into()))
        );
    }

    #[test]
    fn bare_identifiers_and_query_parameters_work() {
        assert_eq!(
            parse("AAAAAAAAAAAAAAAAAAAAAA"),
            Ok(Target::Pano { id: "AAAAAAAAAAAAAAAAAAAAAA".into(), kind: Kind::Official })
        );
        assert_eq!(
            parse("https://www.google.com/maps/@?api=1&map_action=pano&pano=AAAAAAAAAAAAAAAAAAAAAA"),
            Ok(Target::Pano { id: "AAAAAAAAAAAAAAAAAAAAAA".into(), kind: Kind::Official })
        );
    }

    #[test]
    fn direct_photo_address_is_a_photo() {
        assert_eq!(
            parse("https://lh3.googleusercontent.com/p/AF1QipCCCCCCCCCCCCCCCCCCCC=w408-h306-k-no"),
            Ok(Target::Image {
                id: "AF1QipCCCCCCCCCCCCCCCCCCCC".into(),
                base_url: "https://lh3.googleusercontent.com/p/AF1QipCCCCCCCCCCCCCCCCCCCC".into(),
            })
        );
    }

    #[test]
    fn other_text_is_refused() {
        assert_eq!(parse("https://example.com/whatever"), Err(LinkError::NotRecognised));
        assert_eq!(parse("hello"), Err(LinkError::NotRecognised));
        assert_eq!(parse("  "), Err(LinkError::Empty));
    }

    #[test]
    fn pasted_text_is_split_into_links() {
        let text = "# my list\nhttps://a.example/1  https://a.example/2\n\n\"https://a.example/1\"\n";
        assert_eq!(split_links(text), vec!["https://a.example/1", "https://a.example/2"]);
    }

    #[test]
    fn percent_sequences_are_decoded() {
        assert_eq!(percent_decode("https:%2F%2Fx%3Dw1"), "https://x=w1");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz%41"), "%zzA");
    }
}
