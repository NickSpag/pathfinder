// pathfinder/demo/src/main.rs
//
// Copyright © 2019 The Pathfinder Project Developers.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use clap::{App, Arg};
use euclid::Size2D;
use jemallocator;
use pathfinder_geometry::basic::point::{Point2DF32, Point2DI32, Point3DF32};
use pathfinder_geometry::basic::rect::{RectF32, RectI32};
use pathfinder_geometry::basic::transform2d::Transform2DF32;
use pathfinder_geometry::basic::transform3d::{Perspective, Transform3DF32};
use pathfinder_gl::debug::{BUTTON_HEIGHT, BUTTON_TEXT_OFFSET, BUTTON_WIDTH, DebugUI, PADDING};
use pathfinder_gl::debug::{TEXT_COLOR, WINDOW_COLOR};
use pathfinder_gl::device::Texture;
use pathfinder_gl::renderer::Renderer;
use pathfinder_renderer::builder::{RenderOptions, RenderTransform, SceneBuilder};
use pathfinder_renderer::gpu_data::BuiltScene;
use pathfinder_renderer::paint::ColorU;
use pathfinder_renderer::post::{DEFRINGING_KERNEL_CORE_GRAPHICS, STEM_DARKENING_FACTORS};
use pathfinder_renderer::scene::Scene;
use pathfinder_renderer::z_buffer::ZBuffer;
use pathfinder_svg::SceneExt;
use rayon::ThreadPoolBuilder;
use sdl2::{EventPump, Sdl, VideoSubsystem};
use sdl2::event::{Event, WindowEvent};
use sdl2::keyboard::Keycode;
use sdl2::video::{GLContext, GLProfile, Window};
use std::f32::consts::FRAC_PI_4;
use std::panic;
use std::path::PathBuf;
use std::process;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};
use usvg::{Options as UsvgOptions, Tree};

#[global_allocator]
static ALLOC: jemallocator::Jemalloc = jemallocator::Jemalloc;

const MAIN_FRAMEBUFFER_WIDTH: u32 = 1067;
const MAIN_FRAMEBUFFER_HEIGHT: u32 = 800;

const MOUSELOOK_ROTATION_SPEED: f32 = 0.007;
const CAMERA_VELOCITY: f32 = 25.0;

const BACKGROUND_COLOR: ColorU = ColorU { r: 32, g: 32, b: 32, a: 255 };

const EFFECTS_WINDOW_WIDTH: i32 = 550;
const EFFECTS_WINDOW_HEIGHT: i32 = BUTTON_HEIGHT * 3 + PADDING * 4;

const SWITCH_SIZE: i32 = SWITCH_HALF_SIZE * 2 + 1;
const SWITCH_HALF_SIZE: i32 = 96;

const APPROX_FONT_SIZE: f32 = 16.0;

const WORLD_SCALE: f32 = 1.0 / 800.0;

static EFFECTS_PNG_NAME: &'static str = "demo-effects";
static OPEN_PNG_NAME: &'static str = "demo-open";

fn main() {
    DemoApp::new().run();
}

struct DemoApp {
    window: Window,
    #[allow(dead_code)]
    sdl_context: Sdl,
    #[allow(dead_code)]
    sdl_video: VideoSubsystem,
    sdl_event_pump: EventPump,
    #[allow(dead_code)]
    gl_context: GLContext,

    scale_factor: f32,

    camera_position: Point3DF32,
    camera_velocity: Point3DF32,
    camera_yaw: f32,
    camera_pitch: f32,

    frame_counter: u32,
    events: Vec<Event>,
    exit: bool,
    mouselook_enabled: bool,
    ui_event_handled_last_frame: bool,

    ui: DemoUI,
    scene_thread_proxy: SceneThreadProxy,
    renderer: Renderer,
}

