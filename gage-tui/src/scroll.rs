//! Scrollable content view — the one construct for content taller
//! than its viewport.
//!
//! Content is a list of sections (styled lines), built by a closure
//! when the width changes. The view caches the built sections and
//! their wrapped heights per width, then each frame renders only the
//! sections intersecting the viewport into a virtual buffer and blits
//! the window (`stack::blit`). Scroll position is `usize` — there is
//! no whole-content size limit, unlike `Paragraph::scroll`, whose u16
//! offset caps that strategy at 65,535 rows. The one residual bound:
//! a single *section* renders through a `Rect`, so rows past 65,535
//! within one section cannot be displayed — sections are expected to
//! be message-scale, not document-scale.

use ratatui::Frame;
use ratatui::layout::{Margin, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, Padding, Paragraph, ScrollbarState, Widget, Wrap};

use crate::item_table::scrollbar;
use crate::stack;

#[derive(Default)]
pub(crate) struct ScrollView {
    /// Desired scroll offset in wrapped rows; clamped at render
    scroll: usize,
    cache: Option<Cache>,
}

struct Cache {
    width: u16,
    sections: Vec<Vec<Line<'static>>>,
    heights: Vec<u32>,
    total: usize,
    viewport: u16,
}

impl ScrollView {
    pub fn new() -> Self {
        Self::default()
    }

    /// New content is about to be shown: back to the top, rebuild.
    pub fn reset(&mut self) {
        self.scroll = 0;
        self.cache = None;
    }

    /// Content changed in place (e.g. a live refresh): rebuild on the
    /// next render, keeping the scroll position.
    pub fn invalidate(&mut self) {
        self.cache = None;
    }

    pub fn scroll_by(&mut self, delta: isize) {
        self.scroll = self.scroll.saturating_add_signed(delta);
        // Upward clamp happens at render, where the total is known
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll = 0;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll = usize::MAX;
    }

    /// Page size for paging keys — the last rendered viewport
    pub fn page(&self) -> usize {
        self.cache
            .as_ref()
            .map(|c| c.viewport as usize)
            .unwrap_or(0)
            .max(1)
    }

    /// Render as a centered modal (the standard detail dialog chrome).
    /// `build` produces the content sections for a given text width;
    /// it runs only when the width changes.
    pub fn render_modal(
        &mut self,
        frame: &mut Frame,
        title: String,
        build: impl FnOnce(u16) -> Vec<Vec<Line<'static>>>,
    ) {
        let area = modal_rect(frame.area());
        frame.render_widget(Clear, area);
        let block = Block::bordered()
            .title(title)
            .padding(Padding::horizontal(1));
        let body = block.inner(area);
        frame.render_widget(block, area);
        if body.width == 0 || body.height == 0 {
            return;
        }

        if !matches!(&self.cache, Some(c) if c.width == body.width) {
            let sections = build(body.width);
            let heights: Vec<u32> = sections
                .iter()
                .map(|lines| {
                    let count = Paragraph::new(lines.clone())
                        .wrap(Wrap { trim: false })
                        .line_count(body.width);
                    u32::try_from(count).unwrap_or(u32::MAX)
                })
                .collect();
            let total = heights.iter().map(|&h| h as usize).sum();
            self.cache = Some(Cache {
                width: body.width,
                sections,
                heights,
                total,
                viewport: body.height,
            });
        }
        let cache = self
            .cache
            .as_mut()
            .expect("cache is built above when stale or absent");
        cache.viewport = body.height;

        let viewport = body.height as usize;
        let max_scroll = cache.total.saturating_sub(viewport);
        self.scroll = self.scroll.min(max_scroll);
        let scroll = self.scroll;

        // Sections intersecting [scroll, scroll + viewport)
        let mut visible: Vec<(usize, usize)> = Vec::new(); // (section, top within virt)
        let mut origin: Option<usize> = None;
        let mut top = 0usize;
        for (i, &h) in cache.heights.iter().enumerate() {
            let bottom = top + h as usize;
            if bottom > scroll && top < scroll + viewport {
                let first = *origin.get_or_insert(top);
                visible.push((i, top - first));
            }
            if top >= scroll + viewport {
                break;
            }
            top = bottom;
        }
        if let Some(origin) = origin {
            let virt_height = visible
                .iter()
                .map(|&(i, off)| off + cache.heights.get(i).copied().unwrap_or(0) as usize)
                .max()
                .unwrap_or(0);
            let mut virt = ratatui::buffer::Buffer::empty(Rect {
                x: 0,
                y: 0,
                width: body.width,
                height: u16::try_from(virt_height).unwrap_or(u16::MAX),
            });
            for (i, off) in visible {
                let lines = cache.sections.get(i).cloned().unwrap_or_default();
                let height = cache.heights.get(i).copied().unwrap_or(0);
                let rect = Rect {
                    x: 0,
                    y: u16::try_from(off).unwrap_or(u16::MAX),
                    width: body.width,
                    height: u16::try_from(height).unwrap_or(u16::MAX),
                };
                Paragraph::new(lines)
                    .wrap(Wrap { trim: false })
                    .render(rect, &mut virt);
            }
            stack::blit(
                frame,
                body,
                &virt,
                u16::try_from(scroll - origin).unwrap_or(u16::MAX),
            );
        }

        let mut sb_state = ScrollbarState::new(max_scroll).position(scroll);
        frame.render_stateful_widget(
            scrollbar(true),
            area.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut sb_state,
        );
    }
}

/// Content modals cover most of the frame, inset a few cells so the
/// main view remains visible behind them.
fn modal_rect(frame: Rect) -> Rect {
    let margin_x = (frame.width / 10).clamp(1, 4);
    let margin_y = (frame.height / 10).clamp(1, 3);
    frame.inner(Margin {
        horizontal: margin_x,
        vertical: margin_y,
    })
}
