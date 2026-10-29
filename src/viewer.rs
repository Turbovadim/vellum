//! The Vellum window: unified toolbar, sidebar (thumbnails / contents / search
//! results) and a virtualized, zoomable page canvas.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, Bounds, ClickEvent, ElementInputHandler, EntityInputHandler, UTF16Selection, canvas, ClipboardItem, Context, CursorStyle, ExternalPaths,
    FocusHandle, FontWeight, HighlightStyle, Hsla, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ObjectFit, PathPromptOptions, PinchEvent, Pixels, Point, RenderImage, DevicePixels, Size,
    ScrollDelta, ScrollEvent, ScrollHandle, SharedString, Stateful, StyledText, Task, Window,
    WindowControlArea, actions, div, img, point, prelude::*, px, svg, ListAlignment, ListState,
    UniformListScrollHandle, fill, list, uniform_list,
};
use hayro::kurbo::{self, Affine};

use crate::document::{self, DocumentInfo, Job, JobKind, JobOutput, JobResult, RenderPool, TILE, TextChar};
use crate::theme::{Theme, UI_FONT};

actions!(
    preview,
    [
        Open,
        CloseWindow,
        Quit,
        ZoomIn,
        ZoomOut,
        ActualSize,
        ZoomToFit,
        RotateLeft,
        RotateRight,
        HideSidebar,
        ShowThumbnails,
        ShowTableOfContents,
        ToggleSidebar,
        Find,
        FindNext,
        FindPrevious,
        ToggleInspector,
        GoToPage,
        ContinuousScroll,
        SinglePage,
        TwoPages,
        CopyText,
        NextPage,
        PreviousPage,
        FirstPage,
        LastPage,
        ScrollDown,
        ScrollUp,
        ScrollPageDown,
        ScrollPageUp,
        Dismiss,
    ]
);

const TOOLBAR_H: f32 = 52.0;
/// Where toolbar content starts. On macOS this leaves the traffic lights' 18pt inset
/// between the green light and the sidebar icon.
const TOOLBAR_LEFT: f32 = if cfg!(target_os = "macos") { 86.0 } else { 12.0 };
const PAD: f32 = 20.0;
const GAP: f32 = 14.0;
const PAIR_GAP: f32 = 6.0;
const MIN_ZOOM: f32 = 0.1;
const MAX_ZOOM: f32 = 16.0;
const THUMB_W: f32 = 112.0;
const THUMB_MAX_H: f32 = 150.0;
/// Largest whole-page bitmap; beyond this, the visible part is rendered as tiles
/// on top of a whole-page backdrop of this size.
const MAX_RENDER_PIXELS: f64 = 8_000_000.0;
/// Extra area around the viewport (display px) whose tiles are rendered ahead.
const TILE_MARGIN: f32 = 256.0;
const SIDEBAR_MIN: f32 = 150.0;
const SIDEBAR_MAX: f32 = 380.0;
/// How long zoom must hold still before pages are re-rendered at the new scale.
const ZOOM_SETTLE: Duration = Duration::from_millis(140);
/// Upper bound on page bitmaps kept around (RAM + GPU copies).
const PAGE_CACHE_BUDGET: usize = 1024 << 20;

#[derive(Clone, Copy, PartialEq)]
enum ZoomMode {
    Fit,
    Custom(f32),
}

#[derive(Clone, Copy, PartialEq)]
enum DisplayMode {
    Continuous,
    SinglePage,
    TwoPages,
}

#[derive(Clone, Copy, PartialEq)]
enum SidebarMode {
    Thumbnails,
    Contents,
}

#[derive(Clone, Copy, PartialEq)]
enum Popover {
    ViewMenu,
    Share,
}

#[derive(Clone, Copy)]
struct Slot {
    page: usize,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

struct Row {
    top: f32,
    height: f32,
    /// Range into `Layout::slots`.
    slots: Range<usize>,
}

#[derive(Default)]
struct Layout {
    zoom: f32,
    rows: Vec<Row>,
    slots: Vec<Slot>,
    /// Row index of each page, or `u32::MAX` when the page is not laid out.
    page_row: Vec<u32>,
    width: f32,
    height: f32,
    single: bool,
}

/// Everything `compute_layout` depends on; the layout is rebuilt only when it changes.
#[derive(Clone, Copy, PartialEq)]
struct LayoutKey {
    viewport: (f32, f32),
    scale_factor: f32,
    zoom: ZoomMode,
    display: DisplayMode,
    rotation: u16,
    single_page: usize,
    doc: u64,
}

impl Layout {
    fn row_of_page(&self, page: usize) -> Option<usize> {
        self.page_row
            .get(page)
            .copied()
            .filter(|&r| r != u32::MAX)
            .map(|r| r as usize)
    }

    fn row_slots(&self, row: usize) -> &[Slot] {
        &self.slots[self.rows[row].slots.clone()]
    }

    /// Rows overlapping the vertical span `lo..hi`, found by binary search.
    fn rows_between(&self, lo: f32, hi: f32) -> Range<usize> {
        let start = self.rows.partition_point(|r| r.top + r.height < lo);
        let end = self.rows.partition_point(|r| r.top <= hi);
        start..end.max(start)
    }

    fn row_at(&self, y: f32) -> Option<usize> {
        if self.rows.is_empty() {
            return None;
        }
        let ix = self.rows.partition_point(|r| r.top + r.height + GAP * 0.5 < y);
        Some(ix.min(self.rows.len() - 1))
    }

    fn slot(&self, page: usize) -> Option<Slot> {
        let row = self.row_of_page(page)?;
        self.row_slots(row).iter().find(|s| s.page == page).copied()
    }
}

/// Where the user is looking, independent of zoom, used to keep position stable.
#[derive(Clone, Copy)]
struct Anchor {
    page: usize,
    frac_y: f32,
    frac_x: f32,
    viewport: Point<Pixels>,
}

struct PageImage {
    bitmap: Bitmap,
    job: Job,
}

/// A rendered bitmap. GPUI copies an image into the window's GPU atlas the first time it is
/// painted and afterwards finds it there by id, reading the pixels only on a miss. Entries
/// leave the atlas only through `drop_image`, so once a bitmap has been painted its RAM copy
/// is swapped for a 1x1 stand-in with the same id, halving what every shown page costs.
/// Paint through `bitmap_img` so the stand-in's size never matters.
struct Bitmap {
    image: Arc<RenderImage>,
    size: Size<DevicePixels>,
    in_ram: bool,
    /// Handed to GPUI this frame; it is uploaded by the time the next frame renders.
    shown: Cell<bool>,
}

impl Bitmap {
    fn new(image: Arc<RenderImage>) -> Self {
        Self {
            size: image.size(0),
            image,
            in_ram: true,
            shown: Cell::new(false),
        }
    }

    fn paint(&self) -> Arc<RenderImage> {
        self.shown.set(true);
        self.image.clone()
    }

    /// Bytes held in RAM plus on the GPU.
    fn cost(&self) -> usize {
        let bytes = self.size.width.0 as usize * self.size.height.0 as usize * 4;
        if self.in_ram { bytes * 2 } else { bytes }
    }

    /// Returns whether the RAM copy was released.
    fn release_if_uploaded(&mut self) -> bool {
        let release = self.in_ram && self.shown.get();
        if release {
            let pixel = image::Frame::new(image::RgbaImage::new(1, 1));
            let mut stand_in = RenderImage::new(smallvec::smallvec![pixel]);
            stand_in.id = self.image.id;
            self.image = Arc::new(stand_in);
            self.in_ram = false;
        }
        release
    }
}

struct Doc {
    info: DocumentInfo,
    pool: RenderPool,
    pages: HashMap<usize, PageImage>,
    thumbs: HashMap<usize, PageImage>,
    /// Full-resolution tiles for high zoom, keyed by their job (page, scale, rotation, tile).
    tiles: HashMap<Job, Bitmap>,
    wanted_tiles: HashSet<Job>,
    wanted: HashMap<usize, Job>,
    pending: HashSet<Job>,
    text: Vec<Option<Arc<Vec<TextChar>>>>,
    /// Case-folded copy of each page's text, aligned 1:1 with `text`, for searching.
    folded: Vec<Option<Arc<[char]>>>,
    /// Pages whose text has not been extracted yet.
    text_pending: usize,
    text_requested: Vec<bool>,
    _results: Task<()>,
}

impl Doc {
    /// Frees the RAM copy of every bitmap painted last frame, returning whether any was.
    fn release_uploaded(&mut self) -> bool {
        let pages = self.pages.values_mut().chain(self.thumbs.values_mut()).map(|p| &mut p.bitmap);
        let mut released = false;
        for bitmap in pages.chain(self.tiles.values_mut()) {
            released |= bitmap.release_if_uploaded();
        }
        released
    }

