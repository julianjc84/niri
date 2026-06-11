use std::cell::RefCell;
use std::cmp::max;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::iter::zip;
use std::rc::Rc;

use niri_config::{Action, Bind, Config, Key, ModKey, Modifiers, Trigger};
use pangocairo::cairo::{self, ImageSurface};
use pangocairo::pango::{AttrColor, AttrInt, AttrList, AttrString, FontDescription, Weight};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::input::keyboard::xkb::keysym_get_name;
use smithay::output::{Output, WeakOutput};
use smithay::reexports::gbm::Format as Fourcc;
use smithay::utils::{Logical, Scale, Size, Transform};

use crate::render_helpers::primary_gpu_texture::PrimaryGpuTextureRenderElement;
use crate::render_helpers::renderer::NiriRenderer;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::utils::{output_size, to_physical_precise_round};

const PADDING: i32 = 8;
const MARGIN: i32 = PADDING * 2;
const FONT: &str = "sans 14px";
const BORDER: i32 = 4;
const LINE_INTERVAL: i32 = 2;
const TITLE: &str = "Important Hotkeys";

pub struct HotkeyOverlay {
    is_open: bool,
    config: Rc<RefCell<Config>>,
    mod_key: ModKey,
    buffers: RefCell<HashMap<WeakOutput, RenderedOverlay>>,
}

pub struct RenderedOverlay {
    buffer: Option<TextureBuffer<GlesTexture>>,
    // Output size the overlay was laid out for; the column flow depends on it.
    output_size: Size<f64, Logical>,
}

impl HotkeyOverlay {
    pub fn new(config: Rc<RefCell<Config>>, mod_key: ModKey) -> Self {
        Self {
            is_open: false,
            config,
            mod_key,
            buffers: RefCell::new(HashMap::new()),
        }
    }

    pub fn show(&mut self) -> bool {
        if !self.is_open {
            self.is_open = true;
            true
        } else {
            false
        }
    }

    pub fn hide(&mut self) -> bool {
        if self.is_open {
            self.is_open = false;
            true
        } else {
            false
        }
    }

    pub fn is_open(&self) -> bool {
        self.is_open
    }

    pub fn on_hotkey_config_updated(&mut self, mod_key: ModKey) {
        self.mod_key = mod_key;
        self.buffers.borrow_mut().clear();
    }

    pub fn render<R: NiriRenderer>(
        &self,
        renderer: &mut R,
        output: &Output,
    ) -> Option<PrimaryGpuTextureRenderElement> {
        if !self.is_open {
            return None;
        }

        let scale = output.current_scale().fractional_scale();
        let output_size = output_size(output);

        let mut buffers = self.buffers.borrow_mut();
        buffers.retain(|output, _| output.is_alive());

        // FIXME: should probably use the working area rather than view size.
        let weak = output.downgrade();
        if let Some(rendered) = buffers.get(&weak) {
            let mut invalidate = rendered.output_size != output_size;
            if let Some(buffer) = &rendered.buffer {
                invalidate |= buffer.texture_scale() != Scale::from(scale);
            }
            if invalidate {
                buffers.remove(&weak);
            }
        }

        let rendered = buffers.entry(weak).or_insert_with(|| {
            let renderer = renderer.as_gles_renderer();
            render(
                renderer,
                &self.config.borrow(),
                self.mod_key,
                scale,
                output_size,
            )
            .unwrap_or_else(|_| RenderedOverlay {
                buffer: None,
                output_size,
            })
        });
        let buffer = rendered.buffer.as_ref()?;

        let size = buffer.logical_size();
        let location = (output_size.to_f64().to_point() - size.to_point()).downscale(2.);
        let mut location = location.to_physical_precise_round(scale).to_logical(scale);
        location.x = f64::max(0., location.x);
        location.y = f64::max(0., location.y);

        let elem = TextureRenderElement::from_texture_buffer(
            buffer.clone(),
            location,
            0.9,
            None,
            None,
            Kind::Unspecified,
        );

        Some(PrimaryGpuTextureRenderElement(elem))
    }

