mod logging;

use std::{
    hash::{Hash, Hasher},
    mem::transmute,
    sync::LazyLock,
    time::Duration,
};

use eldenring::{
    cs::{CSCamera, CSEzDraw, CSWindowImp, CSWindowType, EzDrawTextCoordMode, RendMan},
    util::system::wait_for_system_init,
};
use fromsoftware_shared::{F32Vector2, F32Vector4, FromStatic, Program};
use nalgebra::Vector3;

use crate::logging::{custom_panic_hook, setup_logging};
use crossbeam_queue::ArrayQueue;
use hudhook::{
    Hudhook, ImguiRenderLoop, RenderContext,
    imgui::{self, Ui},
    windows::Win32::{
        Foundation::HINSTANCE,
        System::{LibraryLoader::DisableThreadLibraryCalls, SystemServices::DLL_PROCESS_ATTACH},
    },
};
use hudhook::{hooks::dx12::ImguiDx12Hooks, imgui::Context};
use pelite::pe::Pe;
use retour::static_detour;

static TEXT_RENDER_QUEUE: LazyLock<ArrayQueue<DrawCommand>> =
    LazyLock::new(|| ArrayQueue::new(1024 * 10));

const BASE_IMGUI_FONT_SIZE_PX: f32 = 24.0;

#[derive(Debug)]
enum DrawCommand {
    Text(String, f32, f32, f32, EzDrawTextCoordMode),
    SetOffset(f32, f32),
}

fn u16_ptr_to_string(ptr: *const u16) -> String {
    let len = (0..)
        .take_while(|&i| unsafe { *ptr.offset(i) } != 0)
        .count();
    let slice = unsafe { std::slice::from_raw_parts(ptr, len) };

    String::from_utf16(slice).unwrap_or(String::from("?EncodingError?"))
}

// void CS::CSEzDraw::DrawText(CSEzDraw *param_1,FloatVector4 *param_2,wchar_t *param_3)
const TEXT_RENDER_REQUEST_RVA: u32 = 0x264efc0;
// void CS::CSEzDraw::DrawTextWithOffset(CSEzDraw *param_1,FloatVector4 *param_2,float (*offset) [2],wchar_t *param_4)
const DRAW_TEXT_WITH_OFFSET_RVA: u32 = 0x264ef20;

static_detour! {
    static DrawTextRenderRequest: unsafe extern "C" fn(*mut CSEzDraw, *mut F32Vector4, *const u16) -> ();
    static DrawTextWithOffset: unsafe extern "C" fn(*mut CSEzDraw, *mut F32Vector4, *mut F32Vector2, *const u16) -> ();
}

struct DebugTextRender {
    offset: (f32, f32),
}
impl DebugTextRender {
    fn new() -> Self {
        Self { offset: (0.0, 0.0) }
    }

    fn window_size() -> (f32, f32) {
        unsafe { CSWindowImp::instance() }
            .map(|w| (w.screen_width as f32, w.screen_height as f32))
            .unwrap_or((1920.0, 1080.0))
    }

    fn window_resolution() -> (f32, f32) {
        if let Ok(window) = unsafe { CSWindowImp::instance() } {
            match window.persistent_window_config.window_type {
                CSWindowType::Windowed => (
                    window.persistent_window_config.windowed_screen_width as f32,
                    window.persistent_window_config.windowed_screen_height as f32,
                ),
                CSWindowType::Fullscreen => (
                    window.persistent_window_config.fullscreen_width as f32,
                    window.persistent_window_config.fullscreen_height as f32,
                ),
                CSWindowType::Borderless => (
                    window.persistent_window_config.borderless_screen_width as f32,
                    window.persistent_window_config.borderless_screen_height as f32,
                ),
            }
        } else {
            (1920.0, 1080.0)
        }
    }
}

impl ImguiRenderLoop for DebugTextRender {
    fn initialize(&mut self, ctx: &mut Context, _render_context: &mut dyn RenderContext) {
        let font_data = std::fs::read("C:\\Windows\\Fonts\\msgothic.ttc")
            .expect("Failed to read font file (msgothic.ttc)");
        let glyph_ranges = imgui::FontGlyphRanges::from_slice(&[
            0x0020, 0x00FF, // Basic Latin + Latin Supplement
            0x3000, 0x30FF, // Japanese punctuation, Hiragana, Katakana
            0x31F0, 0x31FF, // Katakana Phonetic Extensions
            0x3400, 0x4DBF, // CJK Unified Ideographs Extension A
            0x4E00, 0x9FFF, // CJK Unified Ideographs
            0xF900, 0xFAFF, // CJK Compatibility Ideographs
            0xFF00, 0xFFEF, // Halfwidth and Fullwidth Forms
            0x2500, 0x257F, // Box Drawing
            0x2580, 0x259F, // Block Elements (includes ■)
            0x25A0, 0x25FF, // Geometric Shapes (includes ■ specifically)
            0,
        ]);
        ctx.fonts().add_font(&[imgui::FontSource::TtfData {
            data: &font_data,
            size_pixels: BASE_IMGUI_FONT_SIZE_PX,
            config: Some(imgui::FontConfig {
                oversample_h: 3,
                oversample_v: 1,
                pixel_snap_h: true,
                glyph_ranges,
                ..Default::default()
            }),
        }]);
        ctx.fonts().build_alpha8_texture();
    }

