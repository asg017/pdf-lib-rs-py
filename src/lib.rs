use std::collections::HashSet;
use std::io::Read;

use flate2::read::ZlibDecoder;
use pdf_lib_rs::api::{PageSizes, PdfDocument};
use pdf_lib_rs::core::context::PdfContext;
use pdf_lib_rs::core::objects::*;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

// ---------------------------------------------------------------------------
// Helpers for navigating the PDF object graph
// ---------------------------------------------------------------------------

fn resolve_dict<'a>(ctx: &'a PdfContext, obj: &'a PdfObject) -> Option<&'a PdfDict> {
    match obj {
        PdfObject::Dict(d) => Some(d),
        PdfObject::Ref(r) => {
            if let Some(PdfObject::Dict(d)) = ctx.lookup(r) {
                Some(d)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn get_filter_name(stream: &PdfRawStream) -> Option<String> {
    if let Some(PdfObject::Name(n)) = stream.dict.get(&PdfName::of("Filter")) {
        Some(n.as_string().to_string())
    } else if let Some(PdfObject::Array(arr)) = stream.dict.get(&PdfName::of("Filter")) {
        if let Some(PdfObject::Name(n)) = arr.get(0) {
            Some(n.as_string().to_string())
        } else {
            None
        }
    } else {
        None
    }
}

fn get_page_resources<'a>(ctx: &'a PdfContext, page_dict: &'a PdfDict) -> Option<&'a PdfDict> {
    if let Some(obj) = page_dict.get(&PdfName::of("Resources")) {
        return resolve_dict(ctx, obj);
    }
    // Check parent for inherited resources
    if let Some(PdfObject::Ref(parent_ref)) = page_dict.get(&PdfName::of("Parent")) {
        if let Some(PdfObject::Dict(parent_dict)) = ctx.lookup(parent_ref) {
            if let Some(obj) = parent_dict.get(&PdfName::of("Resources")) {
                return resolve_dict(ctx, obj);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Image extraction
// ---------------------------------------------------------------------------

struct ImageInfo {
    width: u32,
    height: u32,
    bits_per_component: u8,
    color_space: String,
    filter: Option<String>,
    data: Vec<u8>,
    page: usize,
}

fn collect_images(doc: &PdfDocument) -> Vec<ImageInfo> {
    let ctx = doc.context();
    let page_refs = doc.get_page_refs();
    let mut images = Vec::new();
    let mut seen: HashSet<(u32, u16)> = HashSet::new();

    for (page_idx, page_ref) in page_refs.iter().enumerate() {
        let page_dict = match ctx.lookup(page_ref) {
            Some(PdfObject::Dict(d)) => d,
            _ => continue,
        };

        let resources = match get_page_resources(ctx, &page_dict) {
            Some(r) => r,
            None => continue,
        };

        let xobj_dict = match resources.get(&PdfName::of("XObject")) {
            Some(obj) => match resolve_dict(ctx, obj) {
                Some(d) => d,
                None => continue,
            },
            None => continue,
        };

        for (_name, value) in xobj_dict.entries() {
            let r = match value {
                PdfObject::Ref(r) => r,
                _ => continue,
            };

            let key = (r.object_number, r.generation_number);
            if !seen.insert(key) {
                continue;
            }

            let stream = match ctx.lookup(r) {
                Some(PdfObject::Stream(s)) => s,
                _ => continue,
            };

            // Check Subtype == /Image
            let is_image = match stream.dict.get(&PdfName::of("Subtype")) {
                Some(PdfObject::Name(n)) => n.as_string() == "/Image",
                _ => false,
            };
            if !is_image {
                continue;
            }

            let width = match stream.dict.get(&PdfName::of("Width")) {
                Some(PdfObject::Number(n)) => n.as_number() as u32,
                _ => continue,
            };
            let height = match stream.dict.get(&PdfName::of("Height")) {
                Some(PdfObject::Number(n)) => n.as_number() as u32,
                _ => continue,
            };
            let bpc = match stream.dict.get(&PdfName::of("BitsPerComponent")) {
                Some(PdfObject::Number(n)) => n.as_number() as u8,
                _ => 8,
            };
            let color_space = match stream.dict.get(&PdfName::of("ColorSpace")) {
                Some(PdfObject::Name(n)) => n.as_string().trim_start_matches('/').to_string(),
                Some(PdfObject::Ref(cs_ref)) => match ctx.lookup(cs_ref) {
                    Some(PdfObject::Name(n)) => n.as_string().trim_start_matches('/').to_string(),
                    _ => "DeviceRGB".to_string(),
                },
                _ => "DeviceRGB".to_string(),
            };
            let filter = get_filter_name(&stream);

            images.push(ImageInfo {
                width,
                height,
                bits_per_component: bpc,
                color_space,
                filter: filter.clone(),
                data: stream.contents.clone(),
                page: page_idx + 1,
            });
        }
    }
    images
}

fn encode_image_to_png(img: &ImageInfo) -> Result<Vec<u8>, String> {
    let raw_data = match img.filter.as_deref() {
        Some("/FlateDecode") => {
            let mut decoder = ZlibDecoder::new(&img.data[..]);
            let mut buf = Vec::new();
            decoder
                .read_to_end(&mut buf)
                .map_err(|e| format!("FlateDecode error: {e}"))?;
            buf
        }
        _ => img.data.clone(),
    };

    // For sub-byte bit depths (1, 2, 4), RGB data is actually packed grayscale
    // since PNG doesn't support sub-byte RGB.
    let (color_type, samples) = if img.bits_per_component < 8 {
        (png::ColorType::Grayscale, 1)
    } else {
        match img.color_space.as_str() {
            "DeviceRGB" => (png::ColorType::Rgb, 3),
            "DeviceCMYK" => (png::ColorType::Rgb, 3), // convert below
            "DeviceGray" => (png::ColorType::Grayscale, 1),
            _ => (png::ColorType::Rgb, 3),
        }
    };

    let pixel_data = if img.color_space == "DeviceCMYK" {
        // Simple CMYK→RGB conversion
        let mut rgb = Vec::with_capacity((img.width * img.height * 3) as usize);
        for chunk in raw_data.chunks(4) {
            if chunk.len() < 4 {
                break;
            }
            let c = chunk[0] as f64 / 255.0;
            let m = chunk[1] as f64 / 255.0;
            let y = chunk[2] as f64 / 255.0;
            let k = chunk[3] as f64 / 255.0;
            rgb.push(((1.0 - c) * (1.0 - k) * 255.0) as u8);
            rgb.push(((1.0 - m) * (1.0 - k) * 255.0) as u8);
            rgb.push(((1.0 - y) * (1.0 - k) * 255.0) as u8);
        }
        rgb
    } else {
        raw_data
    };

    let mut buf = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut buf, img.width, img.height);
        encoder.set_color(color_type);
        encoder.set_depth(match img.bits_per_component {
            1 => png::BitDepth::One,
            2 => png::BitDepth::Two,
            4 => png::BitDepth::Four,
            16 => png::BitDepth::Sixteen,
            _ => png::BitDepth::Eight,
        });
        // For sub-byte depths, each row is ceil(width * bpc * samples / 8) bytes
        let bits_per_row = img.width as usize * img.bits_per_component as usize * samples;
        let bytes_per_row = (bits_per_row + 7) / 8;
        let expected_len = bytes_per_row * img.height as usize;
        let data_to_write = if pixel_data.len() >= expected_len {
            &pixel_data[..expected_len]
        } else {
            &pixel_data
        };
        let mut writer = encoder
            .write_header()
            .map_err(|e| format!("PNG header error: {e}"))?;
        writer
            .write_image_data(data_to_write)
            .map_err(|e| format!("PNG write error: {e}"))?;
    }
    Ok(buf)
}

// ---------------------------------------------------------------------------
// Python classes
// ---------------------------------------------------------------------------

/// An extracted image from a PDF.
#[pyclass]
#[derive(Clone)]
struct PdfImage {
    #[pyo3(get)]
    width: u32,
    #[pyo3(get)]
    height: u32,
    #[pyo3(get)]
    bits_per_component: u8,
    #[pyo3(get)]
    color_space: String,
    #[pyo3(get)]
    page: usize,
    filter: Option<String>,
    data: Vec<u8>,
}

#[pymethods]
impl PdfImage {
    /// Return the raw image bytes (JPEG/JP2 data, or decompressed pixel data).
    fn raw_bytes<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        let raw = match self.filter.as_deref() {
            Some("/DCTDecode") | Some("/JPXDecode") => self.data.clone(),
            Some("/FlateDecode") => {
                let mut decoder = ZlibDecoder::new(&self.data[..]);
                let mut buf = Vec::new();
                if decoder.read_to_end(&mut buf).is_ok() {
                    buf
                } else {
                    self.data.clone()
                }
            }
            _ => self.data.clone(),
        };
        PyBytes::new(py, &raw)
    }

    /// Return the image as PNG bytes.
    fn to_png<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        match self.filter.as_deref() {
            Some("/DCTDecode") | Some("/JPXDecode") => {
                // Already a complete image format — return as-is
                Ok(PyBytes::new(py, &self.data))
            }
            _ => {
                let info = ImageInfo {
                    width: self.width,
                    height: self.height,
                    bits_per_component: self.bits_per_component,
                    color_space: self.color_space.clone(),
                    filter: self.filter.clone(),
                    data: self.data.clone(),
                    page: self.page,
                };
                let png_data =
                    encode_image_to_png(&info).map_err(|e| PyValueError::new_err(e))?;
                Ok(PyBytes::new(py, &png_data))
            }
        }
    }

    /// The image format hint: "jpeg", "jp2", or "raw".
    #[getter]
    fn format(&self) -> &str {
        match self.filter.as_deref() {
            Some("/DCTDecode") => "jpeg",
            Some("/JPXDecode") => "jp2",
            _ => "raw",
        }
    }

    fn __repr__(&self) -> String {
        format!(
            "PdfImage({}x{}, {}, page={}, format={})",
            self.width,
            self.height,
            self.color_space,
            self.page,
            self.format()
        )
    }
}

/// Standard page sizes in points (1 point = 1/72 inch).
#[pyclass]
struct PageSize;

#[pymethods]
impl PageSize {
    #[classattr]
    const LETTER: (f64, f64) = (PageSizes::LETTER[0], PageSizes::LETTER[1]);
    #[classattr]
    const LEGAL: (f64, f64) = (PageSizes::LEGAL[0], PageSizes::LEGAL[1]);
    #[classattr]
    const TABLOID: (f64, f64) = (PageSizes::TABLOID[0], PageSizes::TABLOID[1]);
    #[classattr]
    const LEDGER: (f64, f64) = (PageSizes::LEDGER[0], PageSizes::LEDGER[1]);
    #[classattr]
    const A0: (f64, f64) = (PageSizes::A0[0], PageSizes::A0[1]);
    #[classattr]
    const A1: (f64, f64) = (PageSizes::A1[0], PageSizes::A1[1]);
    #[classattr]
    const A2: (f64, f64) = (PageSizes::A2[0], PageSizes::A2[1]);
    #[classattr]
    const A3: (f64, f64) = (PageSizes::A3[0], PageSizes::A3[1]);
    #[classattr]
    const A4: (f64, f64) = (PageSizes::A4[0], PageSizes::A4[1]);
    #[classattr]
    const A5: (f64, f64) = (PageSizes::A5[0], PageSizes::A5[1]);
    #[classattr]
    const A6: (f64, f64) = (PageSizes::A6[0], PageSizes::A6[1]);
    #[classattr]
    const EXECUTIVE: (f64, f64) = (PageSizes::EXECUTIVE[0], PageSizes::EXECUTIVE[1]);
    #[classattr]
    const FOLIO: (f64, f64) = (PageSizes::FOLIO[0], PageSizes::FOLIO[1]);
}

/// A PDF document loaded from bytes.
#[pyclass]
struct Pdf {
    doc: PdfDocument,
}

#[pymethods]
impl Pdf {
    /// Load a PDF from in-memory bytes.
    #[staticmethod]
    fn load(data: &[u8]) -> PyResult<Self> {
        let doc = PdfDocument::load(data)
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        Ok(Pdf { doc })
    }

    /// Create a new empty PDF document.
    #[staticmethod]
    fn create() -> Self {
        Pdf {
            doc: PdfDocument::create(),
        }
    }

    /// Number of pages.
    #[getter]
    fn page_count(&self) -> usize {
        self.doc.get_page_count()
    }

    /// Whether the document is encrypted.
    #[getter]
    fn is_encrypted(&self) -> bool {
        self.doc.is_encrypted()
    }

    /// Get the document title.
    #[getter]
    fn title(&self) -> Option<String> {
        self.doc.get_title()
    }

    /// Set the document title.
    #[setter]
    fn set_title(&mut self, value: &str) {
        self.doc.set_title(value);
    }

    /// Get the document author.
    #[getter]
    fn author(&self) -> Option<String> {
        self.doc.get_author()
    }

    /// Set the document author.
    #[setter]
    fn set_author(&mut self, value: &str) {
        self.doc.set_author(value);
    }

    /// Set the document subject.
    fn set_subject(&mut self, subject: &str) {
        self.doc.set_subject(subject);
    }

    /// Set the document keywords.
    fn set_keywords(&mut self, keywords: Vec<String>) {
        let refs: Vec<&str> = keywords.iter().map(|s| s.as_str()).collect();
        self.doc.set_keywords(&refs);
    }

    /// Set the document creator.
    fn set_creator(&mut self, creator: &str) {
        self.doc.set_creator(creator);
    }

    /// Set the document producer.
    fn set_producer(&mut self, producer: &str) {
        self.doc.set_producer(producer);
    }

    /// Add a blank page at the end with the given (width, height) in points.
    #[pyo3(signature = (size=None))]
    fn add_page(&mut self, size: Option<(f64, f64)>) {
        let s = size.unwrap_or((PageSizes::LETTER[0], PageSizes::LETTER[1]));
        self.doc.add_page([s.0, s.1]);
    }

    /// Insert a blank page at the given index.
    #[pyo3(signature = (index, size=None))]
    fn insert_page(&mut self, index: usize, size: Option<(f64, f64)>) {
        let s = size.unwrap_or((PageSizes::LETTER[0], PageSizes::LETTER[1]));
        self.doc.insert_page(index, [s.0, s.1]);
    }

    /// Remove the page at the given index.
    fn remove_page(&mut self, index: usize) {
        self.doc.remove_page(index);
    }

    /// Copy pages from another PDF by index. Returns the number of pages copied.
    fn copy_pages(&mut self, src: &Pdf, indices: Vec<usize>) -> usize {
        self.doc.copy_pages(&src.doc, &indices).len()
    }

    /// Serialize the document to PDF bytes.
    fn save<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        let bytes = self.doc.save();
        PyBytes::new(py, &bytes)
    }

    /// Extract all embedded images from the PDF.
    fn extract_images(&self) -> Vec<PdfImage> {
        collect_images(&self.doc)
            .into_iter()
            .map(|img| PdfImage {
                width: img.width,
                height: img.height,
                bits_per_component: img.bits_per_component,
                color_space: img.color_space,
                page: img.page,
                filter: img.filter,
                data: img.data,
            })
            .collect()
    }

    /// Get the number of indirect objects in the document.
    #[getter]
    fn object_count(&self) -> usize {
        self.doc.context().object_count()
    }

    fn __repr__(&self) -> String {
        format!(
            "Pdf(pages={}, encrypted={}, objects={})",
            self.doc.get_page_count(),
            self.doc.is_encrypted(),
            self.doc.context().object_count(),
        )
    }
}

/// Python bindings for pdf-lib-rs — a PDF parsing and manipulation library.
#[pymodule]
fn pdf_lib(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Pdf>()?;
    m.add_class::<PdfImage>()?;
    m.add_class::<PageSize>()?;
    Ok(())
}