impl DemoApp {
    fn new() -> DemoApp {
        let options = Options::get();

        let sdl_context = sdl2::init().unwrap();
        let sdl_video = sdl_context.video().unwrap();

        let gl_attributes = sdl_video.gl_attr();
        gl_attributes.set_context_profile(GLProfile::Core);
        gl_attributes.set_context_version(3, 3);

        let window =
            sdl_video.window("Pathfinder Demo", MAIN_FRAMEBUFFER_WIDTH, MAIN_FRAMEBUFFER_HEIGHT)
                    .opengl()
                    .resizable()
                    .allow_highdpi()
                    .build()
                    .unwrap();

        let gl_context = window.gl_create_context().unwrap();
        gl::load_with(|name| sdl_video.gl_get_proc_address(name) as *const _);

        let sdl_event_pump = sdl_context.event_pump().unwrap();

        let (window_width, _) = window.size();
        let (drawable_width, drawable_height) = window.drawable_size();
        let drawable_size = Size2D::new(drawable_width, drawable_height);

        let base_scene = load_scene(&options);
        let scene_thread_proxy = SceneThreadProxy::new(base_scene, options.clone());
        scene_thread_proxy.set_drawable_size(&drawable_size);

        DemoApp {
            window,
            sdl_context,
            sdl_video,
            sdl_event_pump,
            gl_context,

            scale_factor: drawable_width as f32 / window_width as f32,

            camera_position: Point3DF32::new(500.0, 500.0, 3000.0, 1.0),
            camera_velocity: Point3DF32::new(0.0, 0.0, 0.0, 1.0),
            camera_yaw: 0.0,
            camera_pitch: 0.0,

            frame_counter: 0,
            events: vec![],
            exit: false,
            mouselook_enabled: false,
            ui_event_handled_last_frame: false,

            ui: DemoUI::new(options),
            scene_thread_proxy,
            renderer: Renderer::new(&drawable_size),
        }
    }

    fn run(&mut self) {
        while !self.exit {
            // Update the scene.
            self.build_scene();

            // Handle events.
            // FIXME(pcwalton): This can cause us to miss UI events if things get backed up...
            let ui_event = self.handle_events();

            // Draw the scene.
            let render_msg = self.scene_thread_proxy.receiver.recv().unwrap();
            self.draw_scene(render_msg, ui_event);
        }
    }

    fn build_scene(&mut self) {
        let (drawable_width, drawable_height) = self.window.drawable_size();
        let drawable_size = Size2D::new(drawable_width, drawable_height);

        let perspective = if self.ui.threed_enabled {
            let rotation = Transform3DF32::from_rotation(-self.camera_yaw,
                                                         -self.camera_pitch,
                                                         0.0);
            self.camera_position = self.camera_position +
                rotation.transform_point(self.camera_velocity);

            let aspect = drawable_size.width as f32 / drawable_size.height as f32;
            let mut transform = Transform3DF32::from_perspective(FRAC_PI_4, aspect, 0.025, 100.0);

            transform = transform.post_mul(&Transform3DF32::from_scale(WORLD_SCALE,
                                                                       WORLD_SCALE,
                                                                       WORLD_SCALE));
            transform = transform.post_mul(&Transform3DF32::from_rotation(self.camera_yaw,
                                                                          self.camera_pitch,
                                                                          0.0));
            let translation = self.camera_position.scale(-1.0);
            transform = transform.post_mul(&Transform3DF32::from_translation(translation.x(),
                                                                             translation.y(),
                                                                             translation.z()));

            Some(Perspective::new(&transform, &drawable_size))
        } else {
            None
        };

        let count = if self.frame_counter == 0 { 2 } else { 1 };
        for _ in 0..count {
            self.scene_thread_proxy.sender.send(MainToSceneMsg::Build(BuildOptions {
                perspective,
                stem_darkening_font_size: if self.ui.stem_darkening_effect_enabled {
                    Some(APPROX_FONT_SIZE * self.scale_factor)
                } else {
                    None
                },
            })).unwrap();
        }
    }

