//! Virtual-buffer window rendering.
//!
//! Content taller than its viewport renders into an off-screen buffer
//! and the visible window is blitted into the frame. The session
//! viewer's body stacks mixed widgets this way; the scan view's
//! session dialog combines it with cached section heights so only the
//! sections intersecting the viewport are rendered per frame.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};

/// Copy `area.height` rows of `virt` starting at `src_offset` into the
/// frame at `area`. Rows past the end of `virt` are left untouched.
pub(crate) fn blit(frame: &mut Frame, area: Rect, virt: &Buffer, src_offset: u16) {
    let dst = frame.buffer_mut();
    for row in 0..area.height {
        let src_y = src_offset.saturating_add(row);
        if src_y >= virt.area.height {
            break;
        }
        for col in 0..area.width {
            if let (Some(src_cell), Some(dst_cell)) = (
                virt.cell(Position::new(col, src_y)),
                dst.cell_mut(Position::new(area.x + col, area.y + row)),
            ) {
                *dst_cell = src_cell.clone();
            }
        }
    }
}
