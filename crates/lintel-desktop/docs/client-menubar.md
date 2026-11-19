# Client-rendered window menubar

`lintel::menu::menu_bar` provides an iced-rendered menubar inside an application window, including Linux X11/Wayland windows. It does not install a GTK menubar, a GNOME/KDE global menu, or a system application menu. Native Linux `Capability::WindowMenus` remains unsupported. This is an explicit client-rendered alternative.

The adapter is available with the root `public-iced` and `desktop` features. It adds no dependency, GTK loop, native window handle, or background thread. The standalone `lintel-desktop::client_menu` model/controller is available without native features or iced.

## Application interface

Keep a validated `MenuModel` in application state. The view needs one wrapper and an ordinary message mapping:

```rust,ignore
use lintel::{desktop::{MenuEntry, MenuSpec}, menu::{self, MenuAction, MenuModel}};

// At initialization:
let menu = MenuModel::new(MenuSpec {
    items: vec![MenuEntry::Submenu {
        label: "File".into(),
        enabled: true,
        items: vec![MenuEntry::item("open", "Open")],
    }],
})?;

// In the normal iced view:
menu::menu_bar(&state.menu, Message::Menu, content).into()

// In the normal update, retain the application's existing Task<Message> flow:
match message {
    Message::Menu(MenuAction { id, checked }) => {
        // Apply a check action's proposed value to the controlled model.
        if let Some(value) = checked {
            state.menu.set_checked(&id, value)?;
        }
        // Route id through the application's existing actions.
    }
    // ...
}
```

`MenuAction` contains an owned action ID and an optional proposed checked value. A selection does not mutate business state itself or bypass the application's Message/Task dispatch. `MenuModel::set_enabled`, `set_checked`, and `replace` validate their targets/input. Model changes invalidate open interaction, so a release from an older tree cannot activate a different item at the same path. Independently mutated clones receive distinct revision tokens.

The existing `MenuSpec` supports items, controlled check items, disabled items/submenus, separators, and nested submenus. Accelerator text is shown as metadata. This widget does not register that accelerator; use the existing window/global shortcut APIs when activation is needed.

## Interaction and lifecycle

- Click a header to open or toggle its submenu. Hover switches open headers/submenus. An action requires press and release on the same enabled path; disabled ancestors are checked again before activation.
- F10 opens/closes the menubar. CapsLock and NumLock do not block F10. Arrow keys move between entries/headers and nested menus, skipping disabled entries and separators. Home/End choose boundaries, Enter/Space select, Escape closes a nested level or the menu, and Tab/Shift+Tab close it. Ctrl/Alt/Logo chords are swallowed while modal without executing navigation/actions.
- While open, keyboard and IME events are captured before the content widget. This also works when F10 and the next key arrive in one iced update batch before the overlay has been constructed. Unbound letters/arrows cannot edit the focused background input.
- Opening the menu retains the prior content focus rather than retaining a native window or restoring an arbitrary stale widget ID. Closing, outside presses, window focus loss, model invalidation, and Tree destruction end the menu session. A removed/disabled underlying control is handled by the normal focus validation.
- Popup geometry is constrained to the window, nested panes flip at horizontal edges, and long panes support wheel scrolling and keyboard selection reveal. Geometry from a prior submenu path is never used after a same-batch path change/close; pointer input waits for current layout, and scrolling checks its current pane slot.
- Outside close presses and their matching mouse/touch releases are captured instead of activating the background, including a content widget with an independent `on_release` handler. A fresh complete gesture remains usable. Each menu selection publishes one normal application message. There is no system-wide shortcut handler or process-wide menu state.

With the optional native accessibility integration enabled, the framework binds a private weak callback to each menu. Opening and closing synchronously purge old native actions and change admission under the routing snapshot lock. Closing therefore admits ordinary keyboard/IME input immediately, without waiting for another collection or buffering/replaying text. The collector can read hidden body metadata for weak resume navigation; the modal native tree keeps the active menus and excludes background controls. Closing restores only live, enabled, actionable body routes. New AX actions accepted after close are retained. Multiple or nested menubars aggregate their private source identities, so binding a closed inner bar or closing one bar cannot release another open bar. Window-unfocus cleanup runs once, callbacks wake outside locks, and no strong window/state cycle is retained.

The public iced integration also preserves the unprocessed event suffix when an overlay disappears partway through a batch. Already captured prefix events retain their status; trailing character/IME input reaches the content once. The widget separately consumes the release of a gesture used to dismiss the menu, preventing that suffix delivery from becoming a background click.

The same wrapper now publishes MenuBar/Menu/MenuItem/MenuItemCheckBox semantics and native actions through a bounded weak mailbox. Native callbacks only enqueue requests; ordinary UI processing validates and applies them. See [menu-accessibility.md](../../../docs/menu-accessibility.md) for platform mappings, stable identities, popup epochs and the native-thread boundary. This remains a client-rendered window menu; it is not a native global menubar.

## Reuse and scope

The source survey found no existing menubar module. `lintel-iced-widgets/src/context_menu.rs` already establishes the relevant implementation pattern: per-widget Tree state, iced overlay ownership, viewport-edge constraints, and capturing menu input before underlying content/WebView input. Importing that entire crate would pull its CEF dependencies into a native-only application, so the new root adapter uses that pattern directly with existing iced row/column/container/text/rule widgets.

The selected public iced `widget::overlay::menu` primitive is a selectable options list. It does not represent MenuSpec separators, disabled/check items, nested panes, or their complete modal/focus lifecycle. The adapter composes those states rather than passing them through that options-list API as unsupported pseudo-items.

Source layout:

- `crates/lintel-desktop/src/client_menu.rs`: validated data/controller, no iced/native dependency.
- `src/menu.rs`: thin iced Widget/Overlay adapter and private window-scoped state.
- `examples/client_menubar.rs`: normal business Message/Task example, including disabled/check/deep/long menus, retained text input, and a background-action counter.

The implementation does not change the original business application, start a second GTK loop, or mount an unrelated native window.

## Verification boundary

Standalone model tests cover disabled ancestors, check actions, keyboard wrap/navigation, nested Escape/Tab, pointer cancellation, model replacement at the same path, independently mutated clones, invalid replacement, and empty models. Root tests use a real headless UserInterface and software renderer to cover initially closed/open menus, character/IME suffixes, shared owner navigation, fresh AX actions after close, nested menu admission, and mouse-area release isolation. Additional tests cover lock modifiers/command chords, stale overlay geometry/scrolling, focus-loss deferral, and absence of an accessibility collector. Root owns the shared Cargo and Linux/Windows cross-check commands.

Evidence is recorded under `build/checklist-integration/` and `build/checklist-accessibility-release/`, including the standalone controller checks, real UserInterface regressions and native follow-up results. The macOS build passed native AX Press through nested/check/disabled/long menu items, body focus restoration and Quit with exit code 0. Earlier native checks cover text/F10, Tab/Escape followed by text, background isolation, keyboard/wheel scrolling and normal gestures. These do not constitute screen-reader speech, every resize/edge-placement case or Linux/Windows desktop acceptance.

The debug example accepts `--trace-input` to correlate native winit, menu and subscription events. It records key categories, modifiers, status and focus transitions, without recording characters, IME content or AX values. The flag is opt-in and does not change default event routing.
