use std::cell::{Ref, RefMut, RefCell};
use std::rc::Rc;
use std::sync;
use std::sync::Arc;

use cairo;
use pangocairo as pc;
use pango;
use pango::FontDescription;
use gdk::{ModifierType, EventConfigure, EventButton, EventMotion, EventType, EventScroll};
use gdk_sys;
use glib;
use gtk::prelude::*;
use gtk::DrawingArea;

use neovim_lib::{Neovim, NeovimApi, Value};

use settings::{Settings, FontSource};
use ui_model::{UiModel, Cell, Attrs, Color, ModelRect, COLOR_BLACK, COLOR_WHITE, COLOR_RED};
use nvim;
use nvim::{RedrawEvents, GuiApi, RepaintMode, ErrorReport};
use input;
use input::keyval_to_input_string;
use cursor::Cursor;
use ui;
use ui::UiMutex;
use popup_menu::PopupMenu;

const DEFAULT_FONT_NAME: &'static str = "DejaVu Sans Mono 12";


#[derive(PartialEq)]
pub enum NvimMode {
    Normal,
    Insert,
    Other,
}

pub struct State {
    pub model: UiModel,
    bg_color: Color,
    fg_color: Color,
    sp_color: Color,
    cur_attrs: Option<Attrs>,
    pub mode: NvimMode,
    mouse_enabled: bool,
    drawing_area: DrawingArea,
    nvim: Option<Rc<RefCell<Neovim>>>,
    font_desc: FontDescription,
    cursor: Option<Cursor>,
    popup_menu: Option<PopupMenu>,
    settings: Rc<RefCell<Settings>>,

    line_height: Option<f64>,
    char_width: Option<f64>,
    request_resize: bool,
    resize_timer: Option<glib::SourceId>,

    parent: sync::Weak<UiMutex<ui::Components>>,
}

impl State {
    pub fn new(settings: Rc<RefCell<Settings>>, parent: &Arc<UiMutex<ui::Components>>) -> State {
        State {
            model: UiModel::new(24, 80),
            drawing_area: DrawingArea::new(),
            nvim: None,
            cur_attrs: None,
            bg_color: COLOR_BLACK,
            fg_color: COLOR_WHITE,
            sp_color: COLOR_RED,
            mode: NvimMode::Normal,
            mouse_enabled: true,
            font_desc: FontDescription::from_string(DEFAULT_FONT_NAME),
            cursor: None,
            popup_menu: None,
            settings: settings,

            line_height: None,
            char_width: None,
            resize_timer: None,
            request_resize: false,

            parent: Arc::downgrade(parent),
        }
    }

    pub fn nvim(&self) -> RefMut<Neovim> {
        self.nvim.as_ref().unwrap().borrow_mut()
    }

    fn create_pango_font(&self) -> FontDescription {
        self.font_desc.clone()
    }

    fn colors<'a>(&'a self, cell: &'a Cell) -> (&'a Color, &'a Color) {
        let bg = if let Some(ref bg) = cell.attrs.background {
            bg
        } else {
            &self.bg_color
        };
        let fg = if let Some(ref fg) = cell.attrs.foreground {
            fg
        } else {
            &self.fg_color
        };

        if cell.attrs.reverse {
            (fg, bg)
        } else {
            (bg, fg)
        }
    }

    pub fn set_font_desc(&mut self, desc: &str) {
        self.font_desc = FontDescription::from_string(desc);
        self.line_height = None;
        self.char_width = None;
    }

    pub fn open_file(&self, path: &str) {
        let mut nvim = self.nvim();
        nvim.command(&format!("e {}", path))
            .report_err(&mut *nvim);
    }

    pub fn cd(&self, path: &str) {
        let mut nvim = self.nvim();
        nvim.command(&format!("cd {}", path))
            .report_err(&mut *nvim);
    }

    fn request_resize(&mut self) {
        self.request_resize = true;
    }

    fn close_popup_menu(&self) {
        if self.popup_menu.is_some() {
            let mut nvim = self.nvim();
            nvim.input("<Esc>").report_err(&mut *nvim);
        }
    }

    fn queue_draw_area<M: AsRef<ModelRect>>(&self, rect_list: &Vec<M>) {
        match (&self.line_height, &self.char_width) {
            (&Some(line_height), &Some(char_width)) => {
                for rect in rect_list {
                    let mut rect = rect.as_ref().clone();
                    // this need to repain also line under curren line
                    // in case underscore or 'g' symbol is go here
                    // right one for italic symbol
                    rect.extend(0, 1, 0, 1);
                    let (x, y, width, height) = rect.to_area(line_height, char_width);
                    self.drawing_area.queue_draw_area(x, y, width, height);
                }
            }
            _ => self.drawing_area.queue_draw(),
        }
    }
}

