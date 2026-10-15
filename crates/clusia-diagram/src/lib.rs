//! Diagram layout, pure and deterministic: a `DiagramSpec` and a text `Measure` give a
//! `Geometry` of boxes with wrapped text and orthogonal edge routes. No I/O, no UI types.

mod geometry;
mod layered;
mod layout;
mod measure;
mod sequence;
mod spine;
mod tables;
mod text;
mod usecase;

#[cfg(test)]
mod testkit;

pub use geometry::{
    Activation, Align, Arrow, BoxGeo, EdgeGeo, Geometry, GroupGeo, Lifeline, MessageGeo, Point,
    Rect, RowGeo, TextLines,
};
pub use layout::{MARGIN, layout};
pub use measure::{Fonts, Measure, TextStyle, style_size};
