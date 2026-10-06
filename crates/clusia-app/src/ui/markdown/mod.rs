//! Markdown comments as rich text: a pure parse into blocks and inline runs (`parse`) and a
//! builder (`build`) that lays them out as one node per word, code run, emoji and image.

pub mod build;
pub mod parse;

pub use build::{
    CopiedText, CopyMarkdown, MarkdownPlugin, MdBody, MdEmoji, MdImage, MdLink, RenderOpts,
    copy_button, is_external, line_inlines, link_chip, markdown, markdown_line,
};