pub struct UiState {
    mouse_pressed: bool,
}

impl UiState {
    pub fn new() -> UiState {
        UiState { mouse_pressed: false }
    }
}

pub struct Shell {
    pub state: Arc<UiMutex<State>>,
    ui_state: Rc<RefCell<UiState>>,
}

impl Shell {
    pub fn new(settings: Rc<RefCell<Settings>>, parent: &Arc<UiMutex<ui::Components>>) -> Shell {
        let shell = Shell {
            state: Arc::new(UiMutex::new(State::new(settings, parent))),
            ui_state: Rc::new(RefCell::new(UiState::new())),
        };

        let shell_ref = Arc::downgrade(&shell.state);
        shell.state.borrow_mut().cursor = Some(Cursor::new(shell_ref));

        shell
    }

    pub fn init(&mut self) {
        let state = self.state.borrow_mut();
        state.drawing_area.set_size_request(500, 300);
        state.drawing_area.set_hexpand(true);
        state.drawing_area.set_vexpand(true);
        state.drawing_area.set_can_focus(true);

        state
            .drawing_area
            .set_events((gdk_sys::GDK_BUTTON_RELEASE_MASK | gdk_sys::GDK_BUTTON_PRESS_MASK |
                         gdk_sys::GDK_BUTTON_MOTION_MASK |
                         gdk_sys::GDK_SCROLL_MASK)
                                .bits() as i32);

        let ref_state = self.state.clone();
        let ref_ui_state = self.ui_state.clone();
        state
            .drawing_area
            .connect_button_press_event(move |_, ev| {
                                            gtk_button_press(&mut *ref_state.borrow_mut(),
                                                             &mut *ref_ui_state.borrow_mut(),
                                                             ev)
                                        });

        let ref_ui_state = self.ui_state.clone();
        state
            .drawing_area
            .connect_button_release_event(move |_, _| {
                                              gtk_button_release(&mut *ref_ui_state.borrow_mut())
                                          });


        let ref_state = self.state.clone();
        let ref_ui_state = self.ui_state.clone();
        state
            .drawing_area
            .connect_motion_notify_event(move |_, ev| {
                                             gtk_motion_notify(&mut *ref_state.borrow_mut(),
                                                               &mut *ref_ui_state.borrow_mut(),
                                                               ev)
                                         });

        let ref_state = self.state.clone();
        state
            .drawing_area
            .connect_draw(move |_, ctx| {
                              let mut state = ref_state.borrow_mut();
                              let ref_parent = sync::Weak::upgrade(&state.parent).unwrap();
                              let parent = ref_parent.borrow();
                              gtk_draw(&*parent, &mut *state, ctx)
                          });

        let ref_state = self.state.clone();
        state
            .drawing_area
            .connect_key_press_event(move |_, ev| {
                                         let mut shell = ref_state.borrow_mut();
                                         shell.cursor.as_mut().unwrap().reset_state();
                                         let mut nvim = shell.nvim();
                                         input::gtk_key_press(&mut *nvim, ev)
                                     });

        let ref_state = self.state.clone();
        state
            .drawing_area
            .connect_scroll_event(move |_, ev| gtk_scroll_event(&mut *ref_state.borrow_mut(), ev));

        let ref_state = self.state.clone();
        state
            .drawing_area
            .connect_focus_in_event(move |_, _| gtk_focus_in(&mut *ref_state.borrow_mut()));

        let ref_state = self.state.clone();
        state
            .drawing_area
            .connect_focus_out_event(move |_, _| gtk_focus_out(&mut *ref_state.borrow_mut()));
    }

