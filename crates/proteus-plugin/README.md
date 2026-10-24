# Declarative plugin UI

Implement `Component::view()` and return an `Element` tree. The SDK handles
layout, clipping, padding, separators, inherited styles, cursors and images.
Custom components compose through the same trait; no drawing coordinates or
canvas calls are needed.

```rust
use proteus_plugin::{Component, Element, List, TextInput, text, types::Style};
use proteus_plugin::ui::Length;

struct Browser {
    files: List,
    filter: TextInput,
}

impl Component for Browser {
    fn view(&mut self) -> Element<'_> {
        Element::page(
            Element::text("Browser"),
            Element::row([
                Element::list(&mut self.files, |index, selected, _width| {
                    let label = text(format!("Item {index}"));
                    vec![if selected { label.reverse() } else { label }]
                }).width(Length::Percent(60)).id("files"),
                Element::text("Preview").id("preview"),
            ]).separated(Style::PLAIN),
            Element::input(vec![text("/")], &self.filter),
        )
    }
}
```

Render once after changing state or receiving a resize:

```rust
let rendered = browser.render(cols, rows);
host::draw(surface_id, &rendered.frame)?;

// Use the region emitted by the same layout pass for mouse handling.
if let Some(area) = rendered.area("files") {
    if let Some(index) = browser.files.at_point(area, mouse.col as usize, mouse.row as usize) {
        browser.files.selected = index;
    }
}
```

State and event handling are ordinary Rust code in the plugin. A view is rebuilt
on each render and may borrow the component's list state; the tree is consumed
before `render` returns. The SDK does not introduce another event loop or plugin
lifecycle. The SDK renderer makes no host calls, so components with in-memory state can
be tested natively.

## Composition

| Element | Purpose |
|---|---|
| `page(header, body, footer)` | One-row header/footer and growing body; the footer wins on a one-row surface |
| `row(children)` / `column(children)` | Nested layouts with `.width(...)`, `.height(...)`, `.gap(...)`, `.separated(style)` |
| `text`, `line`, `lines`, `borrowed_lines` | Plain or styled text; large borrowed buffers copy only visible rows |
| `bar(left, right)` | Left label and right-aligned metadata |
| `list(&mut state, row)` | Custom item rows; only visible items are formatted, after their width is known |
| `input(prefix, &field)` | Scrolling `TextInput` and automatic cursor placement |
| `image(ImageView)` | Image fitted to the assigned region with aspect preserved |
| `component(value)` | Nest another `Component` implementation |
| `responsive(closure)` | Choose a tree based on the **allocated** region size |

For example, a preview can disappear automatically in a narrow region:

```rust
Element::responsive(move |size| {
    if size.width >= 60 {
        Element::row([files.width(Length::Percent(60)), preview])
            .separated(Style::PLAIN)
    } else {
        files
    }
})
```

`Length::Fixed(n)` and `Percent(p)` consume space first; `Fill(weight)` shares
the remaining cells. Styles inherit through the tree; explicit span colors win.
`.id("name")` records a region in the returned `Rendered`; `area` looks it up
and `hit(col, row)` finds the deepest/last named region at a point. IDs should be
unique; repeated IDs resolve to the last rendered occurrence. All coordinates
are zero-based cells, matching the WASM UI API.

`List` keeps selection/scroll state, and `TextInput` keeps editing state. Feed
events into their existing `key`, `insert` and `scroll` methods, then render again.
For multiple inputs, use `.focused(false)` on inactive fields so only the active
field supplies a cursor; event routing follows your own focus state.
The image, input, cursor and region maps are rebuilt on every render, preventing
stale placements after conditional views or resizing.

Tiny and zero-sized regions are valid. The renderer bounds all output, pads
partial wide glyphs, and keeps grapheme clusters intact when they are contained
in a span. `ImageView` uses 8×16 pixel cells by default; its `cell_pixels` field
can specify another intended cell size/aspect. `Canvas`/`Rect` are also available
as low-level composition utilities.

The file manager in `plugins/files/src/render.rs` uses this API for its browser,
directory columns and file preview. It contains no canvas drawing or duplicate
mouse-layout calculation. The SDK's module example is checked by Rust doctests.

## Actions and default keybindings

Register an ID independently of its command-palette label:

```rust
host::register_action("sidebar.toggle", "Files: sidebar", Some("alt+e"))?;
```

Handle `Event::Action(id)` by comparing `id` to `"sidebar.toggle"`. Both palette
and key invocations deliver this ID. A label can change without breaking config.
Use `Some("pane:E")` for a mode-specific default or `None` for palette-only.
IDs use 1–100 ASCII letters, digits, dots, underscores or hyphens.

The user can rebind the action exactly through ordinary Lua keymaps:

```lua
require("proteus").config {
  keys = {
    normal = {
      ["alt+e"] = false,
      ["ctrl+alt+e"] = "plugin:proteus.files:sidebar.toggle",
    },
  },
}
```

The reference is `plugin:PLUGIN-ID:ACTION-ID`, and is accepted before the plugin
starts. `p.keymap(mode, key, action_id)` works too. Like built-ins, binding a
new key adds a binding; explicitly unbind the old one to replace it. Explicit
bindings and unbindings always win over plugin defaults. Re-registering an ID
updates its label/default without duplicating it; changing the label never
changes the ID. Removing a plugin removes its defaults but retains the user's
configured references for when it runs again.

### Localization

`i18n::catalog()` creates a Fluent catalog using `host::locale()`.
Load standard `.ftl` resources with `Catalog::add_resource(locale, source)`, then
call `Catalog::format(id, Some(&args))` using typed `FluentArgs`. Fluent handles
variables, terms, attributes, select expressions, and CLDR plural rules including
decimals. On `Event::ConfigChanged`, use `i18n::sync`, refresh action labels/status,
and redraw. See [the localization guide](../../docs/i18n.md) and the pane-list
example's `locales` directory.

Use `host::set_surface_title(surface, title)` when reloading translated titles;
it updates an owned surface without reopening it. Explicit user pane renames
are preserved.

### Plugin-owned configuration

Users pass settings with `p.load_plugin { source = "...", config = { ... } }`.
Deserialize them during `Guest::init`, using Serde from the SDK:

```rust
use proteus_plugin::{host, serde::Deserialize};

#[derive(Default, Deserialize)]
#[serde(crate = "proteus_plugin::serde", default, deny_unknown_fields)]
struct Settings {
    greeting: String,
}

// In init: returning an error reports an invalid configuration to the user.
// let settings: Settings = host::config()?;
```

`host::config::<T>()` requires `DeserializeOwned + Default`; omitted settings use
`T::default()`. `host::plugin_config()` returns the raw JSON string (`null` when
omitted). The host does not interpret plugin settings. Config changes restart the
instance, so read them during initialization. Existing source/build caches are reused.
