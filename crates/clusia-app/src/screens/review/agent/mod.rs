//! The agent's chat inside a review (mockup `AgentChat.png`): the per-review model and the
//! column that shows it.

pub mod model;
pub mod panel;
pub mod permission;
pub mod suggestion;

use bevy::prelude::*;

use crate::bridge::Model;
use crate::review_state::ReviewTabs;

/// Names each chat after the agent its review is pinned to.
pub fn name_chats(tabs: Res<ReviewTabs>, model: Res<Model>, mut chats: ResMut<Chats>) {
    for (pr, chat) in chats.0.iter_mut() {
        let name = tabs.0.get(pr).map_or_else(
            || crate::agent_name::active(&model.snapshot),
            |t| crate::agent_name::of_tab(&model.snapshot, t),
        );
        if chat.agent != name {
            chat.agent = name;
        }
    }
}

pub use model::{
    ChatLine, ChatModel, Chats, DEFAULT_WIDTH, MAX_WIDTH, MIN_WIDTH, PanelTab, SuggestionState,
    display_text, place_of,
};
pub use panel::{
    AgentPanel, AgentRegion, CHAT_INPUT, ChatInput, PanelColumn, PanelTabs, PanelToggle,
    harness_ready, on_panel_toggle, resize_edge, send_message,
};
pub use permission::{PermissionModal, PermissionPlugin, PermissionQueue};