    pub fn state(&self) -> Ref<State> {
        self.state.borrow()
    }

    pub fn drawing_area(&self) -> Ref<DrawingArea> {
        Ref::map(self.state(), |s| &s.drawing_area)
    }

    #[cfg(unix)]
    pub fn redraw(&self, mode: &RepaintMode) {
        self.state.borrow_mut().on_redraw(mode);
    }

    #[cfg(unix)]
    pub fn set_font_desc(&self, font_name: &str) {
        self.state.borrow_mut().set_font_desc(font_name);
    }

    pub fn add_configure_event(&mut self) {
        let mut state = self.state.borrow_mut();

        let ref_state = self.state.clone();
        state
            .drawing_area
            .connect_configure_event(move |_, ev| gtk_configure_event(&ref_state, ev));

        state.cursor.as_mut().unwrap().start();
    }

    pub fn init_nvim(&mut self, nvim_bin_path: Option<&String>, external_popup: bool) {
        let nvim =
            nvim::initialize(self.state.clone(), nvim_bin_path, external_popup).expect("Can't start nvim instance");
        let mut state = self.state.borrow_mut();
        state.nvim = Some(Rc::new(RefCell::new(nvim)));
    }

    pub fn open_file(&self, path: &str) {
        self.state.borrow().open_file(path);
    }

    pub fn cd(&self, path: &str) {
        self.state.borrow().cd(path);
    }

    pub fn detach_ui(&mut self) {
        let state = self.state.borrow();
        state.nvim().ui_detach().expect("Error in ui_detach");
    }

    pub fn edit_paste(&self) {
        let state = self.state.borrow();
        let paste_command = if state.mode == NvimMode::Normal {
            "\"*p"
        } else {
            "<Esc>\"*pa"
        };

        let mut nvim = state.nvim();
        nvim.input(paste_command).report_err(&mut *nvim);
    }

    pub fn edit_save_all(&self) {
        let state = self.state.borrow();
        let mut nvim = &mut *state.nvim();
        nvim.command(":wa").report_err(nvim);
    }
}

fn gtk_focus_in(state: &mut State) -> Inhibit {
    state.cursor.as_mut().unwrap().enter_focus();
    let point = state.model.cur_point();
    state.on_redraw(&RepaintMode::Area(point));
    Inhibit(false)
}

fn gtk_focus_out(state: &mut State) -> Inhibit {
    state.cursor.as_mut().unwrap().leave_focus();
    let point = state.model.cur_point();
    state.on_redraw(&RepaintMode::Area(point));

    Inhibit(false)
}

fn gtk_scroll_event(state: &mut State, ev: &EventScroll) -> Inhibit {
    if !state.mouse_enabled {
        return Inhibit(false);
    }

    state.close_popup_menu();

    match ev.as_ref().direction {
        gdk_sys::GdkScrollDirection::Right => {
            mouse_input(state, "ScrollWheelRight", ev.get_state(), ev.get_position())
        }
        gdk_sys::GdkScrollDirection::Left => {
            mouse_input(state, "ScrollWheelLeft", ev.get_state(), ev.get_position())
        }
        gdk_sys::GdkScrollDirection::Up => {
            mouse_input(state, "ScrollWheelUp", ev.get_state(), ev.get_position())
        }
        gdk_sys::GdkScrollDirection::Down => {
            mouse_input(state, "ScrollWheelDown", ev.get_state(), ev.get_position())
        }
        _ => (),
    }
    Inhibit(false)
}

fn gtk_button_press(shell: &mut State, ui_state: &mut UiState, ev: &EventButton) -> Inhibit {
    if ev.get_event_type() != EventType::ButtonPress {
        return Inhibit(false);
    }

    if shell.mouse_enabled {
        ui_state.mouse_pressed = true;

        mouse_input(shell, "LeftMouse", ev.get_state(), ev.get_position());
    }
    Inhibit(false)
}