    fn handle_events(&mut self) -> UIEvent {
        let mut ui_event = UIEvent::None;

        let wait_for_event = !self.camera_velocity.is_zero() && self.frame_counter >= 2 &&
            !self.ui_event_handled_last_frame;
        if wait_for_event {
            self.events.push(self.sdl_event_pump.wait_event());
        }
        for event in self.sdl_event_pump.poll_iter() {
            self.events.push(event);
        }

        for event in self.events.drain(..) {
            match event {
                Event::Quit { .. } |
                Event::KeyDown { keycode: Some(Keycode::Escape), .. } => {
                    self.exit = true;
                }
                Event::Window { win_event: WindowEvent::SizeChanged(..), .. } => {
                    let (drawable_width, drawable_height) = self.window.drawable_size();
                    let drawable_size = Size2D::new(drawable_width as u32,
                                                    drawable_height as u32);
                    self.scene_thread_proxy.set_drawable_size(&drawable_size);
                    self.renderer.set_main_framebuffer_size(&drawable_size);
                }
                Event::MouseButtonDown { x, y, .. } => {
                    let point = Point2DI32::new(x, y).scale(self.scale_factor as i32);
                    ui_event = UIEvent::MouseDown(point);
                }
                Event::MouseMotion { xrel, yrel, .. } if self.mouselook_enabled => {
                    self.camera_yaw += xrel as f32 * MOUSELOOK_ROTATION_SPEED;
                    self.camera_pitch -= yrel as f32 * MOUSELOOK_ROTATION_SPEED;
                }
                Event::KeyDown { keycode: Some(Keycode::W), .. } => {
                    self.camera_velocity.set_z(-CAMERA_VELOCITY)
                }
                Event::KeyDown { keycode: Some(Keycode::S), .. } => {
                    self.camera_velocity.set_z(CAMERA_VELOCITY)
                }
                Event::KeyDown { keycode: Some(Keycode::A), .. } => {
                    self.camera_velocity.set_x(-CAMERA_VELOCITY)
                }
                Event::KeyDown { keycode: Some(Keycode::D), .. } => {
                    self.camera_velocity.set_x(CAMERA_VELOCITY)
                }
                Event::KeyUp { keycode: Some(Keycode::W), .. } |
                Event::KeyUp { keycode: Some(Keycode::S), .. } => {
                    self.camera_velocity.set_z(0.0);
                }
                Event::KeyUp { keycode: Some(Keycode::A), .. } |
                Event::KeyUp { keycode: Some(Keycode::D), .. } => {
                    self.camera_velocity.set_x(0.0);
                }
                _ => continue,
            }
        }

        ui_event
    }

    fn draw_scene(&mut self, render_msg: SceneToMainMsg, mut ui_event: UIEvent) {
        let SceneToMainMsg::Render { built_scene, tile_time } = render_msg;

        unsafe {
            gl::BindFramebuffer(gl::FRAMEBUFFER, 0);
            gl::ClearColor(BACKGROUND_COLOR.r as f32 / 255.0,
                           BACKGROUND_COLOR.g as f32 / 255.0,
                           BACKGROUND_COLOR.b as f32 / 255.0,
                           BACKGROUND_COLOR.a as f32 / 255.0);
            gl::Clear(gl::COLOR_BUFFER_BIT);

            if self.ui.gamma_correction_effect_enabled {
                self.renderer.enable_gamma_correction(BACKGROUND_COLOR);
            } else {
                self.renderer.disable_gamma_correction();
            }

            if self.ui.subpixel_aa_effect_enabled {
                self.renderer.enable_subpixel_aa(&DEFRINGING_KERNEL_CORE_GRAPHICS);
            } else {
                self.renderer.disable_subpixel_aa();
            }

            self.renderer.render_scene(&built_scene);

            let rendering_time = self.renderer.shift_timer_query();
            self.renderer.debug_ui.add_sample(tile_time, rendering_time);
            self.renderer.debug_ui.draw();

            let had_ui_event = ui_event.is_none();
            self.ui.update(&mut self.renderer.debug_ui, &mut ui_event);
            self.ui_event_handled_last_frame = had_ui_event && ui_event.is_none();

            // If nothing handled the mouse-down event, toggle mouselook.
            if let UIEvent::MouseDown(_) = ui_event {
                self.mouselook_enabled = !self.mouselook_enabled;
            }
        }

        self.window.gl_swap_window();
        self.frame_counter += 1;
    }
}

struct SceneThreadProxy {
    sender: Sender<MainToSceneMsg>,
    receiver: Receiver<SceneToMainMsg>,
}

