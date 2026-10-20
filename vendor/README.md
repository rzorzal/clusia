# Vendored crates

## `bevy_ui` 0.19.1

The unmodified crates.io `bevy_ui` 0.19.1 source (its `LICENSE-MIT` and `LICENSE-APACHE` are kept),
with one change in `src/widget/text_input_layout.rs`, wired in through `[patch.crates-io]` in the
root `Cargo.toml`.

**Why.** `text_input_layout_system` drives every `EditableText` editor through `DerefMut` on
every frame. That marks the component changed, so `update_editable_text_content_size` rewrites
the input's `ContentSize` and the taffy node is dirty up to the root: the whole UI is laid out
again each frame, even when nothing moved. With one idle, unfocused input on screen, a screen of
120 audit findings took 74 ms per frame instead of 6.5 ms.

**The change.** The editor is driven through `bypass_change_detection()`, and the component is
marked changed (`set_changed()`) only when the editor's layout generation moved, so consumers
that key on `Changed<EditableText>` after a re-layout (content size, scroll) still see it. Edits,
width changes and style changes mark it through `&mut` as before.

**Pinned by.** `an_unfocused_area_is_not_marked_changed_on_every_frame` and
`typing_still_grows_the_area_under_real_layout` (a guard that growth still works) in `crates/clusia-app/src/ui/text_area.rs`.

**When to drop it.** When Bevy ships a release with the same fix: delete `vendor/bevy_ui`, the
`[patch.crates-io]` entry and the `exclude`, and run `cargo update -p bevy_ui`. Until then, a
Bevy upgrade means re-vendoring the new `bevy_ui` source and re-applying the change.