    pub fn a11y_text(&self) -> String {
        let config = self.config.borrow();
        let sections = collect_sections(&config);

        let mut buf = String::new();
        writeln!(&mut buf, "{TITLE}").unwrap();

        for section in sections {
            let mut wrote_title = false;

            for action in section.actions {
                let Some((key, action)) = format_bind(&config.binds.0, action) else {
                    continue;
                };

                if !wrote_title {
                    writeln!(&mut buf, "{}", section.title).unwrap();
                    wrote_title = true;
                }

                let key = key.map(|key| key_name(true, self.mod_key, &key));
                let key = key.as_deref().unwrap_or("not bound");

                let action = match pango::parse_markup(&action, '\0') {
                    Ok((_attrs, text, _accel)) => text,
                    Err(_) => action.into(),
                };

                writeln!(&mut buf, "{key} {action}").unwrap();
            }
        }

        buf
    }
}

fn format_bind(binds: &[Bind], action: &Action) -> Option<(Option<Key>, String)> {
    let mut bind_with_non_null = None;
    let mut bind_with_custom_title = None;
    let mut found_null_title = false;

    for bind in binds {
        if bind.action != *action {
            continue;
        }

        match &bind.hotkey_overlay_title {
            Some(Some(_)) => {
                bind_with_custom_title.get_or_insert(bind);
            }
            Some(None) => {
                found_null_title = true;
            }
            None => {
                bind_with_non_null.get_or_insert(bind);
            }
        }
    }

    if bind_with_custom_title.is_none() && found_null_title {
        return None;
    }

    let mut title = None;
    let key = if let Some(bind) = bind_with_custom_title.or(bind_with_non_null) {
        if let Some(Some(custom)) = &bind.hotkey_overlay_title {
            title = Some(custom.clone());
        }

        Some(bind.key)
    } else {
        None
    };
    let title = title.unwrap_or_else(|| action_name(action));

    Some((key, title))
}

struct Section<'a> {
    title: &'static str,
    actions: Vec<&'a Action>,
}