    fn render(&mut self, ui: &mut Ui) {
        // Workaround for crash on empty render queue
        ui.window("_")
            .size([1.0, 1.0], imgui::Condition::FirstUseEver)
            .position([0.0, 0.0], imgui::Condition::FirstUseEver)
            .no_decoration()
            .draw_background(false)
            .no_inputs()
            .resizable(false)
            .movable(false)
            .collapsible(false)
            .title_bar(false)
            .build(|| ui.text("."));
        let Ok(buffer) =
            (unsafe { RendMan::instance().map(|rm| rm.debug_ez_draw.current_buffer()) })
        else {
            return;
        };
        let state = &buffer.ez_draw_state.base;
        while let Some(event) = TEXT_RENDER_QUEUE.pop() {
            match event {
                DrawCommand::SetOffset(x, y) => {
                    self.offset = (x, y);
                }
                DrawCommand::Text(text, x, y, z, render_mode) => {
                    let (new_x, new_y) = match render_mode {
                        EzDrawTextCoordMode::HavokPosition2
                        | EzDrawTextCoordMode::HavokPosition3 => {
                            let camera = unsafe { CSCamera::instance() }.unwrap();
                            let cam = &camera.pers_cam_1;

                            let cam_right = cam.right();
                            let cam_up = cam.up();
                            let cam_forward = cam.forward();
                            let cam_pos_h = cam.position();

                            let cam_right_v = Vector3::new(cam_right.0, cam_right.1, cam_right.2);
                            let cam_up_v = Vector3::new(cam_up.0, cam_up.1, cam_up.2);
                            let cam_forward_v =
                                Vector3::new(cam_forward.0, cam_forward.1, cam_forward.2);
                            let cam_pos = Vector3::new(cam_pos_h.0, cam_pos_h.1, cam_pos_h.2);

                            let world_pos = Vector3::new(x, y, z);
                            let rel = world_pos - cam_pos;

                            let z_cam = cam_forward_v.dot(&rel);
                            if z_cam <= 0.0 {
                                (f32::NAN, f32::NAN)
                            } else {
                                let x_cam = cam_right_v.dot(&rel);
                                let y_cam = cam_up_v.dot(&rel);

                                let fov_rad = cam.fov;
                                let m11 = 1.0 / (0.5 * fov_rad).tan();
                                let m00 = m11 / cam.aspect_ratio;

                                let ndc_x = x_cam * m00 / z_cam;
                                let ndc_y = y_cam * m11 / z_cam;

                                let screen_size = Self::window_size();

                                let screen_x = (ndc_x * 0.5 + 0.5) * screen_size.0;
                                let screen_y = (ndc_y * -0.5 + 0.5) * screen_size.1;

                                (screen_x, screen_y)
                            }
                        }
                        EzDrawTextCoordMode::ScreenSpace0 | EzDrawTextCoordMode::ScreenSpace1 => {
                            let resolution = Self::window_resolution();
                            let size = Self::window_size();
                            let scale_x = size.0 / resolution.0;
                            let scale_y = size.1 / resolution.1;
                            (x * scale_x, y * scale_y)
                        }
                        EzDrawTextCoordMode::Normalized4k => {
                            let screen_size = Self::window_resolution();
                            let diff_x: f32 = screen_size.0 / 3840.0;
                            let diff_y: f32 = screen_size.1 / 2160.0;
                            (x * diff_x, y * diff_y)
                        }
                        EzDrawTextCoordMode::Normalized1080p => {
                            let screen_size = Self::window_resolution();
                            let diff_x: f32 = screen_size.0 / 1920.0;
                            let diff_y: f32 = screen_size.1 / 1080.0;
                            (x * diff_x, y * diff_y)
                        }
                    };
                    if !new_x.is_finite() || !new_y.is_finite() {
                        continue;
                    }

                    let offset_x = new_x + self.offset.0;
                    let offset_y = new_y + self.offset.1;
                    self.offset = (0.0, 0.0);

                    tracing::debug!(
                        "Rendering text '{}' at screen position ({}, {})",
                        text,
                        offset_x,
                        offset_y
                    );

                    // Hash the coordinates and text to create a unique window name
                    let mut hasher = std::collections::hash_map::DefaultHasher::new();
                    (x as u32).hash(&mut hasher);
                    (y as u32).hash(&mut hasher);
                    (offset_x as u32).hash(&mut hasher);
                    (offset_y as u32).hash(&mut hasher);
                    text.hash(&mut hasher);
                    let _guard = ui.push_id(hasher.finish().to_string());
                    let window_size = Self::window_size();
                    ui.window(format!("text_window_{x}_{y}"))
                        .size([window_size.0, window_size.1], imgui::Condition::Always)
                        .position([offset_x, offset_y], imgui::Condition::Always)
                        .no_decoration()
                        .focus_on_appearing(false)
                        .focused(false)
                        .draw_background(false)
                        .no_inputs()
                        .resizable(false)
                        .movable(false)
                        .collapsible(false)
                        .title_bar(false)
                        .build(|| {
                            // Normalize color from [0-255] to [0.0-1.0]
                            let text_color = state.text_color;
                            let _ = ui.push_style_color(
                                imgui::StyleColor::Text,
                                [
                                    text_color.r() as f32 / 255.0,
                                    text_color.g() as f32 / 255.0,
                                    text_color.b() as f32 / 255.0,
                                    text_color.a() as f32 / 255.0,
                                ],
                            );

                            // state.font_size is the pixel size the game wants (e.g., 18.0)
                            // BASE_IMGUI_FONT_SIZE_PX is the size the font was loaded at (24.0)
                            // Multiply by text_pos_height_scale to match game's resolution scaling
                            let font_scale = state.font_size / BASE_IMGUI_FONT_SIZE_PX;

                            ui.set_window_font_scale(font_scale);
                            ui.text(text);
                        });
                }
            }
        }
    }
}