    fn has_unreleased_shown(&self) -> bool {
        let pages = self.pages.values().chain(self.thumbs.values()).map(|p| &p.bitmap);
        pages.chain(self.tiles.values()).any(|b| b.in_ram && b.shown.get())
    }
}

/// A match is just a span of a page's text; rects and snippets are built on demand.
#[derive(Clone, Copy)]
struct SearchHit {
    page: usize,
    start: u32,
    len: u32,
}

impl SearchHit {
    fn range(&self) -> Range<usize> {
        self.start as usize..(self.start + self.len) as usize
    }
}

#[derive(Clone, Copy)]
struct Selection {
    page: usize,
    anchor: kurbo::Point,
    head: kurbo::Point,
    mode: SelectMode,
}

/// Single click drags by character, double click by word, triple click by line.
#[derive(Clone, Copy, PartialEq)]
enum SelectMode {
    Char,
    Word,
    Line,
}

#[derive(Clone, Copy, PartialEq)]
enum Axis {
    Vertical,
    Horizontal,
}

#[derive(Clone, Copy)]
struct ScrollbarDrag {
    axis: Axis,
    start_mouse: f32,
    start_pos: f32,
    /// Content pixels per pixel of thumb travel.
    ratio: f32,
}

struct Highlight {
    page: usize,
    rects: Vec<kurbo::Rect>,
}

pub struct VellumApp {
    focus: FocusHandle,
    search_focus: FocusHandle,
    goto_focus: FocusHandle,
    doc: Option<Doc>,
    loading: bool,
    error: Option<SharedString>,
    zoom: ZoomMode,
    display: DisplayMode,
    rotation: u16,
    current_page: usize,
    sidebar_visible: bool,
    sidebar_mode: SidebarMode,
    sidebar_width: f32,
    resizing_sidebar: bool,
    scroll: ScrollHandle,
    sidebar_scroll: ScrollHandle,
    layout: Layout,
    pending_anchor: Option<Anchor>,
    last_scrolled_thumb: Option<usize>,
    query: String,
    hits: Vec<SearchHit>,
    current_hit: Option<usize>,
    toc_expanded: HashSet<usize>,
    selection: Option<Selection>,
    selecting: bool,
    highlights: Vec<Highlight>,
    highlight_mode: bool,
    inspector: bool,
    popover: Option<Popover>,
    goto: Option<String>,
    titlebar_should_move: bool,
    /// While true, newly found matches may move the current match (until the user navigates).
    hit_auto: bool,
    search_anchor_page: usize,
    scrollbar_drag: Option<ScrollbarDrag>,
    /// Bumped whenever a different document is opened.
    doc_generation: u64,
    layout_key: Option<LayoutKey>,
    /// Inputs of the last `schedule_renders` pass; skipped when unchanged and not dirty.
    render_key: Option<(Range<usize>, u32, u16, u32, bool, Vec<Job>)>,
    renders_dirty: bool,
    thumb_cache: Option<((u16, u64), Arc<Vec<(f32, f32, f32)>>, f32)>,
    thumb_key: Option<(Range<usize>, u16, u32, u64)>,
    needle: Arc<[char]>,
    search_gen: u64,
    search_running: bool,
    _search_task: Option<Task<()>>,
    hit_list: ListState,
    toc_scroll: UniformListScrollHandle,
    toc_version: u64,
    toc_cache: Option<((usize, u64, u64), Arc<Vec<usize>>, Option<usize>)>,
    /// Selection range and rects, refreshed on mouse input instead of every frame.
    sel_cache: Option<(usize, Range<usize>, Arc<Vec<kurbo::Rect>>)>,
    zoom_changed_at: Option<Instant>,
    _settle_task: Option<Task<()>>,
    smooth: SmoothScroller,
    thumb_smooth: SmoothScroller,
    thumb_max: f32,
    zoom_hud_until: Option<Instant>,
    _zoom_hud_task: Option<Task<()>>,
}

impl VellumApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe_window_appearance(window, |_, _, cx| cx.notify())
            .detach();
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        Self {
            focus,
            search_focus: cx.focus_handle(),
            goto_focus: cx.focus_handle(),
            doc: None,
            loading: false,
            error: None,
            zoom: ZoomMode::Fit,
            display: DisplayMode::Continuous,
            rotation: 0,
            current_page: 0,
            sidebar_visible: true,
            sidebar_mode: SidebarMode::Thumbnails,
            sidebar_width: 184.0,
            resizing_sidebar: false,
            scroll: ScrollHandle::new(),
            sidebar_scroll: ScrollHandle::new(),
            layout: Layout::default(),
            pending_anchor: None,
            last_scrolled_thumb: None,
            query: String::new(),
            hits: Vec::new(),
            current_hit: None,
            toc_expanded: HashSet::new(),
            selection: None,
            selecting: false,
            highlights: Vec::new(),
            highlight_mode: false,
            inspector: false,
            popover: None,
            goto: None,
            titlebar_should_move: false,
            hit_auto: false,
            search_anchor_page: 0,
            scrollbar_drag: None,
            doc_generation: 0,
            layout_key: None,
            render_key: None,
            renders_dirty: true,
            thumb_cache: None,
            thumb_key: None,
            needle: Arc::from([]),
            search_gen: 0,
            search_running: false,
            _search_task: None,
            hit_list: ListState::new(0, ListAlignment::Top, px(400.)),
            toc_scroll: UniformListScrollHandle::new(),
            toc_version: 0,
            toc_cache: None,
            sel_cache: None,
            zoom_changed_at: None,
            _settle_task: None,
            smooth: SmoothScroller::default(),
            thumb_smooth: SmoothScroller::default(),
            thumb_max: 0.0,
            zoom_hud_until: None,
            _zoom_hud_task: None,
        }
    }

    // ---------------------------------------------------------------- opening

    /// True while the window shows the "open a document" placeholder.
    pub fn is_empty(&self) -> bool {
        self.doc.is_none() && !self.loading
    }

    pub fn open_path(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.loading = true;
        self.error = None;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move { document::load(&path) })
                .await;
            this.update_in(cx, |this, window, cx| match loaded {
                Ok((info, data)) => this.finish_open(info, data, window, cx),
                Err(err) => {
                    this.loading = false;
                    this.error = Some(format!("{err:#}").into());
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn finish_open(
        &mut self,
        info: DocumentInfo,
        data: Arc<Vec<u8>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_document(window, cx);
        let (tx, rx) = async_channel::unbounded::<JobResult>();
        let pool = RenderPool::new(data, tx);
        let results = cx.spawn_in(window, async move |this, cx| {
            while let Ok(first) = rx.recv().await {
                let mut batch = vec![first];
                while let Ok(more) = rx.try_recv() {
                    batch.push(more);
                }
                let updated = this.update_in(cx, |this, window, cx| {
                    for result in batch {
                        this.on_job_result(result, window, cx);
                    }
                    cx.notify();
                });
                if updated.is_err() {
                    break;
                }
            }
        });
        let pages = info.page_sizes.len();
        window.set_window_title(&info.file_name);
        self.doc = Some(Doc {
            info,
            pool,
            pages: HashMap::new(),
            tiles: HashMap::new(),
            wanted_tiles: HashSet::new(),
            thumbs: HashMap::new(),
            wanted: HashMap::new(),
            pending: HashSet::new(),
            text: vec![None; pages],
            folded: vec![None; pages],
            text_pending: pages,
            text_requested: vec![false; pages],
            _results: results,
        });
        self.doc_generation += 1;
        self.layout_key = None;
        self.render_key = None;
        self.renders_dirty = true;
        self.thumb_cache = None;
        self.thumb_key = None;
        self.toc_cache = None;
        self.sel_cache = None;
        self.hit_list.reset(0);
        self.loading = false;
        self.error = None;
        self.zoom = ZoomMode::Fit;
        self.rotation = 0;
        self.current_page = 0;
        self.hits.clear();
        self.current_hit = None;
        self.selection = None;
        self.highlights.clear();
        self.toc_expanded.clear();
        self.last_scrolled_thumb = None;
        self.layout = Layout::default();
        self.layout_key = None;
        self.scroll.set_offset(point(px(0.), px(0.)));
        self.sidebar_scroll.set_offset(point(px(0.), px(0.)));
        if !self.query.is_empty() {
            self.request_all_text();
        }
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn close_document(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(doc) = self.doc.take() {
            for (_, page) in doc.pages.into_iter().chain(doc.thumbs) {
                cx.drop_image(page.bitmap.image, Some(&mut *window));
            }
            for (_, tile) in doc.tiles {
                cx.drop_image(tile.image, Some(&mut *window));
            }
        }
    }

    fn on_job_result(&mut self, result: JobResult, window: &mut Window, cx: &mut Context<Self>) {
        let Some(doc) = self.doc.as_mut() else { return };
        doc.pending.remove(&result.job);
        let job = result.job;
        match result.output {
            JobOutput::Image(image) => {
                if job.kind == JobKind::Tile {
                    if job.rotation == self.rotation && doc.wanted_tiles.contains(&job) {
                        if let Some(old) = doc.tiles.insert(job, Bitmap::new(image)) {
                            cx.drop_image(old.image, Some(window));
                        }
                    } else {
                        cx.drop_image(image, Some(window));
                    }
                    return;
                }
                let slot = match job.kind {
                    JobKind::Page => {
                        // Keep a result if it is what we currently want, or if we have nothing better.
                        let wanted = doc.wanted.get(&job.page) == Some(&job);
                        let have = doc.pages.get(&job.page);
                        let usable = have.is_none_or(|h| h.job.rotation != self.rotation);
                        if !(wanted || (usable && job.rotation == self.rotation)) {
                            cx.drop_image(image, Some(window));
                            return;
                        }
                        &mut doc.pages
                    }
                    _ => &mut doc.thumbs,
                };
                if let Some(old) = slot.insert(job.page, PageImage { bitmap: Bitmap::new(image), job }) {
                    cx.drop_image(old.bitmap.image, Some(window));
                }
                self.renders_dirty = true;
            }
            JobOutput::Text(chars) => {
                let folded = fold_text(&chars);
                let page = job.page;
                if let Some(entry) = doc.text.get_mut(page) {
                    if entry.is_none() {
                        doc.text_pending = doc.text_pending.saturating_sub(1);
                    }
                    *entry = Some(Arc::new(chars));
                    doc.folded[page] = Some(folded.clone());
                }
                if !self.needle.is_empty() {
                    let mut new_hits = Vec::new();
                    scan_page(page, &folded, &self.needle, &mut new_hits);
                    if !new_hits.is_empty() {
                        let at = self.hits.partition_point(|h| h.page <= page);
                        let added = new_hits.len();
                        self.hits.splice(at..at, new_hits);
                        self.hit_list.splice(at..at, added);
                        if self.hit_auto {
                            self.pick_auto_hit();
                        } else if let Some(current) = self.current_hit.as_mut() {
                            if *current >= at {
                                *current += added;
                            }
                        }
                    }
                }
                if self.selection.is_some_and(|s| s.page == page) {
                    self.refresh_selection();
                }
            }
            JobOutput::Failed => {
                self.renders_dirty = true;
            }
        }
    }

    fn request_text(&mut self, page: usize) {
        let Some(doc) = self.doc.as_mut() else { return };
        if doc.text_requested.get(page).copied() == Some(false) {
            doc.text_requested[page] = true;
            doc.pool.push_front([Job {
                kind: JobKind::Text,
                page,
                scale_micro: 0,
                rotation: 0,
                tile: (0, 0),
            }]);
        }
    }

    fn request_all_text(&mut self) {
        let Some(doc) = self.doc.as_mut() else { return };
        let mut jobs = Vec::new();
        for (page, requested) in doc.text_requested.iter_mut().enumerate() {
            if !*requested {
                *requested = true;
                jobs.push(Job {
                    kind: JobKind::Text,
                    page,
                    scale_micro: 0,
                    rotation: 0,
                    tile: (0, 0),
                });
            }
        }
        doc.pool.push_back(jobs);
    }

    // ---------------------------------------------------------------- layout

    fn page_count(&self) -> usize {
        self.doc.as_ref().map_or(0, |d| d.info.page_sizes.len())
    }

    fn rotated_size(&self, page: usize) -> (f32, f32) {
        let (w, h) = self
            .doc
            .as_ref()
            .and_then(|d| d.info.page_sizes.get(page).copied())
            .unwrap_or((612.0, 792.0));
        if self.rotation % 180 == 90 { (h, w) } else { (w, h) }
    }

    fn viewport(&self, window: &Window) -> (f32, f32) {
        let size = window.viewport_size();
        let sidebar = if self.sidebar_shown() { self.sidebar_width + 1.0 } else { 0.0 };
        (
            (size.width.as_f32() - sidebar).max(1.0),
            (size.height.as_f32() - TOOLBAR_H).max(1.0),
        )
    }

    fn content_origin(&self) -> (f32, f32) {
        let sidebar = if self.sidebar_shown() { self.sidebar_width + 1.0 } else { 0.0 };
        (sidebar, TOOLBAR_H)
    }

    /// Pages shown side by side in each row, as contiguous page ranges.
    fn row_groups(&self) -> impl Iterator<Item = Range<usize>> + '_ {
        let count = self.page_count();
        let single = self.current_page.min(count.saturating_sub(1));
        let (start, step, end) = match self.display {
            DisplayMode::Continuous => (0, 1, count),
            DisplayMode::TwoPages => (0, 2, count),
            DisplayMode::SinglePage => (single, 1, (single + 1).min(count)),
        };
        (start..end).step_by(step).map(move |p| p..(p + step).min(end))
    }

    fn row_width(&self, group: &Range<usize>) -> f32 {
        group.clone().map(|p| self.rotated_size(p).0).sum::<f32>()
            + PAIR_GAP * (group.len().saturating_sub(1)) as f32
    }

    fn effective_zoom(&self, viewport: (f32, f32)) -> f32 {
        match self.zoom {
            ZoomMode::Custom(z) => z,
            ZoomMode::Fit => {
                let avail_w = viewport.0 - PAD * 2.0;
                if self.display == DisplayMode::SinglePage {
                    let g = self.row_groups().next().unwrap_or(0..0);
                    let h = g.clone().map(|p| self.rotated_size(p).1).fold(1.0, f32::max);
                    (avail_w / self.row_width(&g).max(1.0)).min((viewport.1 - PAD * 2.0) / h)
                } else {
                    let widest = self.row_groups().map(|g| self.row_width(&g)).fold(1.0, f32::max);
                    avail_w / widest
                }
            }
        }
        .clamp(MIN_ZOOM, MAX_ZOOM)
    }

    fn layout_key(&self, viewport: (f32, f32), scale_factor: f32) -> LayoutKey {
        LayoutKey {
            viewport,
            scale_factor,
            zoom: self.zoom,
            display: self.display,
            rotation: self.rotation,
            single_page: if self.display == DisplayMode::SinglePage { self.current_page } else { 0 },
            doc: self.doc_generation,
        }
    }

    /// Lays pages out on whole device pixels, so each page bitmap (rendered at
    /// zoom × scale factor) lands 1:1 on the screen's pixels and text stays crisp.
    fn compute_layout(&self, viewport: (f32, f32), scale_factor: f32) -> Layout {
        if self.doc.is_none() {
            return Layout::default();
        }
        let snap = |v: f32| (v * scale_factor).round() / scale_factor;
        let count = self.page_count();
        let zoom = self.effective_zoom(viewport);
        let mut rows = Vec::with_capacity(count);
        let mut slots = Vec::with_capacity(count);
        let mut page_row = vec![u32::MAX; count];
        let mut y = PAD;
        let mut widest = 0.0f32;
        for group in self.row_groups() {
            let row_w = self.row_width(&group) * zoom - PAIR_GAP * (group.len().saturating_sub(1)) as f32 * (zoom - 1.0);
            let row_h = group.clone().map(|p| snap(self.rotated_size(p).1 * zoom)).fold(0.0, f32::max);
            widest = widest.max(row_w);
            let first = slots.len();
            let mut x = -row_w / 2.0;
            for page in group {
                let (w, h) = self.rotated_size(page);
                let (w, h) = (snap(w * zoom), snap(h * zoom));
                slots.push(Slot {
                    page,
                    x,
                    y: snap(y + (row_h - h) / 2.0),
                    w,
                    h,
                });
                page_row[page] = rows.len() as u32;
                x += w + PAIR_GAP;
            }
            rows.push(Row {
                top: y,
                height: row_h,
                slots: first..slots.len(),
            });
            y += row_h + GAP;
        }
        let width = viewport.0.max(widest + PAD * 2.0);
        let mut height = y - GAP + PAD;
        if self.display == DisplayMode::SinglePage {
            height = height.max(viewport.1);
        }
        // Slots were laid out around x = 0; shift to the content's center.
        for slot in &mut slots {
            slot.x = snap(slot.x + width / 2.0);
        }
        if self.display == DisplayMode::SinglePage {
            for row in &mut rows {
                if row.height + PAD * 2.0 < viewport.1 {
                    let dy = snap((viewport.1 - row.height) / 2.0 - row.top);
                    row.top += dy;
                    for slot in &mut slots[row.slots.clone()] {
                        slot.y += dy;
                    }
                }
            }
        }
        Layout {
            zoom,
            rows,
            slots,
            page_row,
            width,
            height,
            single: self.display == DisplayMode::SinglePage,
        }
    }

    fn scroll_pos(&self) -> (f32, f32) {
        let offset = self.scroll.offset();
        (-offset.x.as_f32(), -offset.y.as_f32())
    }

    fn set_scroll_pos(&self, x: f32, y: f32, viewport: (f32, f32), layout: &Layout) {
        let x = x.clamp(0.0, (layout.width - viewport.0).max(0.0));
        let y = y.clamp(0.0, (layout.height - viewport.1).max(0.0));
        self.scroll.set_offset(point(px(-x), px(-y)));
    }

    fn anchor_at(&self, viewport_point: Point<Pixels>) -> Option<Anchor> {
        let (sx, sy) = self.scroll_pos();
        let cy = sy + viewport_point.y.as_f32();
        let cx = sx + viewport_point.x.as_f32();
        let row_ix = self.layout.row_at(cy)?;
        let row = &self.layout.rows[row_ix];
        let slot = self.layout.row_slots(row_ix).first()?;
        Some(Anchor {
            page: slot.page,
            frac_y: (cy - row.top) / row.height.max(1.0),
            frac_x: cx / self.layout.width.max(1.0),
            viewport: viewport_point,
        })
    }

    fn apply_anchor(&self, anchor: Anchor, layout: &Layout, viewport: (f32, f32)) {
        let Some(row_ix) = layout.row_of_page(anchor.page) else { return };
        let row = &layout.rows[row_ix];
        let y = row.top + anchor.frac_y * row.height - anchor.viewport.y.as_f32();
        let x = anchor.frac_x * layout.width - anchor.viewport.x.as_f32();
        self.set_scroll_pos(x, y, viewport, layout);
    }

    // ------------------------------------------------------------- navigation

    fn go_to_page(&mut self, page: usize, cx: &mut Context<Self>) {
        let count = self.page_count();
        if count == 0 {
            return;
        }
        let page = page.min(count - 1);
        self.current_page = page;
        self.smooth.cancel();
        if self.display == DisplayMode::SinglePage {
            self.scroll.set_offset(point(px(0.), px(0.)));
            self.layout = Layout::default();
            self.layout_key = None;
        } else if let Some(row) = self.layout.row_of_page(page) {
            let top = self.layout.rows[row].top - PAD * 0.5;
            let (sx, _) = self.scroll_pos();
            self.scroll.set_offset(point(px(-sx), px(-top.max(0.0))));
        }
        cx.notify();
    }

    fn step_page(&mut self, forward: bool, cx: &mut Context<Self>) {
        let step = if self.display == DisplayMode::TwoPages { 2 } else { 1 };
        let base = if self.display == DisplayMode::TwoPages {
            self.current_page - self.current_page % 2
        } else {
            self.current_page
        };
        let target = if forward {
            base + step
        } else {
            base.saturating_sub(step)
        };
        if target < self.page_count() {
            self.go_to_page(target, cx);
        }
    }

    fn scroll_by(&mut self, dy: f32, window: &mut Window, cx: &mut Context<Self>) {
        let viewport = self.viewport(window);
        let (_, sy) = self.scroll_pos();
        let max = (self.layout.height - viewport.1).max(0.0);
        if self.display == DisplayMode::SinglePage {
            if dy > 0.0 && sy >= max - 0.5 && self.current_page + 1 < self.page_count() {
                return self.go_to_page(self.current_page + 1, cx);
            }
            if dy < 0.0 && sy <= 0.5 && self.current_page > 0 {
                self.go_to_page(self.current_page - 1, cx);
                self.scroll.set_offset(point(px(0.), px(-1.0e6)));
                return;
            }
        }
        let max = self.max_scroll(viewport);
        self.smooth.scroll_by(&self.scroll, 0.0, dy, max);
        cx.notify();
    }

    fn max_scroll(&self, viewport: (f32, f32)) -> (f32, f32) {
        (
            (self.layout.width - viewport.0).max(0.0),
            (self.layout.height - viewport.1).max(0.0),
        )
    }

    /// Mouse wheel / touchpad scrolling on the page canvas.
    fn on_canvas_scroll(&mut self, event: &ScrollEvent, window: &mut Window, cx: &mut Context<Self>) {
        let (dx, dy) = wheel_delta(event);
        let viewport = self.viewport(window);
        if self.display == DisplayMode::SinglePage && dy != 0.0 {
            // At the ends of a single page, the wheel turns the page.
            let (_, sy) = self.scroll_pos();
            let max = self.max_scroll(viewport).1;
            if (dy > 0.0 && sy >= max - 0.5) || (dy < 0.0 && sy <= 0.5) {
                if !event.delta.precise() || event.touch_phase == gpui::TouchPhase::Started {
                    self.scroll_by(dy, window, cx);
                }
                return;
            }
        }
        let max = self.max_scroll(viewport);
        if event.delta.precise() {
            // Touchpads already deliver smooth, momentum-scrolled deltas.
            self.smooth.jump_by(&self.scroll, dx, dy, max);
        } else {
            self.smooth.scroll_by(&self.scroll, dx, dy, max);
        }
        cx.notify();
    }

    fn set_zoom(&mut self, zoom: ZoomMode, anchor: Option<Point<Pixels>>, window: &Window, cx: &mut Context<Self>) {
        self.smooth.cancel();
        let viewport = self.viewport(window);
        let anchor = anchor.unwrap_or(point(px(viewport.0 / 2.0), px(viewport.1 / 2.0)));
        self.pending_anchor = self.anchor_at(anchor);
        self.zoom = match zoom {
            ZoomMode::Custom(z) => ZoomMode::Custom(z.clamp(MIN_ZOOM, MAX_ZOOM)),
            fit => fit,
        };
        self.flash_zoom_hud(cx);
        cx.notify();
    }

    fn flash_zoom_hud(&mut self, cx: &mut Context<Self>) {
        const SHOW_FOR: Duration = Duration::from_millis(1100);
        self.zoom_hud_until = Some(Instant::now() + SHOW_FOR);
        self._zoom_hud_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SHOW_FOR + Duration::from_millis(30)).await;
            this.update(cx, |_, cx| cx.notify()).ok();
        }));
    }

    fn zoom_by(&mut self, factor: f32, anchor: Option<Point<Pixels>>, window: &Window, cx: &mut Context<Self>) {
        if self.doc.is_none() {
            return;
        }
        let current = self.layout.zoom.max(MIN_ZOOM);
        self.set_zoom(ZoomMode::Custom(current * factor), anchor, window, cx);
    }

    fn set_display(&mut self, display: DisplayMode, cx: &mut Context<Self>) {
        if self.display != display {
            self.pending_anchor = Some(Anchor {
                page: self.current_page,
                frac_y: 0.0,
                frac_x: 0.5,
                viewport: point(px(0.), px(PAD * 0.5)),
            });
            self.smooth.cancel();
            self.display = display;
            if display == DisplayMode::SinglePage {
                self.zoom = ZoomMode::Fit;
            }
            self.popover = None;
            cx.notify();
        }
    }

    fn rotate(&mut self, clockwise: bool, cx: &mut Context<Self>) {
        if self.doc.is_none() {
            return;
        }
        self.pending_anchor = Some(Anchor {
            page: self.current_page,
            frac_y: 0.0,
            frac_x: 0.5,
            viewport: point(px(0.), px(PAD * 0.5)),
        });
        self.smooth.cancel();
        self.rotation = (self.rotation + if clockwise { 90 } else { 270 }) % 360;
        self.selection = None;
        cx.notify();
    }

    // ---------------------------------------------------------------- search

    fn set_query(&mut self, query: String, cx: &mut Context<Self>) {
        if query == self.query {
            return;
        }
        // Matches are picked relative to where the user was when they started typing.
        if self.query.trim().is_empty() {
            self.search_anchor_page = self.current_page;
        }
        self.query = query;
        self.hits.clear();
        self.current_hit = None;
        self.hit_list.reset(0);
        self.search_gen += 1;
        self.needle = fold_query(&self.query).into();
        if self.needle.is_empty() {
            self._search_task = None;
            self.search_running = false;
            cx.notify();
            return;
        }
        self.request_all_text();
        self.hit_auto = true;

        // Scan already-extracted pages off the UI thread. Pages whose text arrives
        // meanwhile are scanned as they land (see `on_job_result`) and merged here.
        let Some(doc) = self.doc.as_ref() else { return };
        let pages: Vec<Option<Arc<[char]>>> = doc.folded.clone();
        let scanned: Vec<bool> = pages.iter().map(Option::is_some).collect();
        let needle = self.needle.clone();
        let generation = self.search_gen;
        self.search_running = true;
        self._search_task = Some(cx.spawn(async move |this, cx| {
            let found = cx
                .background_executor()
                .spawn(async move {
                    let mut hits = Vec::new();
                    for (page, hay) in pages.iter().enumerate() {
                        if let Some(hay) = hay {
                            scan_page(page, hay, &needle, &mut hits);
                        }
                    }
                    hits
                })
                .await;
            this.update(cx, |this, cx| {
                if this.search_gen != generation {
                    return;
                }
                let current = this.current_hit.and_then(|ix| this.hits.get(ix)).map(|h| (h.page, h.start));
                let mut hits = found;
                hits.extend(this.hits.drain(..).filter(|h| !scanned[h.page]));
                hits.sort_unstable_by_key(|h| (h.page, h.start));
                this.hits = hits;
                this.search_running = false;
                this.hit_list.reset(this.hits.len());
                this.current_hit = current.and_then(|key| this.hits.iter().position(|h| (h.page, h.start) == key));
                if this.hit_auto {
                    this.pick_auto_hit();
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// While the user hasn't navigated, keep the current match at the first one at or
    /// after the page they started searching from.
    fn pick_auto_hit(&mut self) {
        if self.hits.is_empty() {
            return;
        }
        let first = self.hits.partition_point(|h| h.page < self.search_anchor_page);
        let ix = if first < self.hits.len() { first } else { 0 };
        if self.current_hit != Some(ix) {
            self.current_hit = Some(ix);
            self.reveal_hit(ix);
        }
    }

    /// Rects of a match in unrotated page space.
    fn hit_rects(&self, hit: &SearchHit) -> Vec<kurbo::Rect> {
        self.doc
            .as_ref()
            .and_then(|d| d.text.get(hit.page)?.as_ref().cloned())
            .map(|text| merge_rects(&text[hit.range()]))
            .unwrap_or_default()
    }

    fn select_hit(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.hits.len() {
            self.hit_auto = false;
            self.current_hit = Some(ix);
            self.reveal_hit(ix);
            cx.notify();
        }
    }

    fn step_hit(&mut self, forward: bool, cx: &mut Context<Self>) {
        let n = self.hits.len();
        if n == 0 {
            return;
        }
        let ix = match self.current_hit {
            None => 0,
            Some(i) if forward => (i + 1) % n,
            Some(i) => (i + n - 1) % n,
        };
        self.select_hit(ix, cx);
    }

    /// Scrolls so the given hit is visible (applied on next frame's layout).
    fn reveal_hit(&mut self, ix: usize) {
        self.smooth.cancel();
        let Some(hit) = self.hits.get(ix).copied() else { return };
        let page = hit.page;
        let rect = self.hit_rects(&hit).first().copied();
        self.current_page = page;
        self.hit_list.scroll_to_reveal_item(ix);
        if self.display == DisplayMode::SinglePage {
            self.layout = Layout::default();
            self.layout_key = None;
        }
        if let (Some(rect), Some((w, h))) = (rect, self.unrotated_size(page)) {
            let r = page_to_display(rect, self.rotation, w, h, 1.0);
            let (_, rh) = self.rotated_size(page);
            self.pending_anchor = Some(Anchor {
                page,
                frac_y: (r.y0 as f32 / rh).clamp(0.0, 1.0),
                frac_x: 0.5,
                viewport: point(px(0.), px(120.)),
            });
        }
    }

    fn unrotated_size(&self, page: usize) -> Option<(f32, f32)> {
        self.doc.as_ref()?.info.page_sizes.get(page).copied()
    }

    // ------------------------------------------------------------- selection

    /// Converts a window position into page space (unrotated points) for `slot`.
    fn window_to_page(&self, position: Point<Pixels>, slot: &Slot) -> Option<kurbo::Point> {
        let (ox, oy) = self.content_origin();
        let (sx, sy) = self.scroll_pos();
        let x = position.x.as_f32() - ox + sx - slot.x;
        let y = position.y.as_f32() - oy + sy - slot.y;
        let (w, h) = self.unrotated_size(slot.page)?;
        let zoom = self.layout.zoom as f64;
        let to_display = Affine::scale(zoom) * document::rotation_transform(self.rotation, w as f64, h as f64);
        Some(to_display.inverse() * kurbo::Point::new(x as f64, y as f64))
    }

    fn slot_at(&self, position: Point<Pixels>) -> Option<Slot> {
        let (ox, oy) = self.content_origin();
        let (sx, sy) = self.scroll_pos();
        let x = position.x.as_f32() - ox + sx;
        let y = position.y.as_f32() - oy + sy;
        self.layout
            .rows_between(y, y)
            .flat_map(|row| self.layout.row_slots(row).iter())
            .find(|s| x >= s.x && x <= s.x + s.w && y >= s.y && y <= s.y + s.h)
            .copied()
    }

    fn selection_range(&self) -> Option<(usize, Range<usize>)> {
        let sel = self.selection?;
        let text = self.doc.as_ref()?.text.get(sel.page)?.as_ref()?;
        if sel.mode == SelectMode::Char && (sel.anchor - sel.head).hypot() < 2.0 {
            return None;
        }
        let a = nearest_char(text, sel.anchor)?;
        let b = nearest_char(text, sel.head)?;
        let (mut start, mut end) = if a <= b { (a, b) } else { (b, a) };
        let boundary: fn(char) -> bool = match sel.mode {
            SelectMode::Char => return Some((sel.page, start..end + 1)),
            SelectMode::Word => |c| !c.is_alphanumeric(),
            SelectMode::Line => |c| c == '\n',
        };
        while start > 0 && !boundary(text[start - 1].ch) {
            start -= 1;
        }
        while end + 1 < text.len() && !boundary(text[end + 1].ch) {
            end += 1;
        }
        Some((sel.page, start..end + 1))
    }

    /// Recomputes the cached selection after mouse input or when the page's text arrives.
    fn refresh_selection(&mut self) {
        self.sel_cache = self.selection_range().and_then(|(page, range)| {
            let text = self.doc.as_ref()?.text.get(page)?.as_ref()?;
            let rects = merge_rects(&text[range.clone()]);
            Some((page, range, Arc::new(rects)))
        });
    }

    fn selection_rects(&self) -> Option<(usize, Arc<Vec<kurbo::Rect>>)> {
        self.selection?;
        self.sel_cache.as_ref().map(|(page, _, rects)| (*page, rects.clone()))
    }

    fn selected_text(&self) -> Option<String> {
        self.selection?;
        let (page, range, _) = self.sel_cache.clone()?;
        let text = self.doc.as_ref()?.text.get(page)?.as_ref()?;
        Some(text[range].iter().map(|c| c.ch).collect::<String>().trim().to_string())
    }

    fn commit_highlight(&mut self) -> bool {
        if let Some((page, rects)) = self.selection_rects() {
            if !rects.is_empty() {
                self.highlights.push(Highlight {
                    page,
                    rects: rects.as_ref().clone(),
                });
                self.selection = None;
                return true;
            }
        }
        false
    }

    fn on_canvas_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        self.popover = None;
        self.selection = None;
        if let Some(slot) = self.slot_at(event.position) {
            self.request_text(slot.page);
            if let Some(p) = self.window_to_page(event.position, &slot) {
                self.selection = Some(Selection {
                    page: slot.page,
                    anchor: p,
                    head: p,
                    mode: match event.click_count {
                        0 | 1 => SelectMode::Char,
                        2 => SelectMode::Word,
                        _ => SelectMode::Line,
                    },
                });
                self.selecting = true;
            }
        }
        self.refresh_selection();
        cx.notify();
    }

    fn on_canvas_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selecting || !event.dragging() {
            return;
        }
        let Some(sel) = self.selection else { return };
        let Some(slot) = self.layout.slot(sel.page) else { return };
        if let Some(p) = self.window_to_page(event.position, &slot) {
            self.selection = Some(Selection { head: p, ..sel });
            self.refresh_selection();
            cx.notify();
        }
    }

    fn on_canvas_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.selecting {
            self.selecting = false;
            if self.highlight_mode {
                self.commit_highlight();
            }
            cx.notify();
        }
    }

    // ----------------------------------------------------- render scheduling

    fn sidebar_shown(&self) -> bool {
        self.doc.is_some() && (self.sidebar_visible || !self.query.is_empty())
    }

    /// Requests renders for visible pages and drops far-away bitmaps.
    fn schedule_renders(&mut self, viewport: (f32, f32), window: &mut Window, cx: &mut Context<Self>) {
        let scale_factor = window.scale_factor();
        let (_, sy) = self.scroll_pos();
        let zoom = self.layout.zoom;
        let rotation = self.rotation;
        // Prefetch a screen ahead in the direction of travel, half a screen behind.
        let upward = self.smooth.velocity_y() < 0.0;
        let ahead = if upward { (1.0, 0.5) } else { (0.5, 1.0) };
        let lo = sy - viewport.1 * ahead.0;
        let hi = sy + viewport.1 * (1.0 + ahead.1);
        let rows = self.layout.rows_between(lo, hi);
        // Nothing to do unless the visible rows, zoom or arrivals changed since last frame.
        let tiles = self.visible_tiles(viewport, scale_factor);
        let tile_jobs: Vec<Job> = tiles.iter().map(|(_, job)| *job).collect();
        let key = (rows.clone(), zoom.to_bits(), rotation, scale_factor.to_bits(), upward, tile_jobs);
        if !self.renders_dirty && self.render_key.as_ref() == Some(&key) {
            return;
        }
        self.render_key = Some(key);
        self.renders_dirty = false;
        let visible: Vec<Slot> = rows
            .flat_map(|row| self.layout.row_slots(row).iter().copied())
            .collect();
        let sizes: Vec<(Slot, (f32, f32), Job)> = visible
            .iter()
            .map(|s| (*s, self.rotated_size(s.page), self.thumb_job(s.page, scale_factor)))
            .collect();
        // While a zoom gesture is still moving, keep showing scaled bitmaps instead of
        // starting sharp renders that would be stale (and uninterruptible) a moment later.
        let settling = self.zoom_changed_at.is_some_and(|at| at.elapsed() < ZOOM_SETTLE);
        let center = sy + viewport.1 * 0.5;
        let Some(doc) = self.doc.as_mut() else { return };

        let mut want = HashSet::new();
        let mut new_jobs: Vec<(f32, Job)> = Vec::new();
        let mut placeholders = Vec::new();
        for (slot, (w, h), thumb_job) in sizes {
            let page = slot.page;
            let have_page = doc.pages.get(&page).is_some_and(|img| img.job.rotation == rotation);
            // A cheap thumbnail fills a blank page almost instantly during fast scrolling.
            if !have_page
                && !doc.thumbs.get(&page).is_some_and(|t| t.job.rotation == rotation)
                && doc.pending.insert(thumb_job)
            {
                placeholders.push(thumb_job);
            }
            if settling && have_page {
                doc.wanted.insert(page, doc.pages[&page].job);
                continue;
            }
            let mut scale = (zoom * scale_factor) as f64;
            let pixels = (w as f64 * scale) * (h as f64 * scale);
            if pixels > MAX_RENDER_PIXELS {
                scale *= (MAX_RENDER_PIXELS / pixels).sqrt();
            }
            let scale_micro = (scale * 1_000_000.0).round().max(1.0) as u32;
            let job = Job {
                kind: JobKind::Page,
                page,
                scale_micro,
                rotation,
                tile: (0, 0),
            };
            let good_enough = doc.pages.get(&page).is_some_and(|img| {
                img.job.rotation == rotation
                    && (img.job.scale_micro as f64 / scale_micro as f64 - 1.0).abs() < 0.0005
            });
            if good_enough {
                doc.wanted.insert(page, doc.pages[&page].job);
                continue;
            }
            want.insert(job);
            doc.wanted.insert(page, job);
            if doc.pending.insert(job) {
                new_jobs.push(((slot.y + slot.h * 0.5 - center).abs(), job));
            }
        }
        // Tiles: keep showing the ones we have while a zoom gesture settles; afterwards
        // request the missing visible ones and drop the rest.
        let mut new_tiles: Vec<(f32, Job)> = Vec::new();
        if settling {
            want.extend(doc.wanted_tiles.iter().copied());
        } else {
            let wanted: HashSet<Job> = tiles.iter().map(|(_, job)| *job).collect();
            for (distance, job) in tiles {
                if !doc.tiles.contains_key(&job) && doc.pending.insert(job) {
                    new_tiles.push((distance, job));
                }
            }
            let stale: Vec<Job> = doc.tiles.keys().filter(|job| !wanted.contains(job)).copied().collect();
            for job in stale {
                if let Some(tile) = doc.tiles.remove(&job) {
                    cx.drop_image(tile.image, Some(&mut *window));
                }
            }
            want.extend(wanted.iter().copied());
            doc.wanted_tiles = wanted;
        }
        new_tiles.sort_by(|a, b| a.0.total_cmp(&b.0));
        for dropped in doc.pool.retain_pages(&want) {
            doc.pending.remove(&dropped);
        }
        // Pages nearest the middle of the screen first, placeholders ahead of everything.
        new_jobs.sort_by(|a, b| a.0.total_cmp(&b.0));
        doc.pool.push_front(new_jobs.into_iter().map(|(_, job)| job));
        doc.pool.push_front(new_tiles.into_iter().map(|(_, job)| job));
        doc.pool.push_front(placeholders);

        // Evict bitmaps for pages outside the prefetch range; thumbnails stand in if they return.
        if let (Some(first), Some(last)) = (visible.first(), visible.last()) {
            let keep = first.page.saturating_sub(1)..last.page + 2;
            let evict: Vec<usize> = doc.pages.keys().copied().filter(|p| !keep.contains(p)).collect();
            for page in evict {
                if let Some(img) = doc.pages.remove(&page) {
                    cx.drop_image(img.bitmap.image, Some(&mut *window));
                }
                doc.wanted.remove(&page);
            }
        }

        // Keep the bitmaps' RAM and GPU total bounded by evicting the pages farthest
        // from the screen (their thumbnails remain).
        let cost = |img: &PageImage| img.bitmap.cost();
        let mut total: usize = doc.pages.values().map(cost).sum();
        if total > PAGE_CACHE_BUDGET {
            let mut by_distance: Vec<(f32, usize)> = doc
                .pages
                .keys()
                .filter_map(|&page| {
                    let slot = self.layout.slot(page)?;
                    let on_screen = slot.y + slot.h >= sy && slot.y <= sy + viewport.1;
                    (!on_screen).then(|| ((slot.y + slot.h * 0.5 - center).abs(), page))
                })
                .collect();
            by_distance.sort_by(|a, b| b.0.total_cmp(&a.0));
            for (_, page) in by_distance {
                if total <= PAGE_CACHE_BUDGET {
                    break;
                }
                if let Some(img) = doc.pages.remove(&page) {
                    total -= cost(&img);
                    cx.drop_image(img.bitmap.image, Some(&mut *window));
                }
                doc.wanted.remove(&page);
            }
        }
    }

    /// Tiles covering the visible part (plus a margin) of every page too large to render
    /// whole at the current zoom, with each tile's distance from the viewport centre.
    fn visible_tiles(&self, viewport: (f32, f32), scale_factor: f32) -> Vec<(f32, Job)> {
        let zoom = self.layout.zoom;
        let scale_micro = ((zoom * scale_factor) as f64 * 1_000_000.0).round().max(1.0) as u32;
        let scale = scale_micro as f32 / 1_000_000.0;
        let (sx, sy) = self.scroll_pos();
        let (vx0, vy0) = (sx - TILE_MARGIN, sy - TILE_MARGIN);
        let (vx1, vy1) = (sx + viewport.0 + TILE_MARGIN, sy + viewport.1 + TILE_MARGIN);
        let center = (sx + viewport.0 * 0.5, sy + viewport.1 * 0.5);
        // Display pixels covered by one tile.
        let tile_display = TILE as f32 * zoom / scale;
        let mut out = Vec::new();
        for row in self.layout.rows_between(vy0, vy1) {
            for slot in self.layout.row_slots(row) {
                let (w, h) = self.rotated_size(slot.page);
                if (w * scale) as f64 * (h * scale) as f64 <= MAX_RENDER_PIXELS {
                    continue;
                }
                let x0 = (vx0 - slot.x).max(0.0);
                let x1 = (vx1 - slot.x).min(slot.w);
                let y0 = (vy0 - slot.y).max(0.0);
                let y1 = (vy1 - slot.y).min(slot.h);
                if x0 >= x1 || y0 >= y1 {
                    continue;
                }
                let cols = (x0 / tile_display).floor() as u16..(x1 / tile_display).ceil() as u16;
                let rows = (y0 / tile_display).floor() as u16..(y1 / tile_display).ceil() as u16;
                for ty in rows {
                    for tx in cols.clone() {
                        let cx = slot.x + (tx as f32 + 0.5) * tile_display;
                        let cy = slot.y + (ty as f32 + 0.5) * tile_display;
                        out.push((
                            (cx - center.0).hypot(cy - center.1),
                            Job {
                                kind: JobKind::Tile,
                                page: slot.page,
                                scale_micro,
                                rotation: self.rotation,
                                tile: (tx, ty),
                            },
                        ));
                    }
                }
            }
        }
        out
    }

    /// The thumbnail-size render of a page (shared by the sidebar and blank-page placeholders).
    fn thumb_job(&self, page: usize, scale_factor: f32) -> Job {
        let (thumb_w, _) = self.thumb_size(page);
        let page_w = self.rotated_size(page).0;
        Job {
            kind: JobKind::Thumbnail,
            page,
            scale_micro: (thumb_w / page_w * scale_factor * 1_000_000.0).round() as u32,
            rotation: self.rotation,
            tile: (0, 0),
        }
    }

    fn thumb_size(&self, page: usize) -> (f32, f32) {
        let (w, h) = self.rotated_size(page);
        let s = (THUMB_W / w).min(THUMB_MAX_H / h);
        (w * s, h * s)
    }

    /// (top, thumb_w, thumb_h) per page and the total height; cached per document and rotation.
    fn thumb_layout(&mut self) -> (Arc<Vec<(f32, f32, f32)>>, f32) {
        let key = (self.rotation, self.doc_generation);
        if let Some((k, items, total)) = &self.thumb_cache {
            if *k == key {
                return (items.clone(), *total);
            }
        }
        let mut y = 10.0;
        let mut items = Vec::with_capacity(self.page_count());
        for page in 0..self.page_count() {
            let (w, h) = self.thumb_size(page);
            items.push((y, w, h));
            y += h + 12.0 + 22.0 + 6.0;
        }
        let items = Arc::new(items);
        self.thumb_cache = Some((key, items.clone(), y + 6.0));
        (items, y + 6.0)
    }

    /// Indices of thumbnails overlapping `lo..hi`.
    fn thumbs_between(items: &[(f32, f32, f32)], lo: f32, hi: f32) -> Range<usize> {
        let start = items.partition_point(|(y, _, h)| y + h + 40.0 < lo);
        let end = items.partition_point(|(y, _, _)| *y <= hi);
        start..end.max(start)
    }

    fn schedule_thumbnails(&mut self, items: &[(f32, f32, f32)], viewport_h: f32, window: &mut Window, cx: &mut Context<Self>) {
        let scale_factor = window.scale_factor();
        let top = -self.sidebar_scroll.offset().y.as_f32();
        let rotation = self.rotation;
        let lo = top - viewport_h;
        let hi = top + viewport_h * 2.0;
        let range = Self::thumbs_between(items, lo, hi);
        let key = (range.clone(), rotation, scale_factor.to_bits(), self.doc_generation);
        if self.thumb_key.as_ref() == Some(&key) {
            return;
        }
        self.thumb_key = Some(key);
        let visible: Vec<(usize, f32)> = range.map(|page| (page, items[page].1)).collect();
        let sizes: Vec<f32> = visible.iter().map(|(p, _)| self.rotated_size(*p).0).collect();
        let Some(doc) = self.doc.as_mut() else { return };
        let mut jobs = Vec::new();
        for ((page, thumb_w), page_w) in visible.iter().zip(sizes) {
            let scale_micro = ((thumb_w / page_w * scale_factor * 1_000_000.0).round()) as u32;
            let job = Job {
                kind: JobKind::Thumbnail,
                page: *page,
                scale_micro,
                rotation,
                tile: (0, 0),
            };
            let ok = doc.thumbs.get(page).is_some_and(|t| t.job == job);
            if !ok && doc.pending.insert(job) {
                jobs.push(job);
            }
        }
        doc.pool.push_back(jobs);
        if let (Some(first), Some(last)) = (visible.first(), visible.last()) {
            let keep = first.0.saturating_sub(40)..last.0 + 40;
            let evict: Vec<usize> = doc.thumbs.keys().copied().filter(|p| !keep.contains(p)).collect();
            for page in evict {
                if let Some(t) = doc.thumbs.remove(&page) {
                    cx.drop_image(t.bitmap.image, Some(&mut *window));
                }
            }
        }
    }

    // ------------------------------------------------------------- rendering

    fn render_toolbar(&mut self, t: &Theme, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let has_doc = self.doc.is_some();
        let title: SharedString = self
            .doc
            .as_ref()
            .map(|d| d.info.file_name.clone().into())
            .unwrap_or_else(|| "Vellum".into());
        let subtitle: Option<SharedString> = self.doc.as_ref().map(|d| {
            let n = d.info.page_sizes.len();
            if self.display == DisplayMode::TwoPages && self.current_page + 1 < n {
                let first = self.current_page - self.current_page % 2;
                format!("Pages {}–{} of {}", first + 1, (first + 2).min(n), n).into()
            } else {
                format!("Page {} of {}", self.current_page + 1, n).into()
            }
        });
        let is_mac = cfg!(target_os = "macos");
        let search_focused = self.search_focus.is_focused(window);

        div()
            .id("toolbar")
            .flex()
            .flex_none()
            .items_center()
            .h(px(TOOLBAR_H))
            .w_full()
            // Match the traffic lights' inset at the right edge.
            .pr(px(if is_mac { 18. } else { 0. }))
            .pl(px(TOOLBAR_LEFT))
            .gap(px(6.))
            .bg(t.toolbar_bg)
            .border_b_1()
            .border_color(t.separator)
            .window_control_area(WindowControlArea::Drag)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, _| this.titlebar_should_move = true),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, _| this.titlebar_should_move = false),
            )
            .on_mouse_move(cx.listener(|this, _: &MouseMoveEvent, window, _| {
                if this.titlebar_should_move {
                    this.titlebar_should_move = false;
                    if !cfg!(target_os = "windows") {
                        window.start_window_move();
                    }
                }
            }))
            .on_click(|event: &ClickEvent, window, _| {
                if event.click_count() == 2 && !cfg!(target_os = "windows") {
                    if cfg!(target_os = "macos") {
                        window.titlebar_double_click();
                    } else {
                        window.zoom_window();
                    }
                }
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .h(px(30.))
                    .rounded(px(8.))
                    .child(
                        icon_button("sidebar", "icons/sidebar.svg", t, false)
                            .tooltip(tip("Show Sidebar", Some(shortcut("⌥⌘S", "Ctrl+Alt+S")), *t))
                            .rounded_l(px(8.))
                            .rounded_r(px(0.))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.sidebar_visible = !this.sidebar_visible;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .id("view-menu")
                            .flex()
                            .items_center()
                            .justify_center()
                            .h(px(30.))
                            .w(px(18.))
                            .rounded_r(px(8.))
                            .occlude()
                            .hover(|s| s.bg(t.hover))
                            .active(|s| s.bg(t.pressed))
                            .when(self.popover == Some(Popover::ViewMenu), |s| s.bg(t.pressed))
                            .when(self.popover != Some(Popover::ViewMenu), |d| d.tooltip(tip("View Options", None, *t)))
                            .child(icon("icons/chevron-down.svg", 10., t.icon))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.toggle_popover(Popover::ViewMenu, cx)
                            })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .justify_center()
                    .min_w(px(60.))
                    .ml(px(4.))
                    .overflow_hidden()
                    .child(
                        div()
                            .text_size(px(13.))
                            .line_height(px(16.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(t.text)
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .overflow_hidden()
                            .child(title),
                    )
                    .when_some(subtitle, |this, sub| {
                        this.child(
                            div()
                                .id("page-indicator")
                                .occlude()
                                .cursor_pointer()
                                .hover(|s| s.text_color(t.text))
                                .tooltip(tip("Go to Page…", Some(shortcut("⌥⌘G", "Ctrl+Alt+G")), *t))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.goto = Some(String::new());
                                    window.focus(&this.goto_focus, cx);
                                    cx.notify();
                                }))
                                .text_size(px(11.))
                                .line_height(px(14.))
                                .text_color(t.text_secondary)
                                .whitespace_nowrap()
                                .child(sub),
                        )
                    }),
            )
            .child(div().flex_1().min_w(px(8.)))
            .when(has_doc, |this| {
                this.child(
                    icon_button("info", "icons/info.svg", t, self.inspector)
                        .tooltip(tip("Show Inspector", Some(shortcut("⌘I", "Ctrl+I")), *t))
                        .on_click(cx.listener(
                        |this, _, _, cx| {
                            this.inspector = !this.inspector;
                            cx.notify();
                        },
                    )),
                )
                .child(
                    segmented(t)
                        .child(
                            icon_button("zoom-out", "icons/zoom-out.svg", t, false)
                                .tooltip(tip("Zoom Out", Some(shortcut("⌘−", "Ctrl+Minus")), *t))
                                .rounded(px(0.))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.zoom_by(1.0 / 1.25, None, window, cx)
                                })),
                        )
                        .child(div().w(px(1.)).h(px(16.)).bg(t.separator))
                        .child(
                            icon_button("zoom-in", "icons/zoom-in.svg", t, false)
                                .tooltip(tip("Zoom In", Some(shortcut("⌘+", "Ctrl+Plus")), *t))
                                .rounded(px(0.))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.zoom_by(1.25, None, window, cx)
                                })),
                        ),
                )
                .child(
                    icon_button("share", "icons/share.svg", t, self.popover == Some(Popover::Share))
                        .when(self.popover != Some(Popover::Share), |d| d.tooltip(tip("Share", None, *t)))
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_popover(Popover::Share, cx))),
                )
                .child(
                    icon_button("rotate", "icons/rotate.svg", t, false)
                        .tooltip(tip("Rotate Left", Some(shortcut("⌘L", "Ctrl+L")), *t))
                        .on_click(cx.listener(|this, _, _, cx| this.rotate(false, cx))),
                )
                .child(
                    icon_button("highlight", "icons/highlight.svg", t, self.highlight_mode)
                        .relative()
                        .when(self.highlight_mode, |d| {
                            d.bg(t.highlight_bg).child(
                                div()
                                    .absolute()
                                    .bottom(px(5.))
                                    .w(px(14.))
                                    .h(px(2.))
                                    .rounded_full()
                                    .bg(t.highlight_ink),
                            )
                        })
                        .tooltip(tip(
                            if self.highlight_mode { "Stop Highlighting" } else { "Highlight" },
                            None,
                            *t,
                        ))
                        .on_click(cx.listener(|this, _, _, cx| {
                            // With a selection, highlight it; otherwise toggle highlight mode.
                            if !this.commit_highlight() {
                                this.highlight_mode = !this.highlight_mode;
                            }
                            cx.notify();
                        })),
                )
                .child(self.render_search_field(t, search_focused, cx))
            })
            .when(!is_mac, |this| this.child(caption_buttons(t, window)))
    }

    fn render_search_field(&self, t: &Theme, focused: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let empty = self.query.is_empty();
        let hits = self.hits.len();
        div()
            .id("search-field")
            .relative()
            .key_context("SearchField")
            .track_focus(&self.search_focus)
            .occlude()
            .flex()
            .items_center()
            .gap(px(5.))
            .h(px(28.))
            .w(px(220.))
            .px(px(8.))
            .ml(px(4.))
            .rounded(px(14.))
            .bg(t.field_bg)
            .border_1()
            .border_color(if focused { t.accent.opacity(0.7) } else { t.field_border })
            .cursor(CursorStyle::IBeam)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    window.focus(&this.search_focus, cx);
                    cx.stop_propagation();
                }),
            )
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let mut text = this.query.clone();
                match edit_text(&mut text, event, cx) {
                    TextEdit::Changed => this.set_query(text, cx),
                    TextEdit::Submit(back) => this.step_hit(!back, cx),
                    TextEdit::Cancel => {
                        this.set_query(String::new(), cx);
                        window.focus(&this.focus, cx);
                    }
                    TextEdit::None => return,
                }
                cx.stop_propagation();
            }))
            .child(input_target(self.search_focus.clone(), cx))
            .child(icon("icons/search.svg", 14., t.text_secondary))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .items_center()
                    .overflow_hidden()
                    .text_size(px(13.))
                    .whitespace_nowrap()
                    .when(empty && !focused, |d| d.text_color(t.text_tertiary).child("Search"))
                    .when(!empty, |d| d.text_color(t.text).child(self.query.clone()))
                    .when(focused, |d| d.child(div().w(px(1.5)).h(px(15.)).ml(px(1.)).bg(t.accent))),
            )
            .when(!empty && hits > 0, |d| {
                let label = match self.current_hit {
                    Some(ix) => format!("{} of {}", ix + 1, hits),
                    None => format!("{hits}"),
                };
                d.child(
                    div()
                        .flex_none()
                        .text_size(px(11.))
                        .text_color(t.text_secondary)
                        .child(label),
                )
            })
            .when(!empty, |d| {
                d.child(
                    div()
                        .id("clear-search")
                        .cursor_pointer()
                        .child(icon("icons/xmark-circle.svg", 14., t.text_tertiary))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(|this, _, _, cx| this.set_query(String::new(), cx))),
                )
            })
    }

    fn toggle_popover(&mut self, popover: Popover, cx: &mut Context<Self>) {
        self.popover = if self.popover == Some(popover) {
            None
        } else {
            Some(popover)
        };
        cx.notify();
    }

    fn render_sidebar(&mut self, t: &Theme, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let body: AnyElement = if !self.query.is_empty() {
            self.render_search_results(t, cx)
        } else {
            match self.sidebar_mode {
                SidebarMode::Thumbnails => self.render_thumbnails(t, window, cx),
                SidebarMode::Contents => self.render_contents(t, cx),
            }
        };
        div()
            .flex()
            .flex_none()
            .h_full()
            .child(
                div()
                    .w(px(self.sidebar_width))
                    .h_full()
                    .bg(t.sidebar_bg)
                    .child(body),
            )
            .child(
                div()
                    .id("sidebar-resize")
                    .w(px(1.))
                    .h_full()
                    .bg(t.separator)
                    .relative()
                    .child(
                        div()
                            .id("sidebar-resize-handle")
                            .absolute()
                            .left(px(-3.))
                            .w(px(7.))
                            .h_full()
                            .cursor(CursorStyle::ResizeLeftRight)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                                    this.resizing_sidebar = true;
                                    cx.stop_propagation();
                                }),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_thumbnails(&mut self, t: &Theme, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let (items, total_h) = self.thumb_layout();
        let viewport_h = window.viewport_size().height.as_f32() - TOOLBAR_H;
        self.thumb_max = (total_h - viewport_h).max(0.0);

        // Keep the current page's thumbnail in view when the page changes.
        if self.last_scrolled_thumb != Some(self.current_page) {
            self.last_scrolled_thumb = Some(self.current_page);
            if let Some(&(y, _, h)) = items.get(self.current_page) {
                let top = -self.sidebar_scroll.offset().y.as_f32();
                if y - 10.0 < top || y + h + 40.0 > top + viewport_h {
                    // Glide the sidebar along, as macOS Preview does.
                    let target = (y - viewport_h / 3.0).clamp(0.0, (total_h - viewport_h).max(0.0));
                    let current = -self.sidebar_scroll.offset().y.as_f32();
                    self.thumb_smooth.scroll_by(&self.sidebar_scroll, 0.0, target - current, (0.0, self.thumb_max));
                }
            }
        }
        self.schedule_thumbnails(&items, viewport_h, window, cx);

        let top = -self.sidebar_scroll.offset().y.as_f32();
        let width = self.sidebar_width;
        let current = self.current_page;
        let pair = self.display == DisplayMode::TwoPages;
        let doc = self.doc.as_ref().unwrap();
        let children: Vec<AnyElement> = Self::thumbs_between(&items, top - 200.0, top + viewport_h + 200.0)
            .map(|page| {
                let (y, w, h) = items[page];
                let selected = page == current || (pair && page == (current - current % 2) + 1 && page < items.len());
                let thumb = doc.thumbs.get(&page).map(|t| t.bitmap.paint());
                div()
                    .id(("thumb", page))
                    .absolute()
                    .top(px(y))
                    .left(px(0.))
                    .w(px(width))
                    .flex()
                    .flex_col()
                    .items_center()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.go_to_page(page, cx)))
                    .child(
                        div()
                            .p(px(6.))
                            .rounded(px(7.))
                            .when(selected, |d| d.bg(t.selection))
                            .child(
                                div()
                                    .w(px(w))
                                    .h(px(h))
                                    .bg(gpui::white())
                                    .border_1()
                                    .border_color(t.page_border)
                                    .shadow_sm()
                                    .when_some(thumb, |d, image| {
                                        d.child(bitmap_img(image, w, h))
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .mt(px(3.))
                            .px(px(7.))
                            .h(px(18.))
                            .flex()
                            .items_center()
                            .rounded(px(9.))
                            .text_size(px(11.))
                            .when(selected, |d| d.bg(t.accent).text_color(t.accent_text))
                            .when(!selected, |d| d.text_color(t.text_secondary))
                            .child(format!("{}", page + 1)),
                    )
                    .into_any_element()
            })
            .collect();

        div()
            .id("thumbnails")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.sidebar_scroll)
            .child(
                div()
                    .relative()
                    .w_full()
                    .h(px(total_h))
                    .on_scroll(cx.listener(|this, event: &ScrollEvent, _, cx| {
                        let (_, dy) = wheel_delta(event);
                        let max = (0.0, this.thumb_max);
                        if event.delta.precise() {
                            this.thumb_smooth.jump_by(&this.sidebar_scroll, 0.0, dy, max);
                        } else {
                            this.thumb_smooth.scroll_by(&this.sidebar_scroll, 0.0, dy, max);
                        }
                        cx.stop_propagation();
                        cx.notify();
                    }))
                    .children(children),
            )
            .into_any_element()
    }

    /// Visible (expanded) outline rows and the row to highlight, cached until the page,
    /// expansion state or document changes.
    fn toc_rows(&mut self) -> (Arc<Vec<usize>>, Option<usize>) {
        let key = (self.current_page, self.toc_version, self.doc_generation);
        if let Some((k, rows, current)) = &self.toc_cache {
            if *k == key {
                return (rows.clone(), *current);
            }
        }
        let Some(doc) = self.doc.as_ref() else {
            return (Arc::default(), None);
        };
        let outline = &doc.info.outline;
        // The current section is the last item whose page is at or before the current page.
        let current_item = outline
            .iter()
            .enumerate()
            .filter(|(_, item)| item.page.is_some_and(|p| p <= self.current_page))
            .map(|(ix, _)| ix)
            .last();
        // If the current item sits inside a collapsed parent, highlight that parent instead.
        let current_item = current_item.map(|mut ix| {
            let mut visible = ix;
            let mut depth = outline[ix].depth;
            while ix > 0 {
                ix -= 1;
                if outline[ix].depth < depth {
                    depth = outline[ix].depth;
                    if !self.toc_expanded.contains(&ix) {
                        visible = ix;
                    }
                }
            }
            visible
        });
        let mut rows = Vec::new();
        let mut hidden_below: Option<usize> = None;
        for (ix, item) in outline.iter().enumerate() {
            if let Some(depth) = hidden_below {
                if item.depth > depth {
                    continue;
                }
                hidden_below = None;
            }
            if item.has_children && !self.toc_expanded.contains(&ix) {
                hidden_below = Some(item.depth);
            }
            rows.push(ix);
        }
        let rows = Arc::new(rows);
        self.toc_cache = Some((key, rows.clone(), current_item));
        (rows, current_item)
    }

    fn render_contents(&mut self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        if self.doc.as_ref().is_none_or(|d| d.info.outline.is_empty()) {
            return sidebar_placeholder(t, "No Table of Contents");
        }
        let (rows, _) = self.toc_rows();
        let t = *t;
        uniform_list(
            "contents",
            rows.len(),
            cx.processor(move |this, range: Range<usize>, _, cx| this.render_toc_rows(range, &t, cx)),
        )
        .track_scroll(&self.toc_scroll)
        .size_full()
        .py(px(6.))
        .into_any_element()
    }

    fn render_toc_rows(&mut self, range: Range<usize>, t: &Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let (rows, current_item) = self.toc_rows();
        let Some(doc) = self.doc.as_ref() else { return Vec::new() };
        let outline = &doc.info.outline;
        range
            .filter_map(|row| rows.get(row).copied())
            .map(|ix| {
                let item = &outline[ix];
                let expanded = self.toc_expanded.contains(&ix);
                let selected = Some(ix) == current_item;
                let page = item.page;
                let row = div()
                    .id(("toc", ix))
                    .flex()
                    .items_center()
                    .gap(px(2.))
                    .w_full()
                    .h(px(24.))
                    .pl(px(4. + item.depth as f32 * 14.))
                    .pr(px(8.))
                    .rounded(px(5.))
                    .text_size(px(12.5))
                    .when(selected, |d| d.bg(t.accent).text_color(t.accent_text))
                    .when(!selected, |d| d.text_color(t.text).hover(|s| s.bg(t.hover)))
                    .child(
                        div()
                            .id(("toc-disclosure", ix))
                            .w(px(14.))
                            .flex()
                            .flex_none()
                            .justify_center()
                            .when(item.has_children, |d| {
                                d.child(icon(
                                    if expanded { "icons/chevron-down.svg" } else { "icons/chevron-right.svg" },
                                    11.,
                                    if selected { t.accent_text } else { t.icon },
                                ))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if !this.toc_expanded.remove(&ix) {
                                        this.toc_expanded.insert(ix);
                                    }
                                    this.toc_version += 1;
                                    cx.stop_propagation();
                                    cx.notify();
                                }))
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(if item.title.is_empty() { "Untitled".to_string() } else { item.title.clone() }),
                    )
                    .when_some(page, |d, p| {
                        d.child(
                            div()
                                .flex_none()
                                .text_size(px(11.))
                                .text_color(if selected { t.accent_text.opacity(0.85) } else { t.text_tertiary })
                                .child(format!("{}", p + 1)),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| this.go_to_page(p, cx)))
                    });
                div().w_full().px(px(8.)).child(row).into_any_element()
            })
            .collect()
    }

    fn render_search_results(&mut self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let searching = self.search_running || self.doc.as_ref().is_some_and(|d| d.text_pending > 0);
        let header = match (self.hits.len(), searching) {
            (0, true) => "Searching…".to_string(),
            (0, false) => "No Results".to_string(),
            (1, _) => "1 match".to_string(),
            (n, _) => format!("{n} matches"),
        };
        let theme = *t;
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(
                div()
                    .flex()
                    .items_center()
                    .h(px(34.))
                    .pl(px(16.))
                    .pr(px(8.))
                    .gap(px(2.))
                    .text_size(px(11.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(t.text_secondary)
                    .border_b_1()
                    .border_color(t.separator)
                    .child(div().flex_1().child(header))
                    .when(!self.hits.is_empty(), |d| {
                        d.child(
                            small_icon_button("hit-prev", "icons/chevron-up.svg", t)
                                .tooltip(tip("Previous Match", Some(shortcut("⇧⌘G", "Shift+F3")), *t))
                                .on_click(cx.listener(|this, _, _, cx| this.step_hit(false, cx))),
                        )
                        .child(
                            small_icon_button("hit-next", "icons/chevron-down.svg", t)
                                .tooltip(tip("Next Match", Some(shortcut("⌘G", "F3")), *t))
                                .on_click(cx.listener(|this, _, _, cx| this.step_hit(true, cx))),
                        )
                    }),
            )
            .child(
                list(
                    self.hit_list.clone(),
                    cx.processor(move |this, ix: usize, _, cx| this.render_hit_row(ix, &theme, cx)),
                )
                .flex_1()
                .min_h_0()
                .w_full(),
            )
            .into_any_element()
    }

    /// One search result (plus a page header when it starts a new page), built lazily.
    fn render_hit_row(&mut self, ix: usize, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let Some(hit) = self.hits.get(ix).copied() else {
            return div().into_any_element();
        };
        let text = self.doc.as_ref().and_then(|d| d.text.get(hit.page)?.clone());
        let (snippet, highlight) = text.as_deref().map(|text| hit_snippet(text, &hit)).unwrap_or_default();
        let new_page = ix == 0 || self.hits[ix - 1].page != hit.page;
        let last = ix + 1 == self.hits.len();
        let selected = self.current_hit == Some(ix);
        let styled = StyledText::new(snippet).with_highlights([(
            highlight,
            HighlightStyle {
                font_weight: Some(FontWeight::BOLD),
                ..Default::default()
            },
        )]);
        div()
            .flex()
            .flex_col()
            .when(new_page, |d| {
                d.child(
                    div()
                        .px(px(16.))
                        .pt(px(10.))
                        .pb(px(3.))
                        .text_size(px(11.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(t.text_secondary)
                        .child(format!("Page {}", hit.page + 1)),
                )
            })
            .child(
                div()
                    .id(("hit", ix))
                    .mx(px(8.))
                    .px(px(8.))
                    .py(px(5.))
                    .rounded(px(6.))
                    .text_size(px(12.))
                    .line_height(px(16.))
                    .cursor_pointer()
                    .when(selected, |d| d.bg(t.accent).text_color(t.accent_text))
                    .when(!selected, |d| d.text_color(t.text).hover(|s| s.bg(t.hover)))
                    .line_clamp(3)
                    .child(styled)
                    .on_click(cx.listener(move |this, _, _, cx| this.select_hit(ix, cx))),
            )
            .when(last, |d| d.pb(px(8.)))
            .into_any_element()
    }

    fn render_canvas(&mut self, t: &Theme, viewport: (f32, f32), cx: &mut Context<Self>) -> AnyElement {
        let Some(doc) = self.doc.as_ref() else {
            return self.render_empty_state(t, cx);
        };
        let (_, sy) = self.scroll_pos();
        let zoom = self.layout.zoom;
        let rotation = self.rotation;
        let lo = sy - viewport.1 * 0.25;
        let hi = sy + viewport.1 * 1.25;
        let selection = self.selection_rects();
        let current_hit = self.current_hit;

        let mut pages = Vec::new();
        for row in self.layout.rows_between(lo, hi) {
            for slot in self.layout.row_slots(row) {
                let page = slot.page;
                let image = doc.pages.get(&page).filter(|p| p.job.rotation == rotation).map(|p| p.bitmap.paint());
                let fallback = image.is_none().then(|| doc.thumbs.get(&page).filter(|p| p.job.rotation == rotation).map(|p| p.bitmap.paint())).flatten();
                let (w, h) = doc.info.page_sizes[page];
                let to_display = |r: &kurbo::Rect| page_to_display(*r, rotation, w, h, zoom);
                // Highlights, matches and the selection are painted as plain quads in one pass.
                let mut quads: Vec<(kurbo::Rect, Hsla)> = Vec::new();
                for hl in self.highlights.iter().filter(|h| h.page == page) {
                    for r in &hl.rects {
                        quads.push((to_display(r), rgba_hsla(0xFFE45C, 0.55)));
                    }
                }
                if let Some(text) = doc.text.get(page).and_then(|t| t.as_deref()) {
                    let first = self.hits.partition_point(|h| h.page < page);
                    let end = self.hits.partition_point(|h| h.page <= page);
                    for ix in first..end {
                        let color = if Some(ix) == current_hit { t.find_current } else { t.find_highlight };
                        for r in merge_rects(&text[self.hits[ix].range()]) {
                            quads.push((to_display(&r).inflate(1.5, 1.0), color));
                        }
                    }
                }
                if let Some((sel_page, rects)) = &selection {
                    if *sel_page == page {
                        for r in rects.iter() {
                            quads.push((to_display(r), t.accent.opacity(0.28)));
                        }
                    }
                }
                pages.push(
                    div()
                        .absolute()
                        .left(px(slot.x))
                        .top(px(slot.y))
                        .w(px(slot.w))
                        .h(px(slot.h))
                        .bg(gpui::white())
                        .shadow_md()
                        .cursor(CursorStyle::IBeam)
                        // The page bitmap fills the whole box (a border would squeeze it and
                        // resample the text); the hairline is drawn just outside instead.
                        .when_some(image.or(fallback), |d, image| {
                            d.child(bitmap_img(image, slot.w, slot.h))
                        })
                        .children(
                            // Sharp tiles over the (lower resolution) whole-page backdrop. Each
                            // is placed by its own scale, so tiles stay aligned while zooming.
                            doc.tiles
                                .iter()
                                .filter(|(job, _)| job.page == page && job.rotation == rotation)
                                .map(|(job, tile)| {
                                    let per_device = zoom / (job.scale_micro as f32 / 1_000_000.0);
                                    let (w, h) = (tile.size.width.0 as f32, tile.size.height.0 as f32);
                                    bitmap_img(tile.paint(), w * per_device, h * per_device)
                                        .absolute()
                                        .left(px(job.tile.0 as f32 * TILE as f32 * per_device))
                                        .top(px(job.tile.1 as f32 * TILE as f32 * per_device))
                                }),
                        )
                        .when(!quads.is_empty(), |d| d.child(overlay_canvas(quads)))
                        .child(
                            div()
                                .absolute()
                                .top(px(-1.))
                                .left(px(-1.))
                                .w(px(slot.w + 2.))
                                .h(px(slot.h + 2.))
                                .border_1()
                                .border_color(t.page_border),
                        )
                        .into_any_element(),
                );
            }
        }

        let layout_w = self.layout.width;
        let layout_h = self.layout.height;
        div()
            .id("canvas")
            .key_context("Viewer")
            .track_focus(&self.focus)
            .size_full()
            .relative()
            .bg(t.content_bg)
            .drag_over::<ExternalPaths>({
                let accent = t.accent;
                move |s, _, _, _| s.border_2().border_color(accent).bg(accent.opacity(0.08))
            })
            .child(
                div()
                    .id("canvas-scroll")
                    .size_full()
                    .overflow_scroll()
                    .track_scroll(&self.scroll)
                    .child(
                        div()
                            .id("canvas-content")
                            .relative()
                            .w(px(layout_w))
                            .h(px(layout_h))
                            .on_scroll(cx.listener(|this, event: &ScrollEvent, window, cx| {
                                if event.modifiers.secondary(cx) || event.modifiers.control {
                                    let amount = match event.delta {
                                        ScrollDelta::Lines(p) => p.y * 0.12,
                                        ScrollDelta::Pixels(p) => p.y.as_f32() / 300.0,
                                    };
                                    let (ox, oy) = this.content_origin();
                                    let anchor = point(event.position.x - px(ox), event.position.y - px(oy));
                                    this.zoom_by(amount.exp(), Some(anchor), window, cx);
                                } else {
                                    this.on_canvas_scroll(event, window, cx);
                                }
                                // We scroll ourselves; keep the container from jumping.
                                cx.stop_propagation();
                            }))
                            .on_pinch(cx.listener(|this, event: &PinchEvent, window, cx| {
                                let (ox, oy) = this.content_origin();
                                let anchor = point(event.position.x - px(ox), event.position.y - px(oy));
                                this.zoom_by(1.0 + event.delta, Some(anchor), window, cx);
                            }))
                            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_canvas_mouse_down))
                            .on_mouse_move(cx.listener(Self::on_canvas_mouse_move))
                            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_canvas_mouse_up))
                            .children(pages),
                    ),
            )
            .when(self.display == DisplayMode::SinglePage && self.page_count() > 1, |d| {
                d.child(self.render_page_stepper(t, cx))
            })
            .children(self.render_scrollbar(Axis::Vertical, viewport, t, cx))
            .children(self.render_scrollbar(Axis::Horizontal, viewport, t, cx))
            .children(self.render_zoom_hud(t))
            .into_any_element()
    }

    /// Thin overlay scrollbar that widens on hover, Windows 11 style.
    fn render_scrollbar(&self, axis: Axis, viewport: (f32, f32), t: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (sx, sy) = self.scroll_pos();
        let both = self.layout.height > viewport.1 + 1.0 && self.layout.width > viewport.0 + 1.0;
        let (content, view, pos) = match axis {
            Axis::Vertical => (self.layout.height, viewport.1, sy),
            Axis::Horizontal => (self.layout.width, viewport.0, sx),
        };
        if content <= view + 1.0 {
            return None;
        }
        let inset = if both { 14.0 } else { 4.0 };
        let track = view - inset - 2.0;
        let thumb = (view / content * track).max(36.0).min(track);
        let max_pos = content - view;
        let thumb_pos = (pos / max_pos).clamp(0.0, 1.0) * (track - thumb);
        let ratio = max_pos / (track - thumb).max(1.0);
        let dragging = self.scrollbar_drag.is_some_and(|d| d.axis == axis);
        let group = match axis {
            Axis::Vertical => "vscroll",
            Axis::Horizontal => "hscroll",
        };
        let thumb_el = div()
            .absolute()
            .rounded_full()
            .bg(t.scrollbar)
            .group_hover(group, |s| s.bg(t.scrollbar_hover))
            .when(dragging, |d| d.bg(t.scrollbar_hover));
        let thumb_el = match axis {
            Axis::Vertical => thumb_el
                .top(px(thumb_pos))
                .h(px(thumb))
                .right(px(2.))
                .w(px(if dragging { 7. } else { 3. }))
                .group_hover(group, |s| s.w(px(7.))),
            Axis::Horizontal => thumb_el
                .left(px(thumb_pos))
                .w(px(thumb))
                .bottom(px(2.))
                .h(px(if dragging { 7. } else { 3. }))
                .group_hover(group, |s| s.h(px(7.))),
        };
        let bar = div()
            .id(group)
            .group(group)
            .absolute()
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    let viewport = this.viewport(window);
                    let (ox, oy) = this.content_origin();
                    let (sx, sy) = this.scroll_pos();
                    let (mouse, along) = match axis {
                        Axis::Vertical => (event.position.y.as_f32(), event.position.y.as_f32() - oy - 2.0),
                        Axis::Horizontal => (event.position.x.as_f32(), event.position.x.as_f32() - ox - 2.0),
                    };
                    // Clicking the track outside the thumb jumps there, then drags as usual.
                    let mut start_pos = match axis {
                        Axis::Vertical => sy,
                        Axis::Horizontal => sx,
                    };
                    if along < thumb_pos || along > thumb_pos + thumb {
                        start_pos = ((along - thumb / 2.0) * ratio).clamp(0.0, max_pos);
                        match axis {
                            Axis::Vertical => this.set_scroll_pos(sx, start_pos, viewport, &this.layout),
                            Axis::Horizontal => this.set_scroll_pos(start_pos, sy, viewport, &this.layout),
                        }
                    }
                    this.smooth.cancel();
                    this.scrollbar_drag = Some(ScrollbarDrag {
                        axis,
                        start_mouse: mouse,
                        start_pos,
                        ratio,
                    });
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .child(thumb_el);
        let bar = match axis {
            Axis::Vertical => bar.top(px(2.)).right_0().w(px(12.)).h(px(track)),
            Axis::Horizontal => bar.left(px(2.)).bottom_0().h(px(12.)).w(px(track)),
        };
        Some(bar.into_any_element())
    }

    fn on_scrollbar_drag(&mut self, event: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(drag) = self.scrollbar_drag else { return false };
        if !event.dragging() {
            self.scrollbar_drag = None;
            cx.notify();
            return true;
        }
        let viewport = self.viewport(window);
        let (sx, sy) = self.scroll_pos();
        match drag.axis {
            Axis::Vertical => {
                let pos = drag.start_pos + (event.position.y.as_f32() - drag.start_mouse) * drag.ratio;
                self.set_scroll_pos(sx, pos, viewport, &self.layout);
            }
            Axis::Horizontal => {
                let pos = drag.start_pos + (event.position.x.as_f32() - drag.start_mouse) * drag.ratio;
                self.set_scroll_pos(pos, sy, viewport, &self.layout);
            }
        }
        cx.notify();
        true
    }

    fn render_zoom_hud(&self, t: &Theme) -> Option<AnyElement> {
        if self.zoom_hud_until.is_none_or(|until| Instant::now() >= until) {
            return None;
        }
        Some(
            div()
                .absolute()
                .bottom(px(28.))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .px(px(14.))
                        .h(px(30.))
                        .flex()
                        .items_center()
                        .rounded(px(15.))
                        .bg(t.popover_bg)
                        .border_1()
                        .border_color(t.popover_border)
                        .shadow_md()
                        .text_size(px(13.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(t.text)
                        .child(format!("{:.0}%", self.layout.zoom * 100.0)),
                )
                .into_any_element(),
        )
    }

    fn render_page_stepper(&self, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .absolute()
            .bottom(px(18.))
            .left_0()
            .right_0()
            .flex()
            .justify_center()
            .child(
                div()
                    .flex()
                    .items_center()
                    .h(px(30.))
                    .rounded(px(15.))
                    .bg(t.popover_bg)
                    .border_1()
                    .border_color(t.popover_border)
                    .shadow_md()
                    .child(
                        icon_button("prev-page", "icons/chevron-left.svg", t, false)
                            .rounded(px(15.))
                            .on_click(cx.listener(|this, _, _, cx| this.step_page(false, cx))),
                    )
                    .child(
                        div()
                            .px(px(6.))
                            .text_size(px(12.))
                            .text_color(t.text_secondary)
                            .child(format!("{} / {}", self.current_page + 1, self.page_count())),
                    )
                    .child(
                        icon_button("next-page", "icons/chevron-right.svg", t, false)
                            .rounded(px(15.))
                            .on_click(cx.listener(|this, _, _, cx| this.step_page(true, cx))),
                    ),
            )
    }

    fn render_empty_state(&self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("empty")
            .key_context("Viewer")
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(14.))
            .bg(t.content_bg)
            .child(icon("icons/doc.svg", 64., t.text_tertiary))
            .child(
                div()
                    .text_size(px(15.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(t.text_secondary)
                    .child(if self.loading { "Opening…" } else { "No Document" }),
            )
            .when_some(self.error.clone(), |d, err| {
                d.child(
                    div()
                        .max_w(px(420.))
                        .text_center()
                        .text_size(px(12.))
                        .text_color(rgba_hsla(0xFF453A, 1.0))
                        .child(err),
                )
            })
            .child(
                div()
                    .id("open-button")
                    .px(px(14.))
                    .h(px(28.))
                    .flex()
                    .items_center()
                    .rounded(px(7.))
                    .bg(t.accent)
                    .text_color(t.accent_text)
                    .text_size(px(13.))
                    .cursor_pointer()
                    .hover(|s| s.opacity(0.9))
                    .active(|s| s.opacity(0.8))
                    .child("Open…")
                    .on_click(cx.listener(|this, _, window, cx| this.prompt_open(window, cx))),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(t.text_tertiary)
                    .child("or drop a PDF onto this window"),
            )
            .into_any_element()
    }

    fn render_popover(&self, t: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let popover = self.popover?;
        let menu = match popover {
            Popover::ViewMenu => {
                let sidebar = self.sidebar_visible;
                let mode = self.sidebar_mode;
                let display = self.display;
                menu_panel(t)
                    .left(px(TOOLBAR_LEFT))
                    .child(menu_item("m-hide", "Hide Sidebar", !sidebar, Some("⌥⌘1"), t).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.sidebar_visible = false;
                            this.popover = None;
                            cx.notify();
                        }),
                    ))
                    .child(
                        menu_item("m-thumbs", "Thumbnails", sidebar && mode == SidebarMode::Thumbnails, Some("⌥⌘2"), t)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_sidebar(SidebarMode::Thumbnails, cx)
                            })),
                    )
                    .child(
                        menu_item("m-toc", "Table of Contents", sidebar && mode == SidebarMode::Contents, Some("⌥⌘3"), t)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_sidebar(SidebarMode::Contents, cx)
                            })),
                    )
                    .child(menu_separator(t))
                    .child(
                        menu_item("m-cont", "Continuous Scroll", display == DisplayMode::Continuous, None, t)
                            .on_click(cx.listener(|this, _, _, cx| this.set_display(DisplayMode::Continuous, cx))),
                    )
                    .child(
                        menu_item("m-single", "Single Page", display == DisplayMode::SinglePage, None, t)
                            .on_click(cx.listener(|this, _, _, cx| this.set_display(DisplayMode::SinglePage, cx))),
                    )
                    .child(
                        menu_item("m-two", "Two Pages", display == DisplayMode::TwoPages, None, t)
                            .on_click(cx.listener(|this, _, _, cx| this.set_display(DisplayMode::TwoPages, cx))),
                    )
            }
            Popover::Share => menu_panel(t)
                .right(px(250.))
                .child(menu_item("s-copy", "Copy File Path", false, None, t).on_click(cx.listener(
                    |this, _, _, cx| {
                        if let Some(doc) = &this.doc {
                            cx.write_to_clipboard(ClipboardItem::new_string(
                                doc.info.path.display().to_string(),
                            ));
                        }
                        this.popover = None;
                        cx.notify();
                    },
                )))
                .child(
                    menu_item(
                        "s-reveal",
                        if cfg!(target_os = "macos") { "Show in Finder" } else { "Show in Folder" },
                        false,
                        None,
                        t,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(doc) = &this.doc {
                            cx.reveal_path(&doc.info.path);
                        }
                        this.popover = None;
                        cx.notify();
                    })),
                )
                .child(menu_item("s-open", "Open in Default App", false, None, t).on_click(cx.listener(
                    |this, _, _, cx| {
                        if let Some(doc) = &this.doc {
                            cx.open_with_system(&doc.info.path);
                        }
                        this.popover = None;
                        cx.notify();
                    },
                ))),
        };
        Some(
            div()
                .id("popover-backdrop")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .occlude()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, _, cx| {
                        this.popover = None;
                        cx.notify();
                    }),
                )
                .child(menu.on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation()))
                .into_any_element(),
        )
    }

    fn show_sidebar(&mut self, mode: SidebarMode, cx: &mut Context<Self>) {
        self.sidebar_visible = true;
        self.sidebar_mode = mode;
        self.popover = None;
        cx.notify();
    }

    fn render_inspector(&self, t: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.inspector {
            return None;
        }
        let doc = self.doc.as_ref()?;
        let info = &doc.info;
        let (w, h) = self.rotated_size(self.current_page);
        let mut rows: Vec<(&str, String)> = vec![
            ("Kind", "PDF Document".into()),
            ("Size", human_size(info.file_size)),
            ("Version", info.version.clone()),
            ("Pages", info.page_sizes.len().to_string()),
            (
                "Page Size",
                format!("{:.2} × {:.2} in  ({:.0} × {:.0} mm)", w / 72.0, h / 72.0, w / 72.0 * 25.4, h / 72.0 * 25.4),
            ),
        ];
        for (label, value) in [
            ("Title", &info.title),
            ("Author", &info.author),
            ("Subject", &info.subject),
            ("Creator", &info.creator),
            ("Producer", &info.producer),
            ("Created", &info.created),
            ("Modified", &info.modified),
        ] {
            if let Some(value) = value {
                rows.push((label, value.clone()));
            }
        }
        rows.push(("Location", info.path.parent().map(|p| p.display().to_string()).unwrap_or_default()));

        Some(
            div()
                .id("inspector")
                .absolute()
                .top(px(TOOLBAR_H + 10.))
                .right(px(12.))
                .w(px(300.))
                .occlude()
                .flex()
                .flex_col()
                .rounded(px(12.))
                .bg(t.popover_bg)
                .border_1()
                .border_color(t.popover_border)
                .shadow_lg()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .h(px(36.))
                        .px(px(12.))
                        .border_b_1()
                        .border_color(t.separator)
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(13.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(format!("{} Info", info.file_name)),
                        )
                        .child(
                            div()
                                .id("close-inspector")
                                .p(px(3.))
                                .rounded(px(5.))
                                .hover(|s| s.bg(t.hover))
                                .child(icon("icons/xmark.svg", 12., t.text_secondary))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.inspector = false;
                                    cx.notify();
                                })),
                        ),
                )
                .child(
                    div().flex().flex_col().gap(px(6.)).p(px(12.)).children(rows.into_iter().map(|(label, value)| {
                        div()
                            .flex()
                            .gap(px(10.))
                            .text_size(px(12.))
                            .child(
                                div()
                                    .w(px(72.))
                                    .flex_none()
                                    .text_right()
                                    .text_color(t.text_secondary)
                                    .child(format!("{label}:")),
                            )
                            .child(div().flex_1().min_w_0().text_color(t.text).child(value))
                    })),
                )
                .into_any_element(),
        )
    }

    fn render_goto_sheet(&self, t: &Theme, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let text = self.goto.clone()?;
        let focused = self.goto_focus.is_focused(window);
        Some(
            div()
                .absolute()
                .top(px(TOOLBAR_H))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .id("goto-sheet")
                        .relative()
                        .key_context("SearchField")
                        .track_focus(&self.goto_focus)
                        .occlude()
                        .w(px(280.))
                        .p(px(16.))
                        .flex()
                        .flex_col()
                        .gap(px(12.))
                        .rounded_b(px(12.))
                        .bg(t.popover_bg)
                        .border_1()
                        .border_color(t.popover_border)
                        .shadow_lg()
                        .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                            let mut text = this.goto.clone().unwrap_or_default();
                            match edit_text(&mut text, event, cx) {
                                TextEdit::Changed => {
                                    text.retain(|c| c.is_ascii_digit());
                                    this.goto = Some(text);
                                    cx.notify();
                                }
                                TextEdit::Submit(_) => this.submit_goto(window, cx),
                                TextEdit::Cancel => {
                                    this.goto = None;
                                    window.focus(&this.focus, cx);
                                    cx.notify();
                                }
                                TextEdit::None => return,
                            }
                            cx.stop_propagation();
                        }))
                        .child(input_target(self.goto_focus.clone(), cx))
                        .child(div().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).child("Go to Page"))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .text_size(px(13.))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .w(px(80.))
                                        .h(px(24.))
                                        .px(px(6.))
                                        .rounded(px(5.))
                                        .bg(t.field_bg)
                                        .border_1()
                                        .border_color(if focused { t.accent.opacity(0.7) } else { t.control_border })
                                        .child(text)
                                        .when(focused, |d| d.child(div().w(px(1.5)).h(px(14.)).ml(px(1.)).bg(t.accent))),
                                )
                                .child(div().text_color(t.text_secondary).child(format!("of {}", self.page_count()))),
                        )
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap(px(8.))
                                .child(sheet_button("goto-cancel", "Cancel", false, t).on_click(cx.listener(
                                    |this, _, window, cx| {
                                        this.goto = None;
                                        window.focus(&this.focus, cx);
                                        cx.notify();
                                    },
                                )))
                                .child(
                                    sheet_button("goto-ok", "Go", true, t)
                                        .on_click(cx.listener(|this, _, window, cx| this.submit_goto(window, cx))),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    fn submit_goto(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(n) = self.goto.take().and_then(|s| s.parse::<usize>().ok()) {
            self.go_to_page(n.saturating_sub(1), cx);
        }
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn prompt_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Open".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = paths.await {
                if let Some(path) = paths.into_iter().next() {
                    this.update_in(cx, |this, window, cx| this.open_path(path, window, cx)).ok();
                }
            }
        })
        .detach();
    }
}

