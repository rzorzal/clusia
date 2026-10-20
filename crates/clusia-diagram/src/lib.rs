//! Diagram layout, pure and deterministic: a `DiagramSpec` and a text `Measure` give a
//! `Geometry` of boxes with wrapped text and orthogonal edge routes. It also lays out the
//! keyword map in 3D (`keywords::scene`). No I/O, no UI types.

mod geometry;
pub mod keywords;
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
pub use layout::{MARGIN, Packing, layout, layout_ranked};
pub use measure::{Advances, FaceMeasure, Fonts, Measure, TextStyle, style_size};
