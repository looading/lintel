# Native accessibility for the client-rendered menubar

The window-local menubar now supplies AccessKit menu semantics and actions. The UI is still rendered by iced: this does not install an AppKit application menu, a Win32 menubar, a GTK loop, or a GNOME/KDE global menu. Native window-menu attachment and client-rendered menu accessibility remain separate capabilities.

## Public application interface

Use the existing `public-iced` and `desktop` features, keep `MenuModel` in application state, wrap the window content with `menu::menu_bar`, and enable the existing `.accessibility()` application option. There is no menu-specific plugin, native handle, NodeId, mailbox, or action-handler registration for business code.

```rust,ignore
use lintel::{
    desktop::{MenuEntry, MenuSpec},
    iced::{Element, Task, widget::text},
    menu::{self, MenuAction, MenuModel},
};

struct State {
    menu: MenuModel,
    last_action: String,
}

#[derive(Clone, Debug)]
enum Message {
    Menu(MenuAction),
}

let initial_menu = MenuModel::new(MenuSpec {
    items: vec![MenuEntry::Submenu {
        label: "File".into(),
        enabled: true,
        items: vec![
            MenuEntry::item("open", "Open"),
            MenuEntry::Check {
                id: "pin".into(),
                label: "Pinned".into(),
                enabled: true,
                checked: false,
            },
        ],
    }],
})?;

lintel::application(
    move || State {
        menu: initial_menu.clone(),
        last_action: String::new(),
    },
    |state: &mut State, message| {
        match message {
            Message::Menu(action) => {
                if let Some(value) = action.checked {
                    // The example's IDs are static and known. Dynamic applications
                    // may instead handle an ID removed by a later business update.
                    state.menu.set_checked(&action.id, value)
                        .expect("known check item");
                }
                state.last_action = action.id;
            }
        }
        Task::none()
    },
    |state: &State| -> Element<'_, Message> {
        menu::menu_bar(
            &state.menu,
            Message::Menu,
            text(format!("Last action: {}", state.last_action)),
        ).into()
    },
)
.id("dev.example.accessible-menu")
.title("Accessible menu")
.accessibility()
.run()?;
```

`MenuAction` is unchanged: an owned action ID and an optional proposed checked value. Checking an item remains controlled by the application's normal update. Native actions produce the same Message/Task flow as pointer and keyboard selection. Constructing a new model on every view would invalidate an open interaction; retain the model in state.

Without `.accessibility()`, the client-rendered keyboard/pointer menu still works, but the native adapter does not publish its AX/UIA/AT-SPI tree or receive native requests.

## Node tree, properties and geometry

The exported hierarchy is:

```text
Window
└─ MenuBar
   ├─ MenuItem (header, has_popup=Menu, expanded=false/true)
   │  └─ Menu (only while this branch is open)
   │     ├─ MenuItem
   │     ├─ MenuItemCheckBox (toggled=false/true)
   │     └─ MenuItem (submenu, has_popup=Menu, expanded=false/true)
   │        └─ Menu (only while expanded)
   │           └─ MenuItem
   └─ MenuItem / MenuItemCheckBox (direct top-level actions)
```

Labels come from `MenuSpec`, without adding a checkmark character to the accessible name. Check state uses AccessKit `toggled`; submenu state uses `has_popup(Menu)` and `expanded`. Disabled entries remain readable, have the disabled property, and offer no executable actions. Disabled ancestors are checked again when a queued request is applied. Separators are decorative and are omitted from the accessible action tree.

Header bounds come from the actual header layout. Popup and row bounds come from the actual overlay layout, including edge flips and current scrolling. The collector converts widget/window-local logical coordinates to physical coordinates using the host scale factor; it does not manufacture a second native window. Fully clipped long-menu rows have no AccessKit bounds, while their logical entries remain in the open menu hierarchy. Focusing such an entry selects it and lets the next menu layout reveal it.