fn mouse_input(shell: &mut State, input: &str, state: ModifierType, position: (f64, f64)) {
    if let Some(line_height) = shell.line_height {
        if let Some(char_width) = shell.char_width {

            let mut nvim = shell.nvim();
            let (x, y) = position;
            let col = (x / char_width).trunc() as u64;
            let row = (y / line_height).trunc() as u64;
            let input_str = format!("{}<{},{}>", keyval_to_input_string(input, state), col, row);
            nvim.input(&input_str)
                .expect("Can't send mouse input event");
        }
    }
}

fn gtk_button_release(ui_state: &mut UiState) -> Inhibit {
    ui_state.mouse_pressed = false;
    Inhibit(false)
}

fn gtk_motion_notify(shell: &mut State, ui_state: &mut UiState, ev: &EventMotion) -> Inhibit {
    if shell.mouse_enabled && ui_state.mouse_pressed {
        mouse_input(shell, "LeftDrag", ev.get_state(), ev.get_position());
    }
    Inhibit(false)
}

fn gtk_draw(parent: &ui::Components, state: &mut State, ctx: &cairo::Context) -> Inhibit {
    if state.line_height.is_none() {
        let (width, height) = calc_char_bounds(state, ctx);
        state.line_height = Some(height as f64);
        state.char_width = Some(width as f64);
    }

    draw(state, ctx);
    request_window_resize(parent, state);


    Inhibit(false)
}

#[inline]
fn get_model_clip(state: &State,
                  line_height: f64,
                  char_width: f64,
                  clip: (f64, f64, f64, f64))
                  -> ModelRect {
    let mut model_clip =
        ModelRect::from_area(line_height, char_width, clip.0, clip.1, clip.2, clip.3);
    // in some cases symbols from previous row affect next row
    // for example underscore symbol or 'g'
    // also for italic text it is possible that symbol can affect next one
    // see deference between logical rect and ink rect
    model_clip.extend(1, 0, 1, 0);
    state.model.limit_to_model(&mut model_clip);

    model_clip
}

#[inline]
fn draw_backgound(state: &State,
                  draw_bitmap: &ModelBitamp,
                  ctx: &cairo::Context,
                  line_height: f64,
                  char_width: f64,
                  model_clip: &ModelRect) {
    let line_x = model_clip.left as f64 * char_width;
    let mut line_y: f64 = model_clip.top as f64 * line_height;

    for (line_idx, line) in state.model.clip_model(model_clip) {
        ctx.move_to(line_x, line_y);

        for (col_idx, cell) in line.iter() {
            let current_point = ctx.get_current_point();

            if !draw_bitmap.get(col_idx, line_idx) {
                let (bg, _) = state.colors(cell);
                
                if &state.bg_color != bg {
                    ctx.set_source_rgb(bg.0, bg.1, bg.2);
                    ctx.rectangle(current_point.0, current_point.1, char_width, line_height);
                    ctx.fill();
                }

            }

            ctx.move_to(current_point.0 + char_width, current_point.1);
        }
        line_y += line_height;
    }
}