impl SceneThreadProxy {
    fn new(scene: Scene, options: Options) -> SceneThreadProxy {
        let (main_to_scene_sender, main_to_scene_receiver) = mpsc::channel();
        let (scene_to_main_sender, scene_to_main_receiver) = mpsc::channel();
        SceneThread::new(scene, scene_to_main_sender, main_to_scene_receiver, options);
        SceneThreadProxy { sender: main_to_scene_sender, receiver: scene_to_main_receiver }
    }

    fn set_drawable_size(&self, drawable_size: &Size2D<u32>) {
        self.sender.send(MainToSceneMsg::SetDrawableSize(*drawable_size)).unwrap();
    }
}

struct SceneThread {
    scene: Scene,
    sender: Sender<SceneToMainMsg>,
    receiver: Receiver<MainToSceneMsg>,
    options: Options,
}

impl SceneThread {
    fn new(scene: Scene,
           sender: Sender<SceneToMainMsg>,
           receiver: Receiver<MainToSceneMsg>,
           options: Options) {
        thread::spawn(move || (SceneThread { scene, sender, receiver, options }).run());
    }

    fn run(mut self) {
        while let Ok(msg) = self.receiver.recv() {
            match msg {
                MainToSceneMsg::SetDrawableSize(size) => {
                    self.scene.view_box =
                        RectF32::new(Point2DF32::default(),
                                     Point2DF32::new(size.width as f32, size.height as f32));
                }
                MainToSceneMsg::Build(build_options) => {
                    let start_time = Instant::now();
                    let built_scene = build_scene(&self.scene, build_options, self.options.jobs);
                    let tile_time = Instant::now() - start_time;
                    self.sender.send(SceneToMainMsg::Render { built_scene, tile_time }).unwrap();
                }
            }
        }
    }
}

enum MainToSceneMsg {
    SetDrawableSize(Size2D<u32>),
    Build(BuildOptions),
}

struct BuildOptions {
    perspective: Option<Perspective>,
    stem_darkening_font_size: Option<f32>,
}

enum SceneToMainMsg {
    Render { built_scene: BuiltScene, tile_time: Duration }
}

#[derive(Clone)]
struct Options {
    jobs: Option<usize>,
    threed: bool,
    input_path: PathBuf,
}

impl Options {
    fn get() -> Options {
        let matches = App::new("tile-svg")
            .arg(
                Arg::with_name("jobs")
                    .short("j")
                    .long("jobs")
                    .value_name("THREADS")
                    .takes_value(true)
                    .help("Number of threads to use"),
            )
            .arg(
                Arg::with_name("3d")
                    .short("3")
                    .long("3d")
                    .help("Run in 3D"),
            )
            .arg(
                Arg::with_name("INPUT")
                    .help("Path to the SVG file to render")
                    .required(true)
                    .index(1),
            )
            .get_matches();
        let jobs: Option<usize> = matches
            .value_of("jobs")
            .map(|string| string.parse().unwrap());
        let threed = matches.is_present("3d");
        let input_path = PathBuf::from(matches.value_of("INPUT").unwrap());

        // Set up Rayon.
        let mut thread_pool_builder = ThreadPoolBuilder::new();
        if let Some(jobs) = jobs {
            thread_pool_builder = thread_pool_builder.num_threads(jobs);
        }
        thread_pool_builder.build_global().unwrap();

        Options { jobs, threed, input_path }
    }
}

fn load_scene(options: &Options) -> Scene {
    let usvg = Tree::from_file(&options.input_path, &UsvgOptions::default()).unwrap();
    let scene = Scene::from_tree(usvg);
    println!("Scene bounds: {:?}", scene.bounds);
    println!("{} objects, {} paints", scene.objects.len(), scene.paints.len());
    scene
}

