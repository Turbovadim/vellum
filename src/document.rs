//! PDF loading, metadata/outline parsing and the background render pool.
//!
//! hayro's `Pdf` borrows heavily and its caches are `Rc`-based, so every worker
//! thread owns its own parsed copy of the document, built from shared bytes.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use anyhow::{Context as _, Result, anyhow};
use gpui::RenderImage;
use hayro::hayro_interpret::font::GlyphRun;
use hayro::hayro_interpret::{
    BlendMode, ClipPath, Context, Device, DrawMode, DrawProps, Image, ImageDrawProps,
    InterpreterCache, SoftMask, TransformExt, interpret_page,
};
use hayro::hayro_syntax::Pdf;
use hayro::hayro_syntax::object::dict::keys::{A, D, DEST, DESTS, FIRST, KIDS, NAMES, NEXT, OUTLINES, TITLE};
use hayro::hayro_syntax::object::{Array, Dict, Object, ObjectIdentifier};
use hayro::kurbo::{Affine, BezPath, Point, Rect};
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::vello_cpu::{PixmapMut, RasterizerSettings, RenderContext, Resources, TargetInit};
use hayro::{RenderCache, RenderSettings};

/// Everything the UI needs to lay out a document without touching hayro.
pub struct DocumentInfo {
    pub path: PathBuf,
    pub file_name: String,
    pub file_size: u64,
    /// Page sizes in PDF points, with the page's own /Rotate applied.
    pub page_sizes: Vec<(f32, f32)>,
    pub outline: Vec<OutlineItem>,
    pub title: Option<String>,
    pub author: Option<String>,
    pub subject: Option<String>,
    pub creator: Option<String>,
    pub producer: Option<String>,
    pub created: Option<String>,
    pub modified: Option<String>,
    pub version: String,
}

#[derive(Clone)]
pub struct OutlineItem {
    pub title: String,
    pub page: Option<usize>,
    pub depth: usize,
    pub has_children: bool,
}

/// A character on a page plus its box in unrotated page points (top-left origin).
#[derive(Clone, Copy)]
pub struct TextChar {
    pub ch: char,
    pub rect: Option<Rect>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum JobKind {
    Page,
    Thumbnail,
    /// One `TILE`-sized square of a page at full resolution, for high zoom.
    Tile,
    Text,
}

/// Edge length of a tile, in device pixels.
pub const TILE: u32 = 1024;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Job {
    pub kind: JobKind,
    pub page: usize,
    /// Pixel scale in millionths: exact enough that bitmaps map 1:1 onto screen pixels,
    /// while still letting jobs be hashed.
    pub scale_micro: u32,
    pub rotation: u16,
    /// Column and row of the tile, for `JobKind::Tile` (otherwise `(0, 0)`).
    pub tile: (u16, u16),
}

pub enum JobOutput {
    Image(Arc<RenderImage>),
    Text(Vec<TextChar>),
    Failed,
}

pub struct JobResult {
    pub job: Job,
    pub output: JobOutput,
}

struct Queue {
    jobs: Mutex<QueueState>,
    ready: Condvar,
}

struct QueueState {
    jobs: VecDeque<Job>,
    shutdown: bool,
}

/// A small pool of render threads fed by a priority queue.
pub struct RenderPool {
    queue: Arc<Queue>,
}

impl RenderPool {
    pub fn new(data: Arc<Vec<u8>>, results: async_channel::Sender<JobResult>) -> Self {
        let queue = Arc::new(Queue {
            jobs: Mutex::new(QueueState {
                jobs: VecDeque::new(),
                shutdown: false,
            }),
            ready: Condvar::new(),
        });
        // A few workers, each rasterizing large pages with several threads: heavy pages
        // finish 3-5x sooner than with one thread per page, which is what the user waits on.
        let cores = thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        let workers = (cores / 4).clamp(2, 4);
        let raster_threads = (cores / workers).clamp(1, 8) as u16;
        for ix in 0..workers {
            let queue = queue.clone();
            let data = data.clone();
            let results = results.clone();
            thread::Builder::new()
                .name(format!("pdf-render-{ix}"))
                .spawn(move || worker(queue, data, results, raster_threads))
                .ok();
        }
        Self { queue }
    }

    /// Queue jobs at the front so they are picked up before older work.
    pub fn push_front(&self, jobs: impl IntoIterator<Item = Job>) {
        let jobs: Vec<Job> = jobs.into_iter().collect();
        if jobs.is_empty() {
            return;
        }
        let mut state = self.queue.jobs.lock().unwrap();
        for job in jobs.iter().rev() {
            state.jobs.push_front(*job);
        }
        self.wake(jobs.len());
    }