fn init() {
    setup_logging();

    std::panic::set_hook(Box::new(custom_panic_hook));
    let program = Program::current();
    let text_request_va = program.rva_to_va(TEXT_RENDER_REQUEST_RVA).unwrap();
    unsafe {
        DrawTextRenderRequest
            .initialize(
                transmute::<u64, unsafe extern "C" fn(*mut CSEzDraw, *mut F32Vector4, *const u16)>(
                    text_request_va,
                ),
                |ez_draw: *mut CSEzDraw, pos: *mut F32Vector4, text: *const u16| {
                    let text_str = u16_ptr_to_string(text);
                    let x = (*pos).0;
                    let y = (*pos).1;
                    let z = (*pos).2;
                    let render_mode = (*ez_draw)
                        .current_buffer()
                        .ez_draw_state
                        .base
                        .text_coord_mode;
                    tracing::debug!(
                        "DrawTextRenderRequest: {:?},  {}, {:?}",
                        render_mode,
                        text_str,
                        *pos
                    );

                    TEXT_RENDER_QUEUE.force_push(DrawCommand::Text(text_str, x, y, z, render_mode));
                },
            )
            .unwrap()
            .enable()
            .unwrap();
    }
    let draw_text_with_offset_va = program.rva_to_va(DRAW_TEXT_WITH_OFFSET_RVA).unwrap();
    unsafe {
        DrawTextWithOffset
            .initialize(
                transmute::<
                    u64,
                    unsafe extern "C" fn(
                        *mut CSEzDraw,
                        *mut F32Vector4,
                        *mut F32Vector2,
                        *const u16,
                    ),
                >(draw_text_with_offset_va),
                |ez_draw: *mut CSEzDraw,
                 pos: *mut F32Vector4,
                 offset: *mut F32Vector2,
                 text: *const u16| {
                    TEXT_RENDER_QUEUE.force_push(DrawCommand::SetOffset((*offset).0, (*offset).1));
                    let text_str = u16_ptr_to_string(text);
                    let x = (*pos).0;
                    let y = (*pos).1;
                    let z = (*pos).2;

                    let current_buffer = (*ez_draw).current_buffer();

                    let render_mode = current_buffer.ez_draw_state.base.text_coord_mode;
                    tracing::debug!(
                        "DrawTextWithOffset: {:?},  {}, {:?}, {:?}",
                        render_mode,
                        text_str,
                        *pos,
                        *offset
                    );

                    TEXT_RENDER_QUEUE.force_push(DrawCommand::Text(text_str, x, y, z, render_mode));
                },
            )
            .unwrap()
            .enable()
            .unwrap();
    }

    std::thread::spawn(|| {
        let program = Program::current();
        wait_for_system_init(&program, Duration::MAX).expect("System initialization timed out");

        if let Err(e) = Hudhook::builder()
            .with::<ImguiDx12Hooks>(DebugTextRender::new())
            .build()
            .apply()
        {
            tracing::error!("Failed to apply ImGui hooks: {:?}", e);
        }
    });
}

/// DLL entry point function.
///
/// # Safety
/// This function is safe to call when it's invoked by the Windows loader with valid parameters
/// during DLL loading, unloading, and thread attach/detach events.
#[unsafe(no_mangle)]
#[allow(non_snake_case)]
pub unsafe extern "C" fn DllMain(hinst: HINSTANCE, reason: u32, _reserved: usize) -> bool {
    if reason == DLL_PROCESS_ATTACH {
        unsafe { DisableThreadLibraryCalls(hinst).ok() };

        LazyLock::force(&TEXT_RENDER_QUEUE);
        init();
    };
    true
}