Menu focus is reported through `TreeUpdate.focus` using the current header/item ID. It is separate from the saved body focus token. Opening a menu suspends background action admission; closing the last active menu restores eligible weak body navigation without restoring an old body tree. A new collection publishes the current body nodes and focus.

## Actions and platform differences

The implementation was checked against the locally pinned AccessKit **0.25.2**, `accesskit_macos` **0.27.2**, `accesskit_windows` **0.35.0**, and `accesskit_atspi_common` **0.20.0** sources. An AccessKit action enum being present does not imply every native adapter exposes that action.

| Operation | Application-side behavior | macOS pinned adapter | Windows pinned adapter | Linux AT-SPI pinned adapter |
| --- | --- | --- | --- | --- |
| Focus | Select a header/item; open required ancestors; never invoke a business action. Focusing the MenuBar chooses its first enabled header; focusing a Menu chooses its first enabled entry. | `setAccessibilityFocused(true)` sends `Action::Focus`. | UIA SetFocus sends Focus. | Component GrabFocus sends Focus. |
| Click / Press | Invoke a leaf/check entry once through ordinary Message dispatch. On a submenu entry, toggle that branch. | `accessibilityPerformPress` sends Click. | Invoke sends Click; Toggle on check items also sends Click. | The exposed `click` action sends Click. |
| Expand | Idempotently open a submenu; no business invocation. | No native Expand/ShowMenu selector is assumed. Use Press/Click to enter. | ExpandCollapse uses `has_popup`/`expanded` and sends Expand. | No native Expand entry point is assumed. Use Click. |
| Collapse | Close a branch; collapsing a root header ends the menu session and restores body navigation. | No native Collapse selector is assumed. Press on an expanded header/submenu or keyboard Escape follows the menu's close behavior. | ExpandCollapse sends Collapse. | No native Collapse entry point is assumed. Click or keyboard Escape follows the menu's close behavior. |

Native role names also differ. macOS maps the check item to **AXMenuItem** with a boolean value; Windows maps `MenuItemCheckBox` to the native **CheckBox** control type; AT-SPI uses **CheckMenuItem**. The common AccessKit tree still uses `MenuBar`, `Menu`, `MenuItem`, and `MenuItemCheckBox`.

Expanded state is stored in AccessKit and drives the Windows ExpandCollapse pattern. The implementation does not claim that the pinned macOS/AT-SPI adapters provide a native expanded-state attribute or Expand/Collapse action merely because the common tree contains that property. On those platforms, submenu children appearing/disappearing and Press/Click are the practical inspection/action path.

The widget does not offer SetValue, text insertion, or arbitrary native data actions for menu nodes. Accelerator text remains metadata; this accessibility implementation does not register an accelerator or global shortcut.

## Native thread boundary and bounded delivery

Each MenuState owns one private mailbox with a maximum of **64 pending native requests**. Window snapshots reference it weakly. The mailbox's owner link is also weak; neither the snapshot nor the queue keeps a window or business state alive.

1. The native handler checks the current window snapshot, node/source admission, advertised action, disabled state and queue capacity. It only enqueues owned command data and wakes the host.
2. The UI thread drains requests during ordinary menu/overlay event processing, including redraws.
3. Before changing `MenuSession`, it rechecks the current model token, NodeId, target path, visible branch epoch, source admission and enabled ancestors.
4. A leaf/check selection calls the existing `menu_bar` message mapping on the UI thread. Focus, Expand and Collapse do not call a business handler.

The native thread does not execute a business closure, mutate `MenuModel`, synchronously wait for the UI, or touch an iced widget. Unsupported, stale, disabled, closed-window and full-queue requests are rejected. The native provider's immediate return does not acknowledge completed business work: effects are asynchronous, so tests should wait for the next UI update and inspect the resulting tree/message.

The existing snapshot lock serializes native admission with modal transitions and teardown. Wake calls run after releasing that lock. Multiple MenuBar states use distinct stable source IDs; the window's active-source set keeps background admission suspended until all open menu sources are closed. Requests for an inactive menu source do not bypass another source's modal boundary.