    pub fn push_back(&self, jobs: impl IntoIterator<Item = Job>) {
        let jobs: Vec<Job> = jobs.into_iter().collect();
        if jobs.is_empty() {
            return;
        }
        let mut state = self.queue.jobs.lock().unwrap();
        state.jobs.extend(jobs.iter().copied());
        self.wake(jobs.len());
    }

    /// Wakes only as many workers as there is new work for.
    fn wake(&self, jobs: usize) {
        if jobs == 1 {
            self.queue.ready.notify_one();
        } else {
            self.queue.ready.notify_all();
        }
    }

    /// Drop queued page and tile renders that are no longer wanted, returning them.
    pub fn retain_pages(&self, keep: &HashSet<Job>) -> Vec<Job> {
        let mut state = self.queue.jobs.lock().unwrap();
        let mut dropped = Vec::new();
        state.jobs.retain(|job| {
            let cancellable = matches!(job.kind, JobKind::Page | JobKind::Tile);
            let keep = !cancellable || keep.contains(job);
            if !keep {
                dropped.push(*job);
            }
            keep
        });
        dropped
    }
}

impl Drop for RenderPool {
    fn drop(&mut self) {
        let mut state = self.queue.jobs.lock().unwrap();
        state.shutdown = true;
        state.jobs.clear();
        self.queue.ready.notify_all();
    }
}

fn worker(queue: Arc<Queue>, data: Arc<Vec<u8>>, results: async_channel::Sender<JobResult>, raster_threads: u16) {
    let Ok(pdf) = Pdf::new(data) else { return };
    let cache = RenderCache::new();
    loop {
        let job = {
            let mut state = queue.jobs.lock().unwrap();
            loop {
                if state.shutdown {
                    return;
                }
                if let Some(job) = state.jobs.pop_front() {
                    break job;
                }
                state = queue.ready.wait(state).unwrap();
            }
        };
        let output = match job.kind {
            JobKind::Page | JobKind::Thumbnail | JobKind::Tile => render_page(&pdf, &cache, raster_threads, &job)
                .map(JobOutput::Image)
                .unwrap_or(JobOutput::Failed),
            JobKind::Text => JobOutput::Text(extract_text(&pdf, job.page)),
        };
        if results.send_blocking(JobResult { job, output }).is_err() {
            return;
        }
    }
}

/// Maps a page laid out at `(w, h)` onto itself rotated clockwise by `rotation`.
pub fn rotation_transform(rotation: u16, w: f64, h: f64) -> Affine {
    match rotation % 360 {
        90 => Affine::new([0.0, 1.0, -1.0, 0.0, h, 0.0]),
        180 => Affine::new([-1.0, 0.0, 0.0, -1.0, w, h]),
        270 => Affine::new([0.0, -1.0, 1.0, 0.0, 0.0, w]),
        _ => Affine::IDENTITY,
    }
}

fn render_page<'a>(
    pdf: &'a Pdf,
    cache: &RenderCache<'a>,
    raster_threads: u16,
    job: &Job,
) -> Option<Arc<RenderImage>> {
    let page = pdf.pages().get(job.page)?;
    let scale = job.scale_micro as f64 / 1_000_000.0;
    let (w, h) = page.render_dimensions();
    let (w, h) = (w as f64, h as f64);
    let (rw, rh) = if job.rotation % 180 == 90 { (h, w) } else { (w, h) };
    // A whole page is limited in size; a tile is a window onto the page rendered at
    // full scale, so its bitmap stays small however far the user zooms in.
    let (full_w, full_h, pw, ph, origin) = if job.kind == JobKind::Tile {
        let (full_w, full_h) = ((rw * scale).round().max(1.0), (rh * scale).round().max(1.0));
        let x0 = job.tile.0 as f64 * TILE as f64;
        let y0 = job.tile.1 as f64 * TILE as f64;
        if x0 >= full_w || y0 >= full_h {
            return None;
        }
        // One extra pixel of overlap hides seams between neighbouring tiles.
        let tw = (TILE as f64 + 1.0).min(full_w - x0) as u16;
        let th = (TILE as f64 + 1.0).min(full_h - y0) as u16;
        (full_w, full_h, tw, th, (x0, y0))
    } else {
        let pw = (rw * scale).round().clamp(1.0, 16_000.0);
        let ph = (rh * scale).round().clamp(1.0, 16_000.0);
        (pw, ph, pw as u16, ph as u16, (0.0, 0.0))
    };
    let transform = Affine::translate((-origin.0, -origin.1))
        * Affine::scale_non_uniform(full_w / rw, full_h / rh)
        * rotation_transform(job.rotation, w, h)
        * page.initial_transform(true).to_kurbo();

    // Large bitmaps rasterize in parallel; small ones (thumbnails) aren't worth it.
    // The context is dropped after each job on purpose: its internal buffers grow with
    // scene complexity and would otherwise pin hundreds of MB per worker.
    let threads = if pw as u32 * ph as u32 >= 1_000_000 { raster_threads } else { 0 };
    let mut ctx = RenderContext::new_with(
        pw,
        ph,
        hayro::vello_cpu::RenderSettings {
            num_threads: threads,
            ..Default::default()
        },
    );
    let ctx = &mut ctx;
    hayro::render_into(
        page,
        cache,
        &crate::fonts::interpreter_settings(),
        &RenderSettings::default(),
        ctx,
        transform,
    );
    ctx.flush();
    // Rasterize straight into the buffer GPUI will own, then swap to BGRA in place.
    // The page is painted onto opaque white, so premultiplication is moot.
    let mut bytes = vec![0u8; pw as usize * ph as usize * 4];
    ctx.render_with(
        PixmapMut::new(pw, ph, &mut bytes)?,
        &mut Resources::default(),
        RasterizerSettings {
            target_init: TargetInit::Clear(WHITE),
            ..Default::default()
        },
    );
    for px in bytes.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    let buffer = image::RgbaImage::from_raw(pw as u32, ph as u32, bytes)?;
    Some(Arc::new(RenderImage::new(smallvec::smallvec![
        image::Frame::new(buffer)
    ])))
}