fn draw(state: &State, ctx: &cairo::Context) {
    let layout = pc::create_layout(ctx);
    let mut desc = state.create_pango_font();
    let mut buf = String::with_capacity(4);

    let (row, col) = state.model.get_cursor();

    let line_height = state.line_height.unwrap();
    let char_width = state.char_width.unwrap();
    let mut draw_bitmap = ModelBitamp::new(state.model.columns, state.model.rows);

    ctx.set_source_rgb(state.bg_color.0, state.bg_color.1, state.bg_color.2);
    ctx.paint();

    let clip_rects = &ctx.copy_clip_rectangle_list().rectangles;
    for clip_idx in 0..clip_rects.len() {
        let clip = clip_rects.get(clip_idx).unwrap();

        let model_clip = get_model_clip(state,
                                        line_height,
                                        char_width,
                                        (clip.x, clip.y, clip.x + clip.width, clip.y + clip.height));

        let line_x = model_clip.left as f64 * char_width;
        let mut line_y: f64 = model_clip.top as f64 * line_height;

        draw_backgound(state, &draw_bitmap, ctx, line_height, char_width, &model_clip);

        for (line_idx, line) in state.model.clip_model(&model_clip) {

            ctx.move_to(line_x, line_y);

            for (col_idx, cell) in line.iter() {
                let current_point = ctx.get_current_point();

                if !draw_bitmap.get(col_idx, line_idx) {
                    let double_width = line.is_double_width(col_idx);

                    let (bg, fg) = state.colors(cell);

                    if row == line_idx && col == col_idx {
                        state
                            .cursor
                            .as_ref()
                            .unwrap()
                            .draw(ctx,
                                  state,
                                  char_width,
                                  line_height,
                                  line_y,
                                  double_width,
                                  bg);

                        ctx.move_to(current_point.0, current_point.1);
                    }


                    if !cell.ch.is_whitespace() {
                        update_font_description(&mut desc, &cell.attrs);

                        layout.set_font_description(Some(&desc));
                        buf.clear();
                        buf.push(cell.ch);
                        layout.set_text(&buf, -1);

                        // correct layout for double_width chars
                        if double_width {
                            let (dw_width, dw_height) = layout.get_pixel_size();
                            let x_offset = (char_width * 2.0 - dw_width as f64) / 2.0;
                            let y_offset = (line_height - dw_height as f64) / 2.0;
                            ctx.rel_move_to(x_offset, y_offset);
                        }

                        ctx.set_source_rgb(fg.0, fg.1, fg.2);
                        pc::update_layout(ctx, &layout);
                        pc::show_layout(ctx, &layout);
                    }

                    if cell.attrs.underline || cell.attrs.undercurl {
                        // [TODO]: Current gtk-rs bindings does not provide fontmetrics access
                        // so it is not possible to find right position for underline or undercurl position
                        // > update_font_description(&mut desc, &cell.attrs);
                        // > layout.get_context().unwrap().get_metrics();
                        let top_offset = line_height * 0.9;

                        let sp = if let Some(ref sp) = cell.attrs.special {
                            sp
                        } else {
                            &state.sp_color
                        };

                        ctx.set_source_rgba(sp.0, sp.1, sp.2, 0.7);
                        if cell.attrs.undercurl {
                            ctx.set_dash(&[4.0, 2.0], 0.0);
                            ctx.set_line_width(2.0);
                            ctx.move_to(current_point.0, line_y + top_offset);
                            ctx.line_to(current_point.0 + char_width, line_y + top_offset);
                            ctx.stroke();
                            ctx.set_dash(&[], 0.0);
                        } else if cell.attrs.underline {
                            ctx.set_line_width(1.0);
                            ctx.move_to(current_point.0, line_y + top_offset);
                            ctx.line_to(current_point.0 + char_width, line_y + top_offset);
                            ctx.stroke();
                        }
                    }
                }

                ctx.move_to(current_point.0 + char_width, current_point.1);
            }

            line_y += line_height;
        }

        draw_bitmap.fill_from_model(&model_clip);
    }
}

#[inline]
fn update_font_description(desc: &mut FontDescription, attrs: &Attrs) {
    desc.unset_fields(pango::FONT_MASK_STYLE | pango::FONT_MASK_WEIGHT);
    if attrs.italic {
        desc.set_style(pango::Style::Italic);
    }
    if attrs.bold {
        desc.set_weight(pango::Weight::Bold);
    }
}

fn calc_char_bounds(shell: &State, ctx: &cairo::Context) -> (i32, i32) {
    let layout = pc::create_layout(ctx);

    let desc = shell.create_pango_font();
    layout.set_font_description(Some(&desc));
    layout.set_text("A", -1);

    layout.get_pixel_size()
}

fn request_window_resize(parent: &ui::Components, state: &mut State) {
    if !state.request_resize {
        return;
    }
    if state.resize_timer.is_some() {
        return;
    }

    state.request_resize = false;

    let width = state.drawing_area.get_allocated_width();
    let height = state.drawing_area.get_allocated_height();
    let request_height = (state.model.rows as f64 * state.line_height.unwrap()) as i32;
    let request_width = (state.model.columns as f64 * state.char_width.unwrap()) as i32;

    if width != request_width || height != request_height {
        let window = parent.window();
        let (win_width, win_height) = window.get_size();
        let h_border = win_width - width;
        let v_border = win_height - height;
        window.resize(request_width + h_border, request_height + v_border);
    }
}