fn collect_sections(config: &Config) -> Vec<Section<'_>> {
    let binds = &config.binds.0;

    // Compositor-level actions.
    let mut compositor = vec![&Action::ShowHotkeyOverlay];

    // Prefer Quit(false) if found, otherwise try Quit(true), and if there's neither, fall back to
    // Quit(false).
    if binds.iter().any(|bind| bind.action == Action::Quit(false)) {
        compositor.push(&Action::Quit(false));
    } else if binds.iter().any(|bind| bind.action == Action::Quit(true)) {
        compositor.push(&Action::Quit(true));
    } else {
        compositor.push(&Action::Quit(false));
    }

    compositor.push(&Action::ToggleOverview);

    // Screenshot is not as important, can omit if not bound.
    if let Some(bind) = binds
        .iter()
        .find(|bind| matches!(bind.action, Action::Screenshot(_, _)))
    {
        compositor.push(&bind.action);
    }

    // Moving focus around.
    let navigation = vec![
        &Action::FocusColumnLeft,
        &Action::FocusColumnRight,
        &Action::FocusWorkspaceDown,
        &Action::FocusWorkspaceUp,
    ];

    // Manipulating windows and columns.
    let mut windows = vec![
        &Action::CloseWindow,
        &Action::MoveColumnLeft,
        &Action::MoveColumnRight,
    ];

    // Prefer move-column-to-workspace-down, but fall back to move-window-to-workspace-down.
    if let Some(bind) = binds
        .iter()
        .find(|bind| matches!(bind.action, Action::MoveColumnToWorkspaceDown(_)))
    {
        windows.push(&bind.action);
    } else if binds
        .iter()
        .any(|bind| matches!(bind.action, Action::MoveWindowToWorkspaceDown(_)))
    {
        windows.push(&Action::MoveWindowToWorkspaceDown(true));
    } else {
        windows.push(&Action::MoveColumnToWorkspaceDown(true));
    }

    // Same for -up.
    if let Some(bind) = binds
        .iter()
        .find(|bind| matches!(bind.action, Action::MoveColumnToWorkspaceUp(_)))
    {
        windows.push(&bind.action);
    } else if binds
        .iter()
        .any(|bind| matches!(bind.action, Action::MoveWindowToWorkspaceUp(_)))
    {
        windows.push(&Action::MoveWindowToWorkspaceUp(true));
    } else {
        windows.push(&Action::MoveColumnToWorkspaceUp(true));
    }

    windows.extend(&[
        &Action::SwitchPresetColumnWidth,
        &Action::MaximizeColumn,
        &Action::ConsumeOrExpelWindowLeft,
        &Action::ConsumeOrExpelWindowRight,
        &Action::ToggleWindowFloating,
        &Action::SwitchFocusBetweenFloatingAndTiling,
    ]);

    let in_fixed = |action: &Action| {
        compositor.contains(&action) || navigation.contains(&action) || windows.contains(&action)
    };

    let mut apps = Vec::new();
    let mut other = Vec::new();

    // Add actions with a custom hotkey-overlay-title.
    for bind in binds {
        if matches!(bind.hotkey_overlay_title, Some(Some(_))) {
            let action = &bind.action;

            // Avoid duplicate actions.
            if in_fixed(action) || apps.contains(&action) || other.contains(&action) {
                continue;
            }

            if matches!(action, Action::Spawn(_) | Action::SpawnSh(_)) {
                apps.push(action);
            } else {
                other.push(action);
            }
        }
    }

    // Add the spawn actions.
    for bind in binds.iter().filter(|bind| {
        matches!(bind.action, Action::Spawn(_) | Action::SpawnSh(_))
            // Only show binds with Mod or Super to filter out stuff like volume up/down.
            && (bind.key.modifiers.contains(Modifiers::COMPOSITOR)
                || bind.key.modifiers.contains(Modifiers::SUPER))
            // Also filter out wheel and touchpad scroll binds.
            && matches!(bind.key.trigger, Trigger::Keysym(_))
    }) {
        let action = &bind.action;

        // We only show one bind for each action, so we need to deduplicate the Spawn actions.
        if !apps.contains(&action) {
            apps.push(action);
        }
    }

    let mut sections = vec![
        Section {
            title: "Compositor",
            actions: compositor,
        },
        Section {
            title: "Navigation",
            actions: navigation,
        },
        Section {
            title: "Windows",
            actions: windows,
        },
        Section {
            title: "Apps",
            actions: apps,
        },
        Section {
            title: "Other",
            actions: other,
        },
    ];

    if config.hotkey_overlay.hide_not_bound {
        // Only keep actions that have been bound
        for section in &mut sections {
            section
                .actions
                .retain(|&action| binds.iter().any(|bind| bind.action == *action));
        }
    }

    // Drop sections that ended up empty.
    sections.retain(|section| !section.actions.is_empty());

    sections
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Item {
    /// Header of section `.0`.
    Header(usize),
    /// Row `.1` of section `.0`.
    Row(usize, usize),
}

/// Flows sections into columns no taller than `budget`.
///
/// Takes (header height, row heights) per section; all heights must already include the line
/// interval below the element. `gap` is the extra space above a section header that isn't at the
/// top of its column.
///
/// Content flows continuously: a section that hits the bottom of a column continues its rows at
/// the top of the next one, without repeating the header. The one exception is that a header is
/// never left alone at the bottom of a column — it moves together with at least its first row.
fn flow_into_columns(sections: &[(i32, Vec<i32>)], gap: i32, budget: i32) -> Vec<Vec<Item>> {
    let mut columns = vec![Vec::new()];
    let mut col_h = 0;

    for (s, (header_h, rows)) in sections.iter().enumerate() {
        // Keep the header attached to at least the first row.
        let keep = header_h + rows.first().copied().unwrap_or(0);
        if !columns.last().unwrap().is_empty() && col_h + gap + keep > budget {
            columns.push(Vec::new());
            col_h = 0;
        }

        if !columns.last().unwrap().is_empty() {
            col_h += gap;
        }
        columns.last_mut().unwrap().push(Item::Header(s));
        col_h += header_h;

        for (r, row_h) in rows.iter().enumerate() {
            // The col_h > 0 check keeps a row taller than the whole budget from spilling into
            // an endless run of empty columns.
            if col_h + row_h > budget && col_h > 0 {
                columns.push(Vec::new());
                col_h = 0;
            }
            columns.last_mut().unwrap().push(Item::Row(s, r));
            col_h += row_h;
        }
    }

    columns
}