/// Collects glyph positions and their Unicode values while interpreting a page.
struct TextDevice {
    chars: Vec<TextChar>,
    last: Option<Rect>,
}

impl TextDevice {
    fn push_glyph(&mut self, ch_text: &str, rect: Rect) {
        if let Some(last) = self.last {
            let line_height = last.height().max(rect.height()).max(1.0);
            let same_line = (rect.center().y - last.center().y).abs() < line_height * 0.5;
            if !same_line {
                if self.chars.last().is_some_and(|c| c.ch != '\n') {
                    self.chars.push(TextChar { ch: '\n', rect: None });
                }
            } else {
                let gap = rect.x0 - last.x1;
                if gap > line_height * 0.18 && self.chars.last().is_some_and(|c| c.ch != ' ') {
                    self.chars.push(TextChar { ch: ' ', rect: None });
                }
            }
        }
        let mut iter = ch_text.chars().peekable();
        while let Some(ch) = iter.next() {
            if ch == '\u{0}' {
                continue;
            }
            let ch = if ch.is_whitespace() { ' ' } else { ch };
            if ch == ' ' && self.chars.last().is_some_and(|c| c.ch == ' ' || c.ch == '\n') {
                continue;
            }
            self.chars.push(TextChar {
                ch,
                rect: (ch != ' ').then_some(rect),
            });
        }
        self.last = Some(rect);
    }
}

