//! The agent's chat inside a review (mockup `AgentChat.png`): the per-review model and the
//! column that shows it.

pub mod model;
pub mod panel;
pub mod suggestion;

pub use model::{
    ChatLine, ChatModel, Chats, DEFAULT_WIDTH, MAX_WIDTH, MIN_WIDTH, PanelTab, SuggestionState,
    display_text, place_of,
};
pub use panel::{
    AgentPanel, AgentRegion, CHAT_INPUT, ChatInput, PanelColumn, PanelTabs, PanelToggle,
    harness_ready, on_panel_toggle, resize_edge, send_message,
};