## Identity, revisions and teardown

- Menu and body nodes share the allocator below the high-bit raw-text ID space. IDs are not derived from display text or a business callback.
- A still-present control keeps its ID through ordinary label/checked/enabled/focus/hover changes. A different action ID or kind at the same path receives a different identity.
- Each queued request carries the model token and current target/epoch. A revision can invalidate queued intent even when the control ID itself remains stable.
- Closing a popup removes its branch routes and queued requests immediately. Reopening that branch allocates fresh popup/descendant IDs; an old native reference cannot reactivate the newly opened popup. Ancestor panes that remain open keep their IDs.
- Model replacement cancels stale interaction. The existing model policy may close the open popup on mutation; once a popup disappears, the popup lifecycle rule applies independently of the reason it closed.
- Dropping a MenuState closes its mailbox and releases only its own modal source. Closing the window closes every published menu mailbox, clears native requests and invalidates further delivery.

The menu keeps the existing no-click-through behavior: a consumed pointer down owns only its matching mouse-button or touch-finger up/cancel. Closing does not swallow unrelated text/IME or a later complete pointer gesture. There is no keyboard/IME replay queue in this accessibility path.

## Implementation layout

- `src/accessibility/menu.rs`: private node descriptions, identity/epoch registry and bounded mailbox.
- `src/menu.rs`: actual layout capture, native-command validation/execution and ordinary Message publication.
- `src/accessibility/collect.rs`: combines real menu geometry/tree data with the window's semantics and weak owner links; decorative menu text is not duplicated as unrelated labels.
- `src/accessibility/model.rs`: independent menu routes, source admission, body suspension/resume, window teardown and tree composition.
- `crates/lintel-desktop/src/client_menu.rs`: lightweight Focus/Expand/Collapse controller operations; applications do not need to call them to enable accessibility.
- `examples/client_menubar.rs`: unchanged application-facing MenuAction/Message pattern, with the existing `.accessibility()` option.

No new dependency, generic plugin framework, GTK loop, direct OS callback, or unsafe boundary was added.

## Verification status and acceptance boundary

The completed checks are recorded separately by evidence level:

| Check | Result | Evidence |
| --- | --- | --- |
| Standalone desktop controller filter | **11 passed, 0 failed** | `build/checklist-menubar-ax/controller-test.log` |
| New root `accessibility::menu` filter | **6 passed, 0 failed; 93 tests filtered out** | `build/checklist-accessibility-release/menu-ax-first-tests.log` |
| Full root regression | **99 passed, 0 failed** | `build/checklist-accessibility-release/root-menu-ax-tests.log` |
| macOS native AX | Role hierarchy, Press selection, boolean checked/disabled changes, nested/long items, subtree removal and body input recovery passed; Quit exit 0 | `build/checklist-accessibility-release/menu-ax-native-result.json` |
| Windows/Linux target checks | Type/dependency checks passed; no native execution | `build/checklist-accessibility-release/windows-menu-ax-check.log`, `linux-menu-ax-check.log` |

The six root tests cover roles/properties/geometry, stable identity versus popup epochs, queue limits/revisions/replacement, native-thread enqueue/source/window-close rejection, Weak lifetimes, and a real headless UserInterface sequence from native requests to UI-thread messages. The headless renderer creates no native window or GPU and does not exercise a Cocoa, COM/UIA or AT-SPI client.

The initial filter did not execute the old suite; the later full regression did. Native macOS inspection used the actual provider and observed 19 accepted native requests matched by 19 UI applications, with each requested business action delivered once. Two letter keys entered native IME preedit and were both committed on Return; an unchanged AX value during composition was not classified as lost text. No VoiceOver speech or Windows/Linux desktop runner was exercised.

macOS Press/Click and tree focus observations do not establish a separately operated Focus setter, a Windows UIA ExpandCollapse run or Linux AT-SPI actions. Cached stale native references, queue races and precise event-batch interleavings remain deterministic model/headless evidence. Record further provider and screen-reader results separately.