fn build_scene(scene: &Scene, build_options: BuildOptions, jobs: Option<usize>) -> BuiltScene {
    let z_buffer = ZBuffer::new(scene.view_box);

    let render_options = RenderOptions {
        transform: match build_options.perspective {
            None => RenderTransform::Transform2D(Transform2DF32::default()),
            Some(perspective) => RenderTransform::Perspective(perspective),
        },
        dilation: match build_options.stem_darkening_font_size {
            None => Point2DF32::default(),
            Some(font_size) => {
                let (x, y) = (STEM_DARKENING_FACTORS[0], STEM_DARKENING_FACTORS[1]);
                Point2DF32::new(x, y).scale(font_size)
            }
        },
    };

    let built_objects = panic::catch_unwind(|| {
         match jobs {
            Some(1) => scene.build_objects_sequentially(render_options, &z_buffer),
            _ => scene.build_objects(render_options, &z_buffer),
        }
    });

    let built_objects = match built_objects {
        Ok(built_objects) => built_objects,
        Err(_) => {
            eprintln!("Scene building crashed! Dumping scene:");
            println!("{:?}", scene);
            process::exit(1);
        }
    };

    let mut built_scene = BuiltScene::new(scene.view_box);
    built_scene.shaders = scene.build_shaders();

    let mut scene_builder = SceneBuilder::new(built_objects, z_buffer, scene.view_box);
    built_scene.solid_tiles = scene_builder.build_solid_tiles();
    while let Some(batch) = scene_builder.build_batch() {
        built_scene.batches.push(batch);
    }

    built_scene
}

struct DemoUI {
    effects_texture: Texture,
    open_texture: Texture,

    threed_enabled: bool,
    effects_window_visible: bool,
    gamma_correction_effect_enabled: bool,
    stem_darkening_effect_enabled: bool,
    subpixel_aa_effect_enabled: bool,
}

impl DemoUI {
    fn new(options: Options) -> DemoUI {
        let effects_texture = Texture::from_png(EFFECTS_PNG_NAME);
        let open_texture = Texture::from_png(OPEN_PNG_NAME);

        DemoUI {
            effects_texture,
            open_texture,
            threed_enabled: options.threed,
            effects_window_visible: false,
            gamma_correction_effect_enabled: false,
            stem_darkening_effect_enabled: false,
            subpixel_aa_effect_enabled: false,
        }
    }

    fn update(&mut self, debug_ui: &mut DebugUI, event: &mut UIEvent) {
        let bottom = debug_ui.framebuffer_size().height as i32 - PADDING;

        // Draw effects button.
        let effects_button_position = Point2DI32::new(PADDING, bottom - BUTTON_HEIGHT);
        if self.draw_button(debug_ui, event, effects_button_position, &self.effects_texture) {
            self.effects_window_visible = !self.effects_window_visible;
        }

        // Draw open button.
        let open_button_x = PADDING + BUTTON_WIDTH + PADDING;
        let open_button_y = bottom - BUTTON_HEIGHT;
        let open_button_position = Point2DI32::new(open_button_x, open_button_y);
        self.draw_button(debug_ui, event, open_button_position, &self.open_texture);

        // Draw 3D switch.
        let threed_switch_x = PADDING + (BUTTON_WIDTH + PADDING) * 2;
        let threed_switch_origin = Point2DI32::new(threed_switch_x, open_button_y);
        debug_ui.draw_solid_rect(RectI32::new(threed_switch_origin,
                                              Point2DI32::new(SWITCH_SIZE, BUTTON_HEIGHT)),
                                 WINDOW_COLOR);
        self.threed_enabled = self.draw_switch(debug_ui,
                                               event,
                                               threed_switch_origin,
                                               "2D",
                                               "3D",
                                               self.threed_enabled);

        // Draw effects window, if necessary.
        self.draw_effects_window(debug_ui, event);
    }

    fn draw_effects_window(&mut self, debug_ui: &mut DebugUI, event: &mut UIEvent) {
        if !self.effects_window_visible {
            return;
        }

        let bottom = debug_ui.framebuffer_size().height as i32 - PADDING;
        let effects_window_y = bottom - (BUTTON_HEIGHT + PADDING + EFFECTS_WINDOW_HEIGHT);
        debug_ui.draw_solid_rect(RectI32::new(Point2DI32::new(PADDING, effects_window_y),
                                            Point2DI32::new(EFFECTS_WINDOW_WIDTH,
                                                            EFFECTS_WINDOW_HEIGHT)),
                                WINDOW_COLOR);

        self.gamma_correction_effect_enabled =
            self.draw_effects_switch(debug_ui,
                                    event,
                                    "Gamma Correction",
                                    0,
                                    effects_window_y,
                                    self.gamma_correction_effect_enabled);
        self.stem_darkening_effect_enabled =
            self.draw_effects_switch(debug_ui,
                                    event,
                                    "Stem Darkening",
                                    1,
                                    effects_window_y,
                                    self.stem_darkening_effect_enabled);
        self.subpixel_aa_effect_enabled =
            self.draw_effects_switch(debug_ui,
                                    event,
                                    "Subpixel AA",
                                    2,
                                    effects_window_y,
                                    self.subpixel_aa_effect_enabled);

    }