fn split_color(indexed_color: u64) -> Color {
    let r = ((indexed_color >> 16) & 0xff) as f64;
    let g = ((indexed_color >> 8) & 0xff) as f64;
    let b = (indexed_color & 0xff) as f64;
    Color(r / 255.0, g / 255.0, b / 255.0)
}

fn gtk_configure_event(state: &Arc<UiMutex<State>>, ev: &EventConfigure) -> bool {
    let (width, height) = ev.get_size();

    let mut state_ref = state.borrow_mut();

    if let Some(timer) = state_ref.resize_timer {
        glib::source_remove(timer);
    }
    if let Some(line_height) = state_ref.line_height {
        if let Some(char_width) = state_ref.char_width {

            let state = state.clone();
            state_ref.resize_timer = Some(glib::timeout_add(250, move || {
                let mut state_ref = state.borrow_mut();

                state_ref.resize_timer = None;

                let rows = (height as f64 / line_height).trunc() as usize;
                let columns = (width as f64 / char_width).trunc() as usize;
                if state_ref.model.rows != rows || state_ref.model.columns != columns {
                    if let Err(err) = state_ref
                           .nvim()
                           .ui_try_resize(columns as u64, rows as u64) {
                        println!("Error trying resize nvim {}", err);
                    }
                }
                Continue(false)
            }));
        }
    }
    false
}

impl RedrawEvents for State {
    fn on_cursor_goto(&mut self, row: u64, col: u64) -> RepaintMode {
        RepaintMode::AreaList(self.model.set_cursor(row as usize, col as usize))
    }

    fn on_put(&mut self, text: &str) -> RepaintMode {
        RepaintMode::Area(self.model.put(text, self.cur_attrs.as_ref()))
    }

    fn on_clear(&mut self) -> RepaintMode {
        self.model.clear();
        RepaintMode::All
    }

    fn on_eol_clear(&mut self) -> RepaintMode {
        RepaintMode::Area(self.model.eol_clear())
    }

    fn on_resize(&mut self, columns: u64, rows: u64) -> RepaintMode {
        self.model = UiModel::new(rows, columns);
        self.request_resize();
        RepaintMode::All
    }

    fn on_redraw(&self, mode: &RepaintMode) {
        match mode {
            &RepaintMode::All => self.drawing_area.queue_draw(),
            &RepaintMode::Area(ref rect) => self.queue_draw_area(&vec![rect]),
            &RepaintMode::AreaList(ref list) => self.queue_draw_area(&list.list),
            &RepaintMode::Nothing => (),
        }
    }

    fn on_set_scroll_region(&mut self, top: u64, bot: u64, left: u64, right: u64) -> RepaintMode {
        self.model.set_scroll_region(top, bot, left, right);
        RepaintMode::Nothing
    }

    fn on_scroll(&mut self, count: i64) -> RepaintMode {
        RepaintMode::Area(self.model.scroll(count))
    }

    fn on_highlight_set(&mut self, attrs: &Vec<(Value, Value)>) -> RepaintMode {
        let mut model_attrs = Attrs::new();

        for &(ref key_val, ref val) in attrs {
            if let Some(key) = key_val.as_str() {
                match key {
                    "foreground" => {
                        if let Some(fg) = val.as_u64() {
                            model_attrs.foreground = Some(split_color(fg));
                        }
                    }
                    "background" => {
                        if let Some(bg) = val.as_u64() {
                            model_attrs.background = Some(split_color(bg));
                        }
                    }
                    "special" => {
                        if let Some(bg) = val.as_u64() {
                            model_attrs.special = Some(split_color(bg));
                        }
                    }
                    "reverse" => model_attrs.reverse = true,
                    "bold" => model_attrs.bold = true,
                    "italic" => model_attrs.italic = true,
                    "underline" => model_attrs.underline = true,
                    "undercurl" => model_attrs.undercurl = true,
                    attr_key => println!("unknown attribute {}", attr_key),
                };
            } else {
                panic!("attr key must be string");
            }
        }

        self.cur_attrs = Some(model_attrs);
        RepaintMode::Nothing
    }

    fn on_update_bg(&mut self, bg: i64) -> RepaintMode {
        if bg >= 0 {
            self.bg_color = split_color(bg as u64);
        } else {
            self.bg_color = COLOR_BLACK;
        }
        RepaintMode::Nothing
    }