impl<'a> Device<'a> for TextDevice {
    fn draw_path(&mut self, _: &BezPath, _: DrawProps<'a>, _: &DrawMode) {}
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'a>>, _: BlendMode) {}
    fn draw_glyph_run(&mut self, run: &GlyphRun<'_, 'a>, props: DrawProps<'a>, _: &DrawMode) {
        for glyph in run.glyphs() {
            let Some(text) = glyph.as_unicode() else { continue };
            let text = match text {
                hayro::hayro_interpret::hayro_cmap::BfString::Char(c) => c.to_string(),
                hayro::hayro_interpret::hayro_cmap::BfString::String(s) => s,
            };
            let advance = match &**glyph {
                hayro::hayro_interpret::font::Glyph::Outline(g) => g.advance_width().unwrap_or(500.0),
                _ => 500.0,
            } as f64;
            // Glyph space uses a 1000 unit em; take a generous ascent/descent box.
            let transform = props.transform * glyph.transform();
            let corners = [
                Point::new(0.0, -220.0),
                Point::new(advance, -220.0),
                Point::new(0.0, 880.0),
                Point::new(advance, 880.0),
            ]
            .map(|p| transform * p);
            let mut rect = Rect::from_points(corners[0], corners[1]);
            for corner in &corners[2..] {
                rect = rect.union_pt(*corner);
            }
            if rect.width().is_finite() && rect.height().is_finite() {
                self.push_glyph(&text, rect);
            }
        }
    }
    fn draw_image(&mut self, _: Image<'a, '_>, _: ImageDrawProps<'a>) {}
    fn pop_clip(&mut self) {}
    fn pop_transparency_group(&mut self) {}
}

fn extract_text(pdf: &Pdf, page_ix: usize) -> Vec<TextChar> {
    let Some(page) = pdf.pages().get(page_ix) else {
        return Vec::new();
    };
    let (w, h) = page.render_dimensions();
    let cache = InterpreterCache::new();
    let mut ctx = Context::new(
        page.initial_transform(true).to_kurbo(),
        Rect::new(0.0, 0.0, w as f64, h as f64),
        &cache,
        page.xref(),
        crate::fonts::interpreter_settings(),
    );
    let mut device = TextDevice {
        chars: Vec::new(),
        last: None,
    };
    interpret_page(page, &mut ctx, &mut device);
    device.chars
}

/// Turns hayro's `Pdf17`-style debug names into "1.7".
fn pdf_version(debug: &str) -> String {
    let digits: Vec<char> = debug.chars().filter(char::is_ascii_digit).collect();
    match digits.as_slice() {
        [major, minor] => format!("{major}.{minor}"),
        _ => debug.to_string(),
    }
}

pub fn load(path: &Path) -> Result<(DocumentInfo, Arc<Vec<u8>>)> {
    let path = &std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let file_size = bytes.len() as u64;
    let data = Arc::new(bytes);
    let pdf = Pdf::new(data.clone()).map_err(|err| anyhow!("Could not open PDF: {err:?}"))?;
    if pdf.pages().is_empty() {
        return Err(anyhow!("The document has no pages."));
    }

    let page_sizes = pdf.pages().iter().map(|p| p.render_dimensions()).collect();
    let page_ids: HashMap<ObjectIdentifier, usize> = pdf
        .pages()
        .iter()
        .enumerate()
        .filter_map(|(ix, page)| Some((page.raw().obj_id()?, ix)))
        .collect();
    let outline = parse_outline(&pdf, &page_ids);
    let meta = pdf.metadata();
    let text = |bytes: &Option<Vec<u8>>| {
        bytes
            .as_deref()
            .map(decode_text_string)
            .filter(|s| !s.trim().is_empty())
    };
    let date = |d: &Option<hayro::hayro_syntax::object::DateTime>| {
        d.as_ref().map(|d| {
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}",
                d.year, d.month, d.day, d.hour, d.minute
            )
        })
    };

    let info = DocumentInfo {
        file_name: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled.pdf".into()),
        path: path.to_path_buf(),
        file_size,
        page_sizes,
        outline,
        title: text(&meta.title),
        author: text(&meta.author),
        subject: text(&meta.subject),
        creator: text(&meta.creator),
        producer: text(&meta.producer),
        created: date(&meta.creation_date),
        modified: date(&meta.modification_date),
        version: pdf_version(&format!("{:?}", pdf.version())),
    };
    Ok((info, data))
}

/// Decodes a PDF text string (UTF-16BE with BOM, UTF-8 with BOM, or PDFDocEncoding).
fn decode_text_string(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xFE, 0xFF]) {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        String::from_utf8_lossy(&bytes[3..]).into_owned()
    } else {
        bytes.iter().map(|&b| b as char).collect()
    }
    .replace(['\r', '\n'], " ")
}

struct OutlineParser<'a, 'p> {
    root: Option<Dict<'a>>,
    page_ids: &'p HashMap<ObjectIdentifier, usize>,
    named: Option<HashMap<Vec<u8>, usize>>,
    visited: HashSet<ObjectIdentifier>,
    items: Vec<OutlineItem>,
}

fn parse_outline(pdf: &Pdf, page_ids: &HashMap<ObjectIdentifier, usize>) -> Vec<OutlineItem> {
    let xref = pdf.xref();
    let root: Option<Dict> = xref.get(xref.root_id());
    let Some(first) = root
        .as_ref()
        .and_then(|r| r.get::<Dict>(OUTLINES))
        .and_then(|o| o.get::<Dict>(FIRST))
    else {
        return Vec::new();
    };
    let mut parser = OutlineParser {
        root,
        page_ids,
        named: None,
        visited: HashSet::new(),
        items: Vec::new(),
    };
    parser.walk(first, 0);
    parser.items
}