    fn draw_button(&self,
                   debug_ui: &mut DebugUI,
                   event: &mut UIEvent,
                   origin: Point2DI32,
                   texture: &Texture)
                   -> bool {
        let button_rect = RectI32::new(origin, Point2DI32::new(BUTTON_WIDTH, BUTTON_HEIGHT));
        debug_ui.draw_solid_rect(button_rect, WINDOW_COLOR);
        debug_ui.draw_rect_outline(button_rect, TEXT_COLOR);
        debug_ui.draw_texture(origin + Point2DI32::new(PADDING, PADDING), texture, TEXT_COLOR);
        event.handle_mouse_down_in_rect(button_rect)
    }

    fn draw_effects_switch(&self,
                           debug_ui: &mut DebugUI,
                           event: &mut UIEvent,
                           text: &str,
                           index: i32,
                           window_y: i32,
                           value: bool)
                           -> bool {
        let text_x = PADDING * 2;
        let text_y = window_y + PADDING + BUTTON_TEXT_OFFSET + (BUTTON_HEIGHT + PADDING) * index;
        debug_ui.draw_text(text, Point2DI32::new(text_x, text_y), false);

        let switch_x = PADDING + EFFECTS_WINDOW_WIDTH - (SWITCH_SIZE + PADDING);
        let switch_y = window_y + PADDING + (BUTTON_HEIGHT + PADDING) * index;
        self.draw_switch(debug_ui, event, Point2DI32::new(switch_x, switch_y), "Off", "On", value)
    }

    fn draw_switch(&self,
                   debug_ui: &mut DebugUI,
                   event: &mut UIEvent,
                   origin: Point2DI32,
                   off_text: &str,
                   on_text: &str,
                   mut value: bool)
                   -> bool {
        let widget_rect = RectI32::new(origin, Point2DI32::new(SWITCH_SIZE, BUTTON_HEIGHT));
        if event.handle_mouse_down_in_rect(widget_rect) {
            value = !value;
        }

        debug_ui.draw_rect_outline(widget_rect, TEXT_COLOR);

        let highlight_size = Point2DI32::new(SWITCH_HALF_SIZE, BUTTON_HEIGHT);
        if !value {
            debug_ui.draw_solid_rect(RectI32::new(origin, highlight_size), TEXT_COLOR);
        } else {
            let x_offset = SWITCH_HALF_SIZE + 1;
            debug_ui.draw_solid_rect(RectI32::new(origin + Point2DI32::new(x_offset, 0),
                                                  highlight_size),
                                     TEXT_COLOR);
        }

        let off_size = debug_ui.measure_text(off_text);
        let on_size = debug_ui.measure_text(on_text);
        let off_offset = SWITCH_HALF_SIZE / 2 - off_size / 2;
        let on_offset  = SWITCH_HALF_SIZE + SWITCH_HALF_SIZE / 2 - on_size / 2;
        let text_top = BUTTON_TEXT_OFFSET;

        debug_ui.draw_text(off_text, origin + Point2DI32::new(off_offset, text_top), !value);
        debug_ui.draw_text(on_text, origin + Point2DI32::new(on_offset, text_top), value);

        value
    }
}

enum UIEvent {
    None,
    MouseDown(Point2DI32),
}

impl UIEvent {
    fn is_none(&self) -> bool {
        match *self { UIEvent::None => true, _ => false }
    }

    fn handle_mouse_down_in_rect(&mut self, rect: RectI32) -> bool {
        if let UIEvent::MouseDown(point) = *self {
            if rect.contains_point(point) {
                *self = UIEvent::None;
                return true;
            }
        }
        false
    }
}