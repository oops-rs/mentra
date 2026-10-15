//! What kind of file some bytes are, as a media type.
//!
//! A file crossing a boundary — shown to a model, sent to another agent,
//! saved from one — needs a media type, and the bytes are a better witness
//! than the name: a screenshot saved as `shot` is still a PNG. So
//! [`media_type`] asks the bytes first, the name's extension second, and
//! calls UTF-8 text `text/plain` before giving up on `application/octet-stream`.

use std::path::Path;

/// The media types a model is shown as an image: the four every provider
/// mentra speaks accepts.
pub const VIEWABLE_IMAGES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// The largest image a model is shown, in bytes: the smallest bound among the
/// providers mentra speaks (Anthropic's 5 MB per image).
pub const MAX_VIEWABLE_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// The media type of a file named `name` holding `bytes`.
pub fn media_type(name: &str, bytes: &[u8]) -> &'static str {
    sniff(bytes)
        .or_else(|| by_extension(name))
        .unwrap_or(if std::str::from_utf8(bytes).is_ok() {
            "text/plain"
        } else {
            "application/octet-stream"
        })
}

/// The media type the bytes declare by their signature, when they declare one.
pub fn sniff(bytes: &[u8]) -> Option<&'static str> {
    const SIGNATURES: [(&[u8], &str); 6] = [
        (b"\x89PNG\r\n\x1a\n", "image/png"),
        (b"\xff\xd8\xff", "image/jpeg"),
        (b"GIF87a", "image/gif"),
        (b"GIF89a", "image/gif"),
        (b"%PDF-", "application/pdf"),
        (b"PK\x03\x04", "application/zip"),
    ];
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    SIGNATURES
        .iter()
        .find(|(signature, _)| bytes.starts_with(signature))
        .map(|(_, media_type)| *media_type)
}

/// The media type a file name's extension conventionally means.
pub fn by_extension(name: &str) -> Option<&'static str> {
    let extension = Path::new(name).extension()?.to_str()?.to_ascii_lowercase();
    EXTENSIONS
        .iter()
        .find(|(known, _)| *known == extension)
        .map(|(_, media_type)| *media_type)
}

/// The extension a file of `media_type` conventionally takes, without the
/// dot, when there is one.
pub fn extension_for(media_type: &str) -> Option<&'static str> {
    let essence = essence(media_type);
    EXTENSIONS
        .iter()
        .find(|(_, known)| *known == essence)
        .map(|(extension, _)| *extension)
}

/// Whether a model is shown `media_type` as an image.
pub fn is_viewable_image(media_type: &str) -> bool {
    VIEWABLE_IMAGES.contains(&essence(media_type).as_str())
}

/// Whether `media_type` names text a reader can be shown as it is: `text/*`,
/// or JSON, XML or YAML by name or by structured-syntax suffix. Parameters
/// (`; charset=utf-8`) are ignored.
pub fn is_textual(media_type: &str) -> bool {
    let essence = essence(media_type);
    let Some((kind, subtype)) = essence.split_once('/') else {
        return false;
    };
    if subtype.is_empty() {
        return false;
    }
    if kind == "text" {
        return true;
    }
    let structured = ["json", "xml", "yaml", "x-yaml"];
    kind == "application"
        && (structured.contains(&subtype)
            || ["+json", "+xml", "+yaml"]
                .iter()
                .any(|suffix| subtype.ends_with(suffix)))
}

/// `raw` as a file name safe to save under: its last path segment, without
/// control characters or surrounding space, and never empty, `.` or `..`.
///
/// A file sent from elsewhere names itself, and the name may try to climb out
/// of the folder it is saved in (`../../.ssh/authorized_keys`) or to paint
/// over a terminal; this keeps only what can be a name. `None` when nothing
/// is left.
pub fn safe_file_name(raw: &str) -> Option<String> {
    let last = raw.rsplit(['/', '\\']).next().unwrap_or_default();
    let cleaned: String = last.chars().filter(|c| !c.is_control()).collect();
    let cleaned = cleaned.trim();
    (!cleaned.is_empty() && cleaned != "." && cleaned != "..").then(|| cleaned.to_string())
}

/// A media type without its parameters, lowercased.
fn essence(media_type: &str) -> String {
    media_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

/// Extensions and the media types they mean; the first extension listed for
/// a type is the one [`extension_for`] gives it.
const EXTENSIONS: [(&str, &str); 22] = [
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("svg", "image/svg+xml"),
    ("pdf", "application/pdf"),
    ("zip", "application/zip"),
    ("json", "application/json"),
    ("xml", "application/xml"),
    ("yaml", "application/yaml"),
    ("yml", "application/yaml"),
    ("txt", "text/plain"),
    ("md", "text/markdown"),
    ("csv", "text/csv"),
    ("html", "text/html"),
    ("htm", "text/html"),
    ("css", "text/css"),
    ("js", "text/javascript"),
    ("mp3", "audio/mpeg"),
    ("wav", "audio/wav"),
    ("mp4", "video/mp4"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bytes_are_asked_before_the_name() {
        assert_eq!(
            media_type("shot.txt", b"\x89PNG\r\n\x1a\nrest"),
            "image/png"
        );
        assert_eq!(media_type("photo", b"\xff\xd8\xff\xe0"), "image/jpeg");
        assert_eq!(media_type("a.webp", b"RIFF\0\0\0\0WEBPVP8 "), "image/webp");
    }

    #[test]
    fn the_name_answers_when_the_bytes_do_not_and_text_is_text() {
        assert_eq!(media_type("notes.md", b"# hi"), "text/markdown");
        assert_eq!(media_type("NOTES", b"plain words"), "text/plain");
        assert_eq!(
            media_type("blob", &[0xff, 0xfe, 0x00]),
            "application/octet-stream"
        );
    }

    #[test]
    fn only_the_four_common_images_are_viewable() {
        assert!(is_viewable_image("image/PNG"));
        assert!(is_viewable_image("image/jpeg; q=1"));
        assert!(!is_viewable_image("image/svg+xml"));
        assert!(!is_viewable_image("application/pdf"));
    }

    #[test]
    fn a_media_type_has_its_conventional_extension() {
        assert_eq!(extension_for("image/jpeg"), Some("jpg"));
        assert_eq!(extension_for("application/yaml"), Some("yaml"));
        assert_eq!(extension_for("application/x-unknown"), None);
    }

    #[test]
    fn a_name_from_elsewhere_cannot_leave_its_folder() {
        assert_eq!(
            safe_file_name("../../etc/passwd").as_deref(),
            Some("passwd")
        );
        assert_eq!(
            safe_file_name("C:\\Users\\x\\shot.png").as_deref(),
            Some("shot.png")
        );
        assert_eq!(safe_file_name(".."), None);
        assert_eq!(safe_file_name("dir/"), None);
        assert_eq!(safe_file_name("a\u{1b}[31mb").as_deref(), Some("a[31mb"));
    }

    #[test]
    fn textual_types_are_text_and_structured_text() {
        assert!(is_textual("text/csv"));
        assert!(is_textual("application/vnd.api+json"));
        assert!(!is_textual("image/png"));
        assert!(!is_textual("text/"));
    }
}