impl Render for VellumApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = Theme::for_appearance(window.appearance());
        let viewport = self.viewport(window);
        // Bitmaps painted last frame are in the GPU atlas now. Draw once more after releasing
        // them so GPUI drops last frame's elements, which still hold the pixels.
        if self.doc.as_mut().is_some_and(|doc| doc.release_uploaded()) {
            window.request_animation_frame();
        }

        // Rebuild the layout only when its inputs change; keep the user's place when
        // zoom or arrangement changes.
        let key = self.layout_key(viewport, window.scale_factor());
        if self.layout_key != Some(key) || self.pending_anchor.is_some() {
            let layout = self.compute_layout(viewport, window.scale_factor());
            let changed = (layout.zoom - self.layout.zoom).abs() > 1e-5
                || layout.rows.len() != self.layout.rows.len()
                || layout.single != self.layout.single
                || (layout.width - self.layout.width).abs() > 0.5
                || self.pending_anchor.is_some();
            if changed && self.doc.is_some() {
                let anchor = self.pending_anchor.take().or_else(|| {
                    (!self.layout.rows.is_empty()).then(|| self.anchor_at(point(px(0.), px(0.)))).flatten()
                });
                if let Some(anchor) = anchor {
                    self.apply_anchor(anchor, &layout, viewport);
                }
            }
            if self.layout.zoom > 0.0 && (layout.zoom - self.layout.zoom).abs() > 1e-5 {
                // Re-render sharply once the zoom has held still for a moment.
                self.zoom_changed_at = Some(Instant::now());
                self._settle_task = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(ZOOM_SETTLE + Duration::from_millis(10)).await;
                    this.update(cx, |this, cx| {
                        this.renders_dirty = true;
                        cx.notify();
                    })
                    .ok();
                }));
            }
            self.layout = layout;
            self.layout_key = Some(key);
            self.renders_dirty = true;
        }

        // Advance smooth scrolling and keep frames coming while it runs.
        let max = self.max_scroll(viewport);
        let canvas_moving = self.smooth.tick(&self.scroll, max);
        let thumbs_moving = self.thumb_smooth.tick(&self.sidebar_scroll, (0.0, self.thumb_max));
        if canvas_moving || thumbs_moving {
            window.request_animation_frame();
        }

        // Track the current page from the scroll position.
        if self.doc.is_some() && self.display != DisplayMode::SinglePage {
            let (_, sy) = self.scroll_pos();
            let probe = sy + viewport.1 * 0.35;
            if let Some(row) = self.layout.row_at(probe) {
                let slots = self.layout.row_slots(row);
                let first = slots[0].page;
                let in_row = slots.iter().any(|s| s.page == self.current_page);
                if !in_row {
                    self.current_page = first;
                }
            }
        }
        if self.doc.is_some() {
            self.schedule_renders(viewport, window, cx);
        }

        let toolbar = self.render_toolbar(&t, window, cx).into_any_element();
        let sidebar = self.sidebar_shown().then(|| self.render_sidebar(&t, window, cx));
        let canvas = self.render_canvas(&t, viewport, cx);
        let popover = self.render_popover(&t, cx);
        let inspector = self.render_inspector(&t, cx);
        let goto = self.render_goto_sheet(&t, window, cx);
        // Bitmaps shown for the first time are released next frame; make sure there is one.
        if self.doc.as_ref().is_some_and(|doc| doc.has_unreleased_shown()) {
            window.request_animation_frame();
        }

        div()
            .id("root")
            .key_context("VellumApp")
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.window_bg)
            .font_family(UI_FONT)
            .text_color(t.text)
            .text_size(px(13.))
            .when(self.resizing_sidebar, |d| d.cursor(CursorStyle::ResizeLeftRight))
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                if this.on_scrollbar_drag(event, window, cx) {
                    return;
                }
                if this.resizing_sidebar {
                    if event.dragging() {
                        this.sidebar_width = event.position.x.as_f32().clamp(SIDEBAR_MIN, SIDEBAR_MAX);
                        cx.notify();
                    } else {
                        this.resizing_sidebar = false;
                    }
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| {
                    if this.resizing_sidebar || this.scrollbar_drag.is_some() {
                        this.resizing_sidebar = false;
                        this.scrollbar_drag = None;
                        cx.notify();
                    }
                }),
            )
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                if let Some(path) = paths.paths().first() {
                    this.open_path(path.clone(), window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &Open, window, cx| this.prompt_open(window, cx)))
            .on_action(cx.listener(|_, _: &CloseWindow, window, _| window.remove_window()))
            .on_action(cx.listener(|this, _: &ZoomIn, window, cx| this.zoom_by(1.25, None, window, cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, window, cx| this.zoom_by(0.8, None, window, cx)))
            .on_action(cx.listener(|this, _: &ActualSize, window, cx| {
                this.set_zoom(ZoomMode::Custom(1.0), None, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ZoomToFit, window, cx| this.set_zoom(ZoomMode::Fit, None, window, cx)))
            .on_action(cx.listener(|this, _: &RotateLeft, _, cx| this.rotate(false, cx)))
            .on_action(cx.listener(|this, _: &RotateRight, _, cx| this.rotate(true, cx)))
            .on_action(cx.listener(|this, _: &HideSidebar, _, cx| {
                this.sidebar_visible = false;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| {
                this.sidebar_visible = !this.sidebar_visible;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ShowThumbnails, _, cx| this.show_sidebar(SidebarMode::Thumbnails, cx)))
            .on_action(cx.listener(|this, _: &ShowTableOfContents, _, cx| {
                this.show_sidebar(SidebarMode::Contents, cx)
            }))
            .on_action(cx.listener(|this, _: &ContinuousScroll, _, cx| this.set_display(DisplayMode::Continuous, cx)))
            .on_action(cx.listener(|this, _: &SinglePage, _, cx| this.set_display(DisplayMode::SinglePage, cx)))
            .on_action(cx.listener(|this, _: &TwoPages, _, cx| this.set_display(DisplayMode::TwoPages, cx)))
            .on_action(cx.listener(|this, _: &Find, window, cx| {
                if this.doc.is_some() {
                    window.focus(&this.search_focus, cx);
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &Dismiss, _, cx| {
                // Close the topmost transient UI first.
                if this.popover.take().is_none() && this.goto.take().is_none() {
                    if this.selection.take().is_none() {
                        this.inspector = false;
                    }
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &FindNext, _, cx| this.step_hit(true, cx)))
            .on_action(cx.listener(|this, _: &FindPrevious, _, cx| this.step_hit(false, cx)))
            .on_action(cx.listener(|this, _: &ToggleInspector, _, cx| {
                this.inspector = !this.inspector && this.doc.is_some();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &GoToPage, window, cx| {
                if this.doc.is_some() {
                    this.goto = Some(String::new());
                    window.focus(&this.goto_focus, cx);
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &CopyText, _, cx| {
                if let Some(text) = this.selected_text() {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
            }))
            .on_action(cx.listener(|this, _: &NextPage, _, cx| this.step_page(true, cx)))
            .on_action(cx.listener(|this, _: &PreviousPage, _, cx| this.step_page(false, cx)))
            .on_action(cx.listener(|this, _: &FirstPage, _, cx| this.go_to_page(0, cx)))
            .on_action(cx.listener(|this, _: &LastPage, _, cx| {
                let last = this.page_count().saturating_sub(1);
                this.go_to_page(last, cx)
            }))
            .on_action(cx.listener(|this, _: &ScrollDown, window, cx| this.scroll_by(48.0, window, cx)))
            .on_action(cx.listener(|this, _: &ScrollUp, window, cx| this.scroll_by(-48.0, window, cx)))
            .on_action(cx.listener(|this, _: &ScrollPageDown, window, cx| {
                let h = this.viewport(window).1;
                this.scroll_by(h - 40.0, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ScrollPageUp, window, cx| {
                let h = this.viewport(window).1;
                this.scroll_by(-(h - 40.0), window, cx)
            }))
            .child(toolbar)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .children(sidebar)
                    .child(div().flex_1().min_w_0().h_full().child(canvas)),
            )
            .children(inspector)
            .children(goto)
            .children(popover)
            .when(self.scrollbar_drag.is_some(), |d| d.child(drag_capture(cx)))
    }
}

// ------------------------------------------------------------------ helpers

fn rgba_hsla(hex: u32, alpha: f32) -> Hsla {
    let color: Hsla = gpui::rgb(hex).into();
    color.opacity(alpha)
}

fn icon(path: &'static str, size: f32, color: Hsla) -> impl IntoElement {
    svg().path(path).size(px(size)).flex_none().text_color(color)
}

fn icon_button(id: &'static str, path: &'static str, t: &Theme, active: bool) -> Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .h(px(30.))
        .w(px(34.))
        .rounded(px(8.))
        .occlude()
        .hover(|s| s.bg(t.hover))
        .active(|s| s.bg(t.pressed))
        .when(active, |s| s.bg(t.pressed))
        .child(icon(path, 17., if active { t.text } else { t.icon }))
}

fn segmented(t: &Theme) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .h(px(30.))
        .rounded(px(8.))
        .overflow_hidden()
        .bg(t.control_bg)
}

/// Windows-style minimize / maximize / close buttons. On Windows the platform
/// handles them through hit-testing, which also enables Snap Layouts.
fn caption_buttons(t: &Theme, window: &Window) -> impl IntoElement {
    let active = window.is_window_active();
    let glyph_color = if active { t.text } else { t.text_tertiary };
    let button = |id: &'static str, glyph: &'static str, area: WindowControlArea| {
        let close = area == WindowControlArea::Close;
        let close_red: Hsla = gpui::rgb(0xC42B1C).into();
        div()
            .id(id)
            .flex()
            .items_center()
            .justify_center()
            .w(px(46.))
            .h_full()
            .occlude()
            .text_size(px(10.))
            .text_color(glyph_color)
            .window_control_area(area)
            .when(close, |d| {
                d.hover(move |s| s.bg(close_red).text_color(gpui::white()))
                    .active(move |s| s.bg(close_red.opacity(0.85)).text_color(gpui::white()))
            })
            .when(!close, |d| d.hover(|s| s.bg(t.hover)).active(|s| s.bg(t.pressed)))
            .when(cfg!(not(target_os = "windows")), |d| {
                d.on_click(move |_, window, _| match area {
                    WindowControlArea::Min => window.minimize_window(),
                    WindowControlArea::Max => window.zoom_window(),
                    _ => window.remove_window(),
                })
            })
            .child(glyph)
    };
    div()
        .flex()
        .self_start()
        .h(px(32.))
        .ml(px(10.))
        .font_family("Segoe Fluent Icons")
        .child(button("caption-min", "\u{e921}", WindowControlArea::Min))
        .child(button(
            "caption-max",
            if window.is_maximized() { "\u{e923}" } else { "\u{e922}" },
            WindowControlArea::Max,
        ))
        .child(button("caption-close", "\u{e8bb}", WindowControlArea::Close))
}

/// Paints a bitmap's image into a `w` x `h` box. The aspect ratio is explicit because GPUI
/// otherwise takes it from the pixels, and a released bitmap's stand-in is 1x1.
fn bitmap_img(image: Arc<RenderImage>, w: f32, h: f32) -> gpui::Img {
    img(image)
        .w(px(w))
        .h(px(h))
        .aspect_ratio(w / h)
        .object_fit(ObjectFit::Fill)
}

fn menu_panel(t: &Theme) -> gpui::Div {
    div()
        .absolute()
        .top(px(TOOLBAR_H - 6.))
        .min_w(px(220.))
        .p(px(5.))
        .flex()
        .flex_col()
        .rounded(px(10.))
        .bg(t.popover_bg)
        .border_1()
        .border_color(t.popover_border)
        .shadow_lg()
}

fn menu_item(id: &'static str, label: &'static str, checked: bool, shortcut: Option<&'static str>, t: &Theme) -> Stateful<gpui::Div> {
    let shortcut = shortcut.map(|s| {
        if cfg!(target_os = "macos") {
            s.to_string()
        } else {
            s.replace("⌥⌘", "Ctrl+Alt+").replace('⌥', "Alt+").replace('⌘', "Ctrl+")
        }
    });
    div()
        .id(id)
        .flex()
        .items_center()
        .h(px(28.))
        .px(px(8.))
        .rounded(px(6.))
        .text_size(px(13.))
        .text_color(t.text)
        .cursor_pointer()
        .hover(|s| s.bg(t.selection))
        .child(div().w(px(18.)).text_color(t.accent).child(if checked { "✓" } else { "" }))
        .child(div().flex_1().child(label))
        .when_some(shortcut, |d, s| d.child(div().pl(px(16.)).text_color(t.text_tertiary).child(s)))
}

fn menu_separator(t: &Theme) -> impl IntoElement {
    div().h(px(1.)).mx(px(8.)).my(px(4.)).bg(t.separator)
}

fn sheet_button(id: &'static str, label: &'static str, primary: bool, t: &Theme) -> Stateful<gpui::Div> {
    div()
        .id(id)
        .px(px(14.))
        .h(px(24.))
        .flex()
        .items_center()
        .rounded(px(6.))
        .text_size(px(13.))
        .when(primary, |d| d.bg(t.accent).text_color(t.accent_text))
        .when(!primary, |d| d.bg(t.control_bg).border_1().border_color(t.control_border).text_color(t.text))
        .hover(|s| s.opacity(0.9))
        .child(label)
}

fn sidebar_placeholder(t: &Theme, text: &'static str) -> AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(12.))
        .text_color(t.text_tertiary)
        .child(text)
        .into_any_element()
}

/// Paints rects (in the page's display coordinates) as rounded quads.
fn overlay_canvas(quads: Vec<(kurbo::Rect, Hsla)>) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            for (r, color) in &quads {
                let quad = Bounds::new(
                    point(bounds.origin.x + px(r.x0 as f32), bounds.origin.y + px(r.y0 as f32)),
                    gpui::size(px(r.width() as f32), px(r.height() as f32)),
                );
                window.paint_quad(fill(quad, *color).corner_radii(px(2.)));
            }
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// Maps a rect in unrotated page points into the displayed (rotated, zoomed) page box.
fn page_to_display(rect: kurbo::Rect, rotation: u16, w: f32, h: f32, zoom: f32) -> kurbo::Rect {
    let transform = Affine::scale(zoom as f64) * document::rotation_transform(rotation, w as f64, h as f64);
    transform.transform_rect_bbox(rect)
}

fn human_size(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= 1_000_000.0 {
        format!("{:.1} MB ({} bytes)", b / 1_000_000.0, bytes)
    } else if b >= 1_000.0 {
        format!("{:.0} KB ({} bytes)", b / 1_000.0, bytes)
    } else {
        format!("{bytes} bytes")
    }
}

/// Picks the character closest to `p`, preferring characters on the same line.
fn nearest_char(text: &[TextChar], p: kurbo::Point) -> Option<usize> {
    let mut best: Option<(f64, usize)> = None;
    for (ix, c) in text.iter().enumerate() {
        let Some(r) = c.rect else { continue };
        let dx = if p.x < r.x0 { r.x0 - p.x } else if p.x > r.x1 { p.x - r.x1 } else { 0.0 };
        let dy = if p.y < r.y0 { r.y0 - p.y } else if p.y > r.y1 { p.y - r.y1 } else { 0.0 };
        let score = dy * 8.0 + dx;
        if best.is_none_or(|(s, _)| score < s) {
            best = Some((score, ix));
        }
    }
    best.map(|(_, ix)| ix)
}

/// Unions character boxes into one rect per line.
fn merge_rects(chars: &[TextChar]) -> Vec<kurbo::Rect> {
    let mut out: Vec<kurbo::Rect> = Vec::new();
    for r in chars.iter().filter_map(|c| c.rect) {
        if let Some(last) = out.last_mut() {
            let line_h = last.height().max(r.height());
            if (last.center().y - r.center().y).abs() < line_h * 0.5 && r.x0 >= last.x0 - line_h {
                *last = last.union(r);
                continue;
            }
        }
        out.push(r);
    }
    out
}

fn fold_query(query: &str) -> Vec<char> {
    query
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .map(fold_char)
        .collect()
}

/// Case-folds a page's text 1:1 with its characters (line breaks become spaces).
fn fold_text(text: &[TextChar]) -> Arc<[char]> {
    text.iter()
        .map(|c| if c.ch == '\n' { ' ' } else { fold_char(c.ch) })
        .collect()
}

/// Appends non-overlapping matches of `needle` in a folded page to `out`.
fn scan_page(page: usize, hay: &[char], needle: &[char], out: &mut Vec<SearchHit>) {
    let Some(&first) = needle.first() else { return };
    let n = needle.len();
    let mut i = 0;
    while i + n <= hay.len() {
        if hay[i] == first && hay[i..i + n] == *needle {
            out.push(SearchHit {
                page,
                start: i as u32,
                len: n as u32,
            });
            i += n;
        } else {
            i += 1;
        }
    }
}

/// A short excerpt around a match and the byte range of the match within it.
fn hit_snippet(text: &[TextChar], hit: &SearchHit) -> (SharedString, Range<usize>) {
    let range = hit.range();
    let ctx_start = range.start.saturating_sub(24);
    let ctx_end = (range.end + 48).min(text.len());
    let mut snippet = String::new();
    if ctx_start > 0 {
        snippet.push('…');
    }
    let mut hl_start = 0;
    let mut hl_end = 0;
    for (j, c) in text[ctx_start..ctx_end].iter().enumerate() {
        let j = j + ctx_start;
        if j == range.start {
            hl_start = snippet.len();
        }
        snippet.push(if c.ch == '\n' { ' ' } else { c.ch });
        if j + 1 == range.end {
            hl_end = snippet.len();
        }
    }
    if ctx_end < text.len() {
        snippet.push('…');
    }
    (snippet.into(), hl_start..hl_end)
}

fn fold_char(c: char) -> char {
    match c {
        '\u{2018}' | '\u{2019}' => '\'',
        '\u{201C}' | '\u{201D}' => '"',
        '\u{FB01}' => 'f',
        c => c.to_lowercase().next().unwrap_or(c),
    }
}

enum TextEdit {
    None,
    Changed,
    Submit(bool),
    Cancel,
}

/// Editing keys for single-line fields; typed characters arrive via `EntityInputHandler`.
fn edit_text(text: &mut String, event: &KeyDownEvent, cx: &mut App) -> TextEdit {
    let ks = &event.keystroke;
    let m = ks.modifiers;
    match ks.key.as_str() {
        "enter" => return TextEdit::Submit(m.shift),
        "escape" => return TextEdit::Cancel,
        "backspace" => {
            if m.secondary(cx) || m.alt {
                let trimmed = text.trim_end().len();
                let cut = text[..trimmed].rfind(' ').map_or(0, |i| i + 1);
                text.truncate(cut);
            } else {
                text.pop();
            }
            return TextEdit::Changed;
        }
        "v" if m.secondary(cx) => {
            if let Some(clip) = cx.read_from_clipboard().and_then(|c| c.text()) {
                text.push_str(&clip.replace(['\n', '\r'], " "));
                return TextEdit::Changed;
            }
            return TextEdit::None;
        }
        _ => {}
    }
    TextEdit::None
}

/// An invisible element that receives platform text input (typing, IME) for `focus`.
fn input_target(focus: FocusHandle, cx: &mut Context<VellumApp>) -> impl IntoElement {
    let entity = cx.entity();
    canvas(
        |_, _, _| {},
        move |bounds, _, window, cx| {
            window.handle_input(&focus, ElementInputHandler::new(bounds, entity), cx);
        },
    )
    .absolute()
    .size_full()
}

impl VellumApp {
    fn focused_field(&self, window: &Window) -> Option<String> {
        if self.search_focus.is_focused(window) {
            Some(self.query.clone())
        } else if self.goto_focus.is_focused(window) {
            Some(self.goto.clone().unwrap_or_default())
        } else {
            None
        }
    }
}

impl EntityInputHandler for VellumApp {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        window: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let text = self.focused_field(window)?;
        let utf16: Vec<u16> = text.encode_utf16().collect();
        let range = range.start.min(utf16.len())..range.end.min(utf16.len());
        *adjusted_range = Some(range.clone());
        Some(String::from_utf16_lossy(&utf16[range]))
    }

    fn selected_text_range(&mut self, _: bool, window: &mut Window, _: &mut Context<Self>) -> Option<UTF16Selection> {
        let len = self.focused_field(window)?.encode_utf16().count();
        Some(UTF16Selection {
            range: len..len,
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        None
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {}

    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text: String = text.chars().filter(|c| !c.is_control()).collect();
        if text.is_empty() {
            return;
        }
        if self.search_focus.is_focused(window) {
            let query = format!("{}{}", self.query, text);
            self.set_query(query, cx);
        } else if self.goto_focus.is_focused(window) {
            let mut value = self.goto.clone().unwrap_or_default();
            value.extend(text.chars().filter(|c| c.is_ascii_digit()));
            self.goto = Some(value);
            cx.notify();
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        _: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace_text_in_range(range, new_text, window, cx);
    }

    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        Some(element_bounds)
    }

    fn character_index_for_point(&mut self, _: Point<Pixels>, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        None
    }
}

/// Picks the macOS or Windows/Linux spelling of a shortcut.
fn shortcut(mac: &'static str, other: &'static str) -> &'static str {
    if cfg!(target_os = "macos") { mac } else { other }
}

struct Tooltip {
    title: SharedString,
    shortcut: Option<SharedString>,
    theme: Theme,
}

impl Render for Tooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        div().pl(px(6.)).pt(px(6.)).child(
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(9.))
                .py(px(5.))
                .rounded(px(6.))
                .bg(t.popover_bg)
                .border_1()
                .border_color(t.popover_border)
                .shadow_md()
                .font_family(UI_FONT)
                .text_size(px(12.))
                .text_color(t.text)
                .child(self.title.clone())
                .when_some(self.shortcut.clone(), |d, s| d.child(div().text_color(t.text_tertiary).child(s))),
        )
    }
}

fn tip(
    title: &'static str,
    shortcut: Option<&'static str>,
    theme: Theme,
) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView + 'static {
    move |_, cx| {
        cx.new(|_| Tooltip {
            title: title.into(),
            shortcut: shortcut.map(Into::into),
            theme,
        })
        .into()
    }
}

fn small_icon_button(id: &'static str, path: &'static str, t: &Theme) -> Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .size(px(24.))
        .rounded(px(6.))
        .cursor_pointer()
        .hover(|s| s.bg(t.hover))
        .active(|s| s.bg(t.pressed))
        .child(icon(path, 12., t.icon))
}

/// While a scrollbar is being dragged, follow the mouse anywhere in the window.
fn drag_capture(cx: &mut Context<VellumApp>) -> impl IntoElement {
    let entity = cx.entity();
    canvas(
        |_, _, _| {},
        move |_, _, window, _| {
            let on_move = entity.clone();
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                if phase == gpui::DispatchPhase::Capture {
                    on_move.update(cx, |this, cx| this.on_scrollbar_drag(event, window, cx));
                }
            });
            let on_up = entity.clone();
            window.on_mouse_event(move |_: &MouseUpEvent, phase, _, cx| {
                if phase == gpui::DispatchPhase::Capture {
                    on_up.update(cx, |this, cx| {
                        this.scrollbar_drag = None;
                        cx.notify();
                    });
                }
            });
        },
    )
    .absolute()
    .size_0()
}

/// Pixels per wheel "line": Windows reports 3 lines per notch, and Chromium
/// scrolls 100px per notch.
const WHEEL_LINE_PX: f32 = 100.0 / 3.0;

/// Converts a scroll event into (dx, dy) content pixels, positive = down/right.
fn wheel_delta(event: &ScrollEvent) -> (f32, f32) {
    let (mut dx, mut dy) = match event.delta {
        ScrollDelta::Lines(p) => (-p.x * WHEEL_LINE_PX, -p.y * WHEEL_LINE_PX),
        ScrollDelta::Pixels(p) => (-p.x.as_f32(), -p.y.as_f32()),
    };
    if event.modifiers.shift && dx == 0.0 {
        std::mem::swap(&mut dx, &mut dy);
    }
    (dx, dy)
}

/// Browser-style animated scrolling: wheel notches and key presses move a target,
/// and the view eases toward it each frame. Repeated notches accumulate, so fast
/// wheel spins speed up instead of stuttering.
#[derive(Default)]
struct SmoothScroller {
    target: Option<(f32, f32)>,
    last_tick: Option<Instant>,
    velocity: (f32, f32),
}

impl SmoothScroller {
    /// Time constant of the ease; ~95% of the distance is covered in 3 tau (~150ms).
    const TAU: f32 = 0.05;

    fn position(handle: &ScrollHandle) -> (f32, f32) {
        let offset = handle.offset();
        (-offset.x.as_f32(), -offset.y.as_f32())
    }

    fn scroll_by(&mut self, handle: &ScrollHandle, dx: f32, dy: f32, max: (f32, f32)) {
        let base = self.target.unwrap_or_else(|| Self::position(handle));
        let target = ((base.0 + dx).clamp(0.0, max.0), (base.1 + dy).clamp(0.0, max.1));
        if self.target.is_none() {
            self.last_tick = None;
        }
        self.target = Some(target);
    }

    fn jump_by(&mut self, handle: &ScrollHandle, dx: f32, dy: f32, max: (f32, f32)) {
        self.cancel();
        let (x, y) = Self::position(handle);
        let (x, y) = ((x + dx).clamp(0.0, max.0), (y + dy).clamp(0.0, max.1));
        handle.set_offset(point(px(-x), px(-y)));
    }

    fn cancel(&mut self) {
        self.target = None;
        self.last_tick = None;
        self.velocity = (0.0, 0.0);
    }

    fn velocity_y(&self) -> f32 {
        self.velocity.1
    }

    /// Moves one frame toward the target. Returns true while still animating.
    fn tick(&mut self, handle: &ScrollHandle, max: (f32, f32)) -> bool {
        let Some(target) = self.target else { return false };
        let target = (target.0.clamp(0.0, max.0), target.1.clamp(0.0, max.1));
        let now = Instant::now();
        let dt = self
            .last_tick
            .map_or(1.0 / 120.0, |last| (now - last).as_secs_f32())
            .clamp(1.0 / 480.0, 1.0 / 20.0);
        self.last_tick = Some(now);

        let (x, y) = Self::position(handle);
        let k = 1.0 - (-dt / Self::TAU).exp();
        let (nx, ny) = (x + (target.0 - x) * k, y + (target.1 - y) * k);
        if (target.0 - nx).abs() < 0.5 && (target.1 - ny).abs() < 0.5 {
            handle.set_offset(point(px(-target.0), px(-target.1)));
            self.cancel();
            return false;
        }
        self.velocity = ((nx - x) / dt, (ny - y) / dt);
        handle.set_offset(point(px(-nx), px(-ny)));
        true
    }
}