impl<'a> OutlineParser<'a, '_> {
    fn walk(&mut self, first: Dict<'a>, depth: usize) {
        let mut current = Some(first);
        while let Some(item) = current {
            if self.items.len() > 20_000 || depth > 32 {
                return;
            }
            if let Some(id) = item.obj_id() {
                if !self.visited.insert(id) {
                    return;
                }
            }
            let title = item
                .get::<hayro::hayro_syntax::object::String>(TITLE)
                .map(|s| decode_text_string(s.as_bytes()))
                .unwrap_or_default();
            let page = self.item_page(&item);
            let child = item.get::<Dict>(FIRST);
            self.items.push(OutlineItem {
                title: title.trim().to_string(),
                page,
                depth,
                has_children: child.is_some(),
            });
            if let Some(child) = child {
                self.walk(child, depth + 1);
            }
            current = item.get::<Dict>(NEXT);
        }
    }

    fn item_page(&mut self, item: &Dict<'a>) -> Option<usize> {
        if let Some(dest) = item.get::<Object>(DEST) {
            return self.resolve_dest(dest, 0);
        }
        let action = item.get::<Dict>(A)?;
        self.resolve_dest(action.get::<Object>(D)?, 0)
    }

    fn resolve_dest(&mut self, dest: Object<'a>, depth: u8) -> Option<usize> {
        if depth > 4 {
            return None;
        }
        match dest {
            Object::Array(array) => self.array_page(&array),
            Object::Dict(dict) => self.resolve_dest(dict.get::<Object>(D)?, depth + 1),
            Object::Name(name) => self.named_page(name.as_ref()),
            Object::String(s) => self.named_page(s.as_bytes()),
            _ => None,
        }
    }

    fn array_page(&self, array: &Array<'a>) -> Option<usize> {
        let first = array.raw_iter().next()?;
        if let Some(r) = first.as_obj_ref() {
            return self.page_ids.get(&ObjectIdentifier::from(r)).copied();
        }
        match array.iter::<Object>().next()? {
            Object::Number(n) => Some(n.as_i64().max(0) as usize),
            _ => None,
        }
    }

    fn named_page(&mut self, name: &[u8]) -> Option<usize> {
        if self.named.is_none() {
            let mut map = HashMap::new();
            if let Some(root) = self.root.clone() {
                if let Some(dests) = root.get::<Dict>(DESTS) {
                    for (key, _) in dests.entries() {
                        if let Some(value) = dests.get::<Object>(key.as_ref()) {
                            if let Some(page) = self.dest_value_page(value) {
                                map.insert(key.as_ref().to_vec(), page);
                            }
                        }
                    }
                }
                if let Some(tree) = root.get::<Dict>(NAMES).and_then(|n| n.get::<Dict>(DESTS)) {
                    let mut seen = HashSet::new();
                    self.collect_name_tree(tree, &mut map, &mut seen, 0);
                }
            }
            self.named = Some(map);
        }
        self.named.as_ref()?.get(name).copied()
    }

    fn dest_value_page(&self, value: Object<'a>) -> Option<usize> {
        match value {
            Object::Array(array) => self.array_page(&array),
            Object::Dict(dict) => self.array_page(&dict.get::<Array>(D)?),
            _ => None,
        }
    }

    fn collect_name_tree(
        &self,
        node: Dict<'a>,
        map: &mut HashMap<Vec<u8>, usize>,
        seen: &mut HashSet<ObjectIdentifier>,
        depth: usize,
    ) {
        if depth > 32 || map.len() > 200_000 {
            return;
        }
        if let Some(id) = node.obj_id() {
            if !seen.insert(id) {
                return;
            }
        }
        if let Some(names) = node.get::<Array>(NAMES) {
            let mut iter = names.iter::<Object>();
            while let (Some(key), Some(value)) = (iter.next(), iter.next()) {
                let key = match key {
                    Object::String(s) => s.as_bytes().to_vec(),
                    Object::Name(n) => n.as_ref().to_vec(),
                    _ => continue,
                };
                if let Some(page) = self.dest_value_page(value) {
                    map.insert(key, page);
                }
            }
        }
        if let Some(kids) = node.get::<Array>(KIDS) {
            for kid in kids.iter::<Dict>() {
                self.collect_name_tree(kid, map, seen, depth + 1);
            }
        }
    }
}