    fn on_update_fg(&mut self, fg: i64) -> RepaintMode {
        if fg >= 0 {
            self.fg_color = split_color(fg as u64);
        } else {
            self.fg_color = COLOR_WHITE;
        }
        RepaintMode::Nothing
    }

    fn on_update_sp(&mut self, sp: i64) -> RepaintMode {
        if sp >= 0 {
            self.sp_color = split_color(sp as u64);
        } else {
            self.sp_color = COLOR_RED;
        }
        RepaintMode::Nothing
    }

    fn on_mode_change(&mut self, mode: &str) -> RepaintMode {
        match mode {
            "normal" => self.mode = NvimMode::Normal,
            "insert" => self.mode = NvimMode::Insert,
            _ => self.mode = NvimMode::Other,
        }

        RepaintMode::Area(self.model.cur_point())
    }

    fn on_mouse(&mut self, on: bool) -> RepaintMode {
        self.mouse_enabled = on;
        RepaintMode::Nothing
    }

    fn on_busy(&mut self, busy: bool) -> RepaintMode {
        if busy {
            self.cursor.as_mut().unwrap().busy_on();
        } else {
            self.cursor.as_mut().unwrap().busy_off();
        }
        RepaintMode::Area(self.model.cur_point())
    }

    fn popupmenu_show(&mut self,
                      menu: &Vec<Vec<&str>>,
                      selected: i64,
                      row: u64,
                      col: u64)
                      -> RepaintMode {
        match (&self.line_height, &self.char_width) {
            (&Some(line_height), &Some(char_width)) => {
                let point = ModelRect::point(col as usize, row as usize);
                let (x, y, width, height) = point.to_area(line_height, char_width);

                self.popup_menu = Some(PopupMenu::new(
                        &self.drawing_area,
                        self.nvim.as_ref().unwrap().clone(),
                                                      &self.font_desc,
                                                      menu,
                                                      selected,
                                                      x,
                                                      y,
                                                      width,
                                                      height));
                self.popup_menu.as_ref().unwrap().show();
            }
            _ => (),
        };

        RepaintMode::Nothing
    }

    fn popupmenu_hide(&mut self) -> RepaintMode {
        self.popup_menu.take().unwrap().hide();
        RepaintMode::Nothing
    }

    fn popupmenu_select(&mut self, selected: i64) -> RepaintMode {
        self.popup_menu.as_mut().unwrap().select(selected);
        RepaintMode::Nothing
    }
}

impl GuiApi for State {
    fn set_font(&mut self, font_desc: &str) {
        self.set_font_desc(font_desc);

        let mut settings = self.settings.borrow_mut();
        settings.set_font_source(FontSource::Rpc);
    }
}

pub struct ModelBitamp {
    words_for_cols: usize,
    model: Vec<u64>,
}

impl ModelBitamp {
    pub fn new(cols: usize, rows: usize) -> ModelBitamp {
        let words_for_cols = cols / 64 + 1;

        ModelBitamp { 
            words_for_cols: words_for_cols,
            model: vec![0; rows * words_for_cols],
        }
    }

    fn fill_from_model(&mut self, rect: &ModelRect) {
        for row in rect.top..rect.bot + 1 {
            let row_pos = self.words_for_cols * row;
            for col in rect.left..rect.right + 1 {
                let col_pos = col / 64;
                let col_offset = col % 64;
                self.model[row_pos + col_pos] |= 1 << col_offset;
            }
        }
    }

    #[inline]
    fn get(&self, col: usize, row: usize) -> bool {
        let row_pos = self.words_for_cols * row;
        let col_pos = col / 64;
        let col_offset = col % 64;
        self.model[row_pos + col_pos] & (1 << col_offset) != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bitmap() {
        let mut bitmap = ModelBitamp::new(80, 24);
        bitmap.fill_from_model(&ModelRect::new(22, 22, 63, 68));

        assert_eq!(true, bitmap.get(63, 22));
        assert_eq!(true, bitmap.get(68, 22));
        assert_eq!(false, bitmap.get(62, 22));
    }
}