fn render(
    renderer: &mut GlesRenderer,
    config: &Config,
    mod_key: ModKey,
    scale: f64,
    output_size: Size<f64, Logical>,
) -> anyhow::Result<RenderedOverlay> {
    let _span = tracy_client::span!("hotkey_overlay::render");

    let margin: i32 = to_physical_precise_round(scale, MARGIN);
    let padding: i32 = to_physical_precise_round(scale, PADDING);
    let line_interval: i32 = to_physical_precise_round(scale, LINE_INTERVAL);

    let sections = collect_sections(config)
        .into_iter()
        .filter_map(|section| {
            let rows = section
                .actions
                .into_iter()
                .filter_map(|action| format_bind(&config.binds.0, action))
                .map(|(key, action)| {
                    let key = key.map(|key| key_name(false, mod_key, &key));
                    let key = key.as_deref().unwrap_or("(not bound)");
                    let key = format!(" {key} ");
                    (key, action)
                })
                .collect::<Vec<_>>();
            (!rows.is_empty()).then_some((section.title, rows))
        })
        .collect::<Vec<_>>();

    let mut font = FontDescription::from_string(FONT);
    font.set_absolute_size(to_physical_precise_round(scale, font.size()));

    let surface = ImageSurface::create(cairo::Format::ARgb32, 0, 0)?;
    let cr = cairo::Context::new(&surface)?;
    let layout = pangocairo::functions::create_layout(&cr);
    layout.context().set_round_glyph_positions(false);
    layout.set_font_description(Some(&font));

    let bold = AttrList::new();
    bold.insert(AttrInt::new_weight(Weight::Bold));
    layout.set_attributes(Some(&bold));
    layout.set_text(TITLE);
    let title_size = layout.pixel_size();

    // Section headers: bold, tinted like the border accent.
    let header_attrs = AttrList::new();
    header_attrs.insert(AttrInt::new_weight(Weight::Bold));
    header_attrs.insert(AttrColor::new_foreground(32768, 52428, 65535));

    let attrs = AttrList::new();
    attrs.insert(AttrString::new_family("Monospace"));
    attrs.insert(AttrColor::new_background(12000, 12000, 12000));

    layout.set_attributes(Some(&header_attrs));
    let header_sizes = sections
        .iter()
        .map(|(title, _)| {
            layout.set_text(title);
            layout.pixel_size()
        })
        .collect::<Vec<_>>();

    layout.set_attributes(Some(&attrs));
    let key_sizes = sections
        .iter()
        .map(|(_, rows)| {
            rows.iter()
                .map(|(key, _)| {
                    layout.set_text(key);
                    layout.pixel_size()
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    layout.set_attributes(None);
    let action_sizes = sections
        .iter()
        .map(|(_, rows)| {
            rows.iter()
                .map(|(_, action)| {
                    layout.set_markup(action);
                    layout.pixel_size()
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    // Per-section element heights, line interval included.
    let sec_dims = zip(&header_sizes, zip(&key_sizes, &action_sizes))
        .map(|((_, header_h), (keys, actions))| {
            let rows = zip(keys, actions)
                .map(|((_, key_h), (_, act_h))| max(*key_h, *act_h) + line_interval)
                .collect::<Vec<_>>();
            (header_h + line_interval, rows)
        })
        .collect::<Vec<_>>();

    // Flow the sections into columns that fit within the output, leaving a margin around the
    // overlay.
    let target_h: i32 = to_physical_precise_round(scale, output_size.h);
    let budget = max(
        target_h - margin * 2 - padding * 2 - title_size.1 - padding,
        1,
    );
    let columns = flow_into_columns(&sec_dims, padding, budget);

    let column_height = |col: &[Item]| -> i32 {
        let mut h = 0;
        for item in col {
            match *item {
                Item::Header(s) => {
                    if h > 0 {
                        h += padding;
                    }
                    h += sec_dims[s].0;
                }
                Item::Row(s, r) => h += sec_dims[s].1[r],
            }
        }
        h
    };

    let key_width = key_sizes
        .iter()
        .flatten()
        .map(|(w, _)| *w)
        .max()
        .unwrap_or(0);
    let action_width = action_sizes
        .iter()
        .flatten()
        .map(|(w, _)| *w)
        .max()
        .unwrap_or(0);
    let header_width = header_sizes.iter().map(|(w, _)| *w).max().unwrap_or(0);
    let col_width = max(key_width + padding + action_width, header_width);
    let col_gap = padding * 3;

    let n_cols = columns.len() as i32;
    let content_width = n_cols * col_width + (n_cols - 1) * col_gap;
    let content_height = columns
        .iter()
        .map(|col| column_height(col))
        .max()
        .unwrap_or(0);

    let mut width = max(content_width, title_size.0);
    let mut height = title_size.1 + padding + content_height;

    width += padding * 2;
    height += padding * 2;

    let surface = ImageSurface::create(cairo::Format::ARgb32, width, height)?;
    let cr = cairo::Context::new(&surface)?;
    cr.set_source_rgb(0.1, 0.1, 0.1);
    cr.paint()?;

    let layout = pangocairo::functions::create_layout(&cr);
    layout.context().set_round_glyph_positions(false);
    layout.set_font_description(Some(&font));

    cr.set_source_rgb(1., 1., 1.);

    cr.move_to(((width - title_size.0) / 2).into(), padding.into());
    layout.set_attributes(Some(&bold));
    layout.set_text(TITLE);
    pangocairo::functions::show_layout(&cr, &layout);

    let mut x = padding;
    for col in &columns {
        let mut y = padding + title_size.1 + padding;
        let mut first = true;

        for item in col {
            match *item {
                Item::Header(s) => {
                    if !first {
                        y += padding;
                    }
                    cr.move_to(x.into(), y.into());
                    layout.set_attributes(Some(&header_attrs));
                    layout.set_text(sections[s].0);
                    pangocairo::functions::show_layout(&cr, &layout);
                    y += sec_dims[s].0;
                }
                Item::Row(s, r) => {
                    let (key, action) = &sections[s].1[r];

                    cr.move_to(x.into(), y.into());
                    layout.set_attributes(Some(&attrs));
                    layout.set_text(key);
                    pangocairo::functions::show_layout(&cr, &layout);

                    cr.move_to((x + key_width + padding).into(), y.into());

                    let (attrs, text) = match pango::parse_markup(action, '\0') {
                        Ok((attrs, text, _accel)) => (Some(attrs), text),
                        Err(err) => {
                            warn!("error parsing markup for key {key}: {err}");
                            (None, action.into())
                        }
                    };

                    layout.set_attributes(attrs.as_ref());
                    layout.set_text(&text);
                    pangocairo::functions::show_layout(&cr, &layout);

                    y += sec_dims[s].1[r];
                }
            }
            first = false;
        }

        x += col_width + col_gap;
    }

    cr.move_to(0., 0.);
    cr.line_to(width.into(), 0.);
    cr.line_to(width.into(), height.into());
    cr.line_to(0., height.into());
    cr.line_to(0., 0.);
    cr.set_source_rgb(0.5, 0.8, 1.0);
    // Keep the border width even to avoid blurry edges.
    cr.set_line_width((f64::from(BORDER) / 2. * scale).round() * 2.);
    cr.stroke()?;
    drop(cr);

    let data = surface.take_data().unwrap();
    let buffer = TextureBuffer::from_memory(
        renderer,
        &data,
        Fourcc::Argb8888,
        (width, height),
        false,
        scale,
        Transform::Normal,
        Vec::new(),
    )?;

    Ok(RenderedOverlay {
        buffer: Some(buffer),
        output_size,
    })
}

fn action_name(action: &Action) -> String {
    match action {
        Action::Quit(_) => String::from("Exit niri"),
        Action::ShowHotkeyOverlay => String::from("Show Important Hotkeys"),
        Action::CloseWindow => String::from("Close Focused Window"),
        Action::FocusColumnLeft => String::from("Focus Column to the Left"),
        Action::FocusColumnRight => String::from("Focus Column to the Right"),
        Action::MoveColumnLeft => String::from("Move Column Left"),
        Action::MoveColumnRight => String::from("Move Column Right"),
        Action::FocusWorkspaceDown => String::from("Switch Workspace Down"),
        Action::FocusWorkspaceUp => String::from("Switch Workspace Up"),
        Action::MoveColumnToWorkspaceDown(_) => String::from("Move Column to Workspace Down"),
        Action::MoveColumnToWorkspaceUp(_) => String::from("Move Column to Workspace Up"),
        Action::MoveWindowToWorkspaceDown(_) => String::from("Move Window to Workspace Down"),
        Action::MoveWindowToWorkspaceUp(_) => String::from("Move Window to Workspace Up"),
        Action::SwitchPresetColumnWidth => String::from("Switch Preset Column Widths"),
        Action::MaximizeColumn => String::from("Maximize Column"),
        Action::ConsumeOrExpelWindowLeft => String::from("Consume or Expel Window Left"),
        Action::ConsumeOrExpelWindowRight => String::from("Consume or Expel Window Right"),
        Action::ToggleWindowFloating => String::from("Move Window Between Floating and Tiling"),
        Action::SwitchFocusBetweenFloatingAndTiling => {
            String::from("Switch Focus Between Floating and Tiling")
        }
        Action::ToggleOverview => String::from("Open the Overview"),
        Action::Screenshot(_, _) => String::from("Take a Screenshot"),
        Action::Spawn(args) => format!(
            "Spawn <span face='monospace' bgcolor='#000000'>{}</span>",
            args.first().unwrap_or(&String::new())
        ),
        Action::SpawnSh(command) => format!(
            "Spawn <span face='monospace' bgcolor='#000000'>{}</span>",
            // Fairly crude but should get the job done in most cases.
            command.split_ascii_whitespace().next().unwrap_or("")
        ),
        _ => String::from("FIXME: Unknown"),
    }
}

fn key_name(screen_reader: bool, mod_key: ModKey, key: &Key) -> String {
    let mut name = String::new();

    let has_comp_mod = key.modifiers.contains(Modifiers::COMPOSITOR);

    // Compositor mod goes first.
    if has_comp_mod {
        match mod_key {
            ModKey::Super => {
                name.push_str("Super + ");
            }
            ModKey::Alt => {
                name.push_str("Alt + ");
            }
            ModKey::Shift => {
                name.push_str("Shift + ");
            }
            ModKey::Ctrl => {
                name.push_str("Ctrl + ");
            }
            ModKey::IsoLevel3Shift => {
                name.push_str("Mod5 + ");
            }
            ModKey::IsoLevel5Shift => {
                name.push_str("Mod3 + ");
            }
        }
    }

    if key.modifiers.contains(Modifiers::SUPER) && !(has_comp_mod && mod_key == ModKey::Super) {
        name.push_str("Super + ");
    }
    if key.modifiers.contains(Modifiers::CTRL) && !(has_comp_mod && mod_key == ModKey::Ctrl) {
        name.push_str("Ctrl + ");
    }
    if key.modifiers.contains(Modifiers::SHIFT) && !(has_comp_mod && mod_key == ModKey::Shift) {
        name.push_str("Shift + ");
    }
    if key.modifiers.contains(Modifiers::ALT) && !(has_comp_mod && mod_key == ModKey::Alt) {
        name.push_str("Alt + ");
    }
    if key.modifiers.contains(Modifiers::ISO_LEVEL3_SHIFT)
        && !(has_comp_mod && mod_key == ModKey::IsoLevel3Shift)
    {
        name.push_str("Mod5 + ");
    }
    if key.modifiers.contains(Modifiers::ISO_LEVEL5_SHIFT)
        && !(has_comp_mod && mod_key == ModKey::IsoLevel5Shift)
    {
        name.push_str("Mod3 + ");
    }

    let pretty = match key.trigger {
        Trigger::Keysym(keysym) => prettify_keysym_name(screen_reader, &keysym_get_name(keysym)),
        Trigger::MouseLeft => String::from("Mouse Left"),
        Trigger::MouseRight => String::from("Mouse Right"),
        Trigger::MouseMiddle => String::from("Mouse Middle"),
        Trigger::MouseBack => String::from("Mouse Back"),
        Trigger::MouseForward => String::from("Mouse Forward"),
        Trigger::WheelScrollDown => String::from("Wheel Scroll Down"),
        Trigger::WheelScrollUp => String::from("Wheel Scroll Up"),
        Trigger::WheelScrollLeft => String::from("Wheel Scroll Left"),
        Trigger::WheelScrollRight => String::from("Wheel Scroll Right"),
        Trigger::TouchpadScrollDown => String::from("Touchpad Scroll Down"),
        Trigger::TouchpadScrollUp => String::from("Touchpad Scroll Up"),
        Trigger::TouchpadScrollLeft => String::from("Touchpad Scroll Left"),
        Trigger::TouchpadScrollRight => String::from("Touchpad Scroll Right"),
        Trigger::TabletStylusButton1 => String::from("Tablet Stylus Button 1"),
        Trigger::TabletStylusButton2 => String::from("Tablet Stylus Button 2"),
        Trigger::TabletStylusButton3 => String::from("Tablet Stylus Button 3"),
    };
    name.push_str(&pretty);

    name
}

fn prettify_keysym_name(screen_reader: bool, name: &str) -> String {
    let name = if screen_reader {
        name
    } else {
        match name {
            "slash" => "/",
            "comma" => ",",
            "period" => ".",
            "minus" => "-",
            "equal" => "=",
            "grave" => "`",
            "bracketleft" => "[",
            "bracketright" => "]",
            "adiaeresis" => "Ä",
            "ediaeresis" => "Ë",
            "idiaeresis" => "Ï",
            "odiaeresis" => "Ö",
            "udiaeresis" => "Ü",
            "ydiaeresis" => "Ÿ",
            "wdiaeresis" => "Ẅ",
            _ => name,
        }
    };

    let name = match name {
        "Next" => "Page Down",
        "Prior" => "Page Up",
        "Print" => "PrtSc",
        "Return" => "Enter",
        "space" => "Space",
        _ => name,
    };

    if name.len() == 1 && name.is_ascii() {
        name.to_ascii_uppercase()
    } else {
        name.into()
    }
}

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;

    use super::*;

    #[track_caller]
    fn check(config: &str, action: Action) -> String {
        let config = Config::parse_mem(config).unwrap();
        if let Some((key, title)) = format_bind(&config.binds.0, &action) {
            let key = key.map(|key| key_name(false, ModKey::Super, &key));
            let key = key.as_deref().unwrap_or("(not bound)");
            format!(" {key} : {title}")
        } else {
            String::from("None")
        }
    }

    #[test]
    fn test_format_bind() {
        // Not bound.
        assert_snapshot!(check("", Action::Screenshot(true, None)), @" (not bound) : Take a Screenshot");

        // Bound with a default title.
        assert_snapshot!(
            check(
                r#"binds {
                    Mod+P { screenshot; }
                }"#,
                Action::Screenshot(true, None),
            ),
            @" Super + P : Take a Screenshot"
        );

        // Custom title.
        assert_snapshot!(
            check(
                r#"binds {
                    Mod+P hotkey-overlay-title="Hello" { screenshot; }
                }"#,
                Action::Screenshot(true, None),
            ),
            @" Super + P : Hello"
        );

        // Prefer first bind.
        assert_snapshot!(
            check(
                r#"binds {
                    Mod+P { screenshot; }
                    Print { screenshot; }
                }"#,
                Action::Screenshot(true, None),
            ),
            @" Super + P : Take a Screenshot"
        );

        // Prefer bind with custom title.
        assert_snapshot!(
            check(
                r#"binds {
                    Mod+P { screenshot; }
                    Print hotkey-overlay-title="My Cool Bind" { screenshot; }
                }"#,
                Action::Screenshot(true, None),
            ),
            @" PrtSc : My Cool Bind"
        );

        // Prefer first bind with custom title.
        assert_snapshot!(
            check(
                r#"binds {
                    Mod+P hotkey-overlay-title="First" { screenshot; }
                    Print hotkey-overlay-title="My Cool Bind" { screenshot; }
                }"#,
                Action::Screenshot(true, None),
            ),
            @" Super + P : First"
        );

        // Any bind with null title hides it.
        assert_snapshot!(
            check(
                r#"binds {
                    Mod+P { screenshot; }
                    Print hotkey-overlay-title=null { screenshot; }
                }"#,
                Action::Screenshot(true, None),
            ),
            @"None"
        );

        // Custom title takes preference over null.
        assert_snapshot!(
            check(
                r#"binds {
                    Mod+P hotkey-overlay-title="Hello" { screenshot; }
                    Print hotkey-overlay-title=null { screenshot; }
                }"#,
                Action::Screenshot(true, None),
            ),
            @" Super + P : Hello"
        );
    }

    #[test]
    fn test_sections() {
        let config = Config::parse_mem(
            r#"
            hotkey-overlay {
                hide-not-bound
            }
            binds {
                Mod+Shift+Slash { show-hotkey-overlay; }
                Mod+Shift+E { quit; }
                Mod+P { screenshot; }
                Mod+Left { focus-column-left; }
                Mod+Q { close-window; }
                Mod+T { spawn "alacritty"; }
                Mod+X hotkey-overlay-title="Do Something" { spawn-sh "something"; }
                Mod+C hotkey-overlay-title="Center It" { center-column; }
            }
            "#,
        )
        .unwrap();

        let overlay = HotkeyOverlay::new(Rc::new(RefCell::new(config)), ModKey::Super);
        assert_snapshot!(overlay.a11y_text(), @r"
        Important Hotkeys
        Compositor
        Super + Shift + slash Show Important Hotkeys
        Super + Shift + E Exit niri
        Super + P Take a Screenshot
        Navigation
        Super + Left Focus Column to the Left
        Windows
        Super + Q Close Focused Window
        Apps
        Super + X Do Something
        Super + T Spawn alacritty
        Other
        Super + C Center It
        ");
    }

    #[test]
    fn test_flow_single_column() {
        // Everything fits: one column, sections in order.
        let sections = vec![(10, vec![20, 20]), (10, vec![20])];
        let columns = flow_into_columns(&sections, 5, 1000);
        assert_eq!(
            columns,
            vec![vec![
                Item::Header(0),
                Item::Row(0, 0),
                Item::Row(0, 1),
                Item::Header(1),
                Item::Row(1, 0),
            ]],
        );
    }

    #[test]
    fn test_flow_section_continues_in_next_column() {
        // The second section starts below the first and overflows mid-section; the remaining
        // rows continue at the top of the next column without a repeated header.
        let sections = vec![(10, vec![20, 20]), (10, vec![20, 20])];
        let columns = flow_into_columns(&sections, 5, 100);
        assert_eq!(
            columns,
            vec![
                vec![
                    Item::Header(0),
                    Item::Row(0, 0),
                    Item::Row(0, 1),
                    Item::Header(1),
                    Item::Row(1, 0),
                ],
                vec![Item::Row(1, 1)],
            ],
        );
    }

    #[test]
    fn test_flow_header_keeps_first_row() {
        // The second header would fit at the bottom of the column, but not together with its
        // first row; it moves to the next column instead of being orphaned.
        let sections = vec![(10, vec![20, 20]), (10, vec![20, 20])];
        let columns = flow_into_columns(&sections, 5, 70);
        assert_eq!(
            columns,
            vec![
                vec![Item::Header(0), Item::Row(0, 0), Item::Row(0, 1)],
                vec![Item::Header(1), Item::Row(1, 0), Item::Row(1, 1)],
            ],
        );
    }

    #[test]
    fn test_flow_giant_section_splits() {
        // A section taller than the budget flows its rows across columns without repeating the
        // header.
        let sections = vec![(10, vec![20, 20, 20, 20, 20])];
        let columns = flow_into_columns(&sections, 5, 50);
        assert_eq!(
            columns,
            vec![
                vec![Item::Header(0), Item::Row(0, 0), Item::Row(0, 1)],
                vec![Item::Row(0, 2), Item::Row(0, 3)],
                vec![Item::Row(0, 4)],
            ],
        );
    }

    #[test]
    fn test_flow_tiny_budget_makes_progress() {
        // A row taller than the whole budget still gets placed; no infinite columns.
        let sections = vec![(10, vec![100, 100])];
        let columns = flow_into_columns(&sections, 5, 50);
        assert_eq!(
            columns,
            vec![
                vec![Item::Header(0)],
                vec![Item::Row(0, 0)],
                vec![Item::Row(0, 1)],
            ],
        );
    }
}
