//! The one image path (spec, Architecture). Every image that enters the store, whether uploaded,
//! fetched for a URL import or read out of a Mealie backup, goes through `prepare`: sniffed by magic
//! bytes (png, jpeg, webp), bounded, decoded, turned upright, shrunk to 2048 px on the longest side,
//! re-encoded in its own format and addressed by the sha256 of the bytes that are stored. Re-encoding
//! drops EXIF (GPS included) and means a file that merely starts like an image is refused.
use std::io::Cursor;
use std::path::Path;

use image::imageops::FilterType;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader};
use sha2::{Digest, Sha256};
use sqlx::SqliteConnection;

use crate::{Error, Result};

/// Longest side kept.
const MAX_SIDE: u32 = 2048;
/// Longest side accepted at all; checked from the header, before any pixel is decoded.
const MAX_DECODED_SIDE: u32 = 12000;
const JPEG_QUALITY: u8 = 85;

/// An image ready to be stored.
#[derive(Debug, Clone)]
pub(crate) struct Processed {
    pub hash: String,
    pub mime: &'static str,
    pub bytes: Vec<u8>,
}

fn bad(m: impl Into<String>) -> Error {
    Error::Validation(m.into())
}

fn sniff(bytes: &[u8]) -> Option<ImageFormat> {
    match bytes {
        [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, ..] => Some(ImageFormat::Png),
        [0xff, 0xd8, 0xff, ..] => Some(ImageFormat::Jpeg),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some(ImageFormat::WebP),
        _ => None,
    }
}

/// The file extension an image of this mime type is stored under.
pub(crate) fn ext(mime: &str) -> Option<&'static str> {
    match mime {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/webp" => Some("webp"),
        _ => None,
    }
}

fn process(bytes: &[u8]) -> Result<Processed> {
    let refused = |e: image::ImageError| bad(format!("not a usable image: {e}"));
    let format = sniff(bytes).ok_or_else(|| bad("image must be a png, jpeg or webp file"))?;
    let mut decoder = ImageReader::with_format(Cursor::new(bytes), format).into_decoder().map_err(refused)?;
    let (w, h) = decoder.dimensions();
    if w > MAX_DECODED_SIDE || h > MAX_DECODED_SIDE {
        return Err(bad(format!("image is {w}x{h} pixels; at most {MAX_DECODED_SIDE} on a side is accepted")));
    }
    let orientation = decoder.orientation().map_err(refused)?;
    let mut img = DynamicImage::from_decoder(decoder).map_err(refused)?;
    img.apply_orientation(orientation);
    if img.width() > MAX_SIDE || img.height() > MAX_SIDE {
        img = img.resize(MAX_SIDE, MAX_SIDE, FilterType::Lanczos3);
    }
    let mut out = Cursor::new(Vec::new());
    match format {
        ImageFormat::Jpeg => img.write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY)),
        other => img.write_to(&mut out, other),
    }
    .map_err(refused)?;
    let bytes = out.into_inner();
    let mime = format.to_mime_type();
    Ok(Processed { hash: Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect(), mime, bytes })
}

/// `process` off the async threads: decoding a big photo takes long enough to matter there.
pub(crate) async fn prepare(bytes: &[u8]) -> Result<Processed> {
    let bytes = bytes.to_vec();
    tokio::task::spawn_blocking(move || process(&bytes)).await.map_err(|e| Error::Internal(format!("image task: {e}")))?
}

impl Processed {
    pub(crate) fn file_name(&self) -> String {
        format!("{}.{}", self.hash, ext(self.mime).unwrap_or("bin"))
    }

    pub(crate) fn image(&self) -> crate::Image {
        crate::Image { hash: self.hash.clone(), mime: self.mime.into() }
    }

    /// Content-addressed, so writing it twice is harmless, and a file left behind by a transaction
    /// that then failed is collected later with the other unreferenced ones. Written whole or not at
    /// all: a half-written file would otherwise sit under the hash of the complete one.
    pub(crate) fn write(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join(format!("{}.tmp", self.hash));
        std::fs::write(&tmp, &self.bytes)?;
        std::fs::rename(tmp, dir.join(self.file_name()))?;
        Ok(())
    }

    pub(crate) async fn record(&self, conn: &mut SqliteConnection) -> Result<()> {
        sqlx::query("INSERT OR IGNORE INTO images (hash, mime) VALUES (?, ?)").bind(&self.hash).bind(self.mime).execute(conn).await?;
        Ok(())
    }
}

/// The bytes of the multipart field `photo`. Shared by the API and the HTML upload form.
pub(crate) async fn read_photo(mut form: axum::extract::Multipart) -> Result<axum::body::Bytes> {
    let bad = |e: axum::extract::multipart::MultipartError| Error::Validation(format!("bad upload: {e}"));
    while let Some(field) = form.next_field().await.map_err(bad)? {
        if field.name() == Some("photo") {
            return field.bytes().await.map_err(bad);
        }
    }
    Err(Error::Validation("form field `photo` is missing".into()))
}
