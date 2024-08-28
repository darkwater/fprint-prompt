// Based on:
// https://github.com/Smithay/client-toolkit/blob/c583de8dd5651f8168c6513cd282137c42aae049/examples/simple_layer.rs

use std::convert::TryInto;

use dbus::{ffidisp::Connection as DbusConnection, Message, MessageType};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_layer, delegate_output, delegate_pointer,
    delegate_registry, delegate_seat, delegate_shm,
    output::{OutputHandler, OutputState},
    reexports::protocols::wp::{
        fractional_scale::v1::client::{
            wp_fractional_scale_manager_v1::{self, WpFractionalScaleManagerV1},
            wp_fractional_scale_v1::{self, WpFractionalScaleV1},
        },
        viewporter::client::{
            wp_viewport::{self, WpViewport},
            wp_viewporter::{self, WpViewporter},
        },
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers},
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
    Connection, Dispatch, QueueHandle,
};

struct PositionInfo {
    thickness: u32,
    length: u32,
    edge: Anchor,
    close_to: Anchor,
    offset: u32,
}

impl PositionInfo {
    fn win_width(&self) -> u32 {
        match self.edge {
            Anchor::TOP | Anchor::BOTTOM => self.length + self.offset,
            Anchor::LEFT | Anchor::RIGHT => self.thickness,
            _ => unreachable!(),
        }
    }

    fn win_height(&self) -> u32 {
        match self.edge {
            Anchor::TOP | Anchor::BOTTOM => self.thickness,
            Anchor::LEFT | Anchor::RIGHT => self.length + self.offset,
            _ => unreachable!(),
        }
    }
}

fn main() {
    std::env::set_var("RUST_LOG", "info");
    env_logger::init();

    let pos = PositionInfo {
        thickness: 8,
        length: 138,
        edge: Anchor::RIGHT,
        close_to: Anchor::TOP,
        offset: 100,
    };

    // All Wayland apps start by connecting the compositor (server).
    let conn = Connection::connect_to_env().unwrap();

    // Enumerate the list of globals to get the protocols the server implements.
    let (globals, mut event_queue) = registry_queue_init(&conn).unwrap();
    let qh = event_queue.handle();

    // The compositor (not to be confused with the server which is commonly called the compositor) allows
    // configuring surfaces to be presented.
    let compositor = CompositorState::bind(&globals, &qh).expect("wl_compositor is not available");
    // This app uses the wlr layer shell, which may not be available with every compositor.
    let layer_shell = LayerShell::bind(&globals, &qh).expect("layer shell is not available");
    // Since we are not using the GPU in this example, we use wl_shm to allow software rendering to a buffer
    // we share with the compositor process.
    let shm = Shm::bind(&globals, &qh).expect("wl_shm is not available");

    let fsm = globals
        .bind::<WpFractionalScaleManagerV1, _, _>(&qh, 1..=1, ())
        .expect("wp_fractional_scale_manager_v1 is not available");

    let viewporter = globals
        .bind::<WpViewporter, _, _>(&qh, 1..=1, ())
        .expect("wp_fractional_scale_manager_v1 is not available");

    // A layer surface is created from a surface.
    let surface = compositor.create_surface(&qh);

    let fs = fsm.get_fractional_scale(&surface, &qh, ());
    let viewport = viewporter.get_viewport(&surface, &qh, ());

    let output_state = OutputState::new(&globals, &qh);

    // And then we create the layer shell.
    let layer = layer_shell.create_layer_surface(
        &qh,
        surface,
        Layer::Overlay,
        Some("fprint-prompt"),
        // HACK: just first output
        output_state.outputs().next().as_ref(),
    );

    // Configure the layer surface, providing things like the anchor on screen, desired size and the keyboard
    // interactivity
    layer.set_anchor(pos.edge | pos.close_to);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer.set_size(pos.win_width(), pos.win_height());

    // In order for the layer surface to be mapped, we need to perform an initial commit with no attached
    // buffer. For more info, see WaylandSurface::commit
    //
    // The compositor will respond with an initial configure that we can then use to present to the layer
    // surface with the correct options.
    layer.commit();

    // We don't know how large the window will be yet, so lets assume the minimum size we suggested for the
    // initial memory allocation.
    let pool = SlotPool::new(
        pos.win_width() as usize * pos.win_height() as usize * 4,
        &shm,
    )
    .expect("Failed to create pool");

    let mut simple_layer = SimpleLayer {
        // Seats and outputs may be hotplugged at runtime, therefore we need to setup a registry state to
        // listen for seats and outputs.
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,

        exit: false,
        first_configure: true,
        pool,
        width: pos.win_width(),
        height: pos.win_height(),
        shift: None,
        layer,
        keyboard: None,
        keyboard_focus: false,
        pointer: None,

        pos,
        scale: 1.,
        viewport,
        prompt: None,
    };

    let dbus = DbusConnection::new_system().unwrap();

    dbus.add_match("interface='net.reactivated.Fprint.Device',member='VerifyStatus'")
        .unwrap();

    dbus.add_match("interface='net.reactivated.Fprint.Device',member='VerifyFingerSelected'")
        .unwrap();

    // We don't draw immediately, the configure will notify us when to first draw.
    loop {
        event_queue.blocking_dispatch(&mut simple_layer).unwrap();

        if let Some(msg) = dbus.incoming(10).next() {
            match verify_status_msg(&msg) {
                Some(FprintEvent::VerifyFingerSelected { finger_name }) => {
                    println!("Finger selected: {:?}", finger_name);
                    simple_layer.prompt = finger_name;
                }
                Some(FprintEvent::VerifyStatus { result, done }) => {
                    println!("Result: {:?}, Done: {:?}", result, done);

                    if done == Some(true) {
                        simple_layer.prompt = None;
                    }
                }
                None => (),
            }
        }

        if simple_layer.exit {
            println!("exiting example");
            break;
        }
    }
}

struct SimpleLayer {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,

    exit: bool,
    first_configure: bool,
    pool: SlotPool,
    width: u32,
    height: u32,
    shift: Option<u32>,
    layer: LayerSurface,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    keyboard_focus: bool,
    pointer: Option<wl_pointer::WlPointer>,

    pos: PositionInfo,
    scale: f32,
    viewport: WpViewport,
    prompt: Option<String>,
}

enum FprintEvent {
    VerifyFingerSelected {
        finger_name: Option<String>,
    },
    VerifyStatus {
        result: Option<String>,
        done: Option<bool>,
    },
}

fn verify_status_msg(msg: &Message) -> Option<FprintEvent> {
    if msg.msg_type() != MessageType::Signal {
        return None;
    };
    if &*msg.interface().unwrap() != "net.reactivated.Fprint.Device" {
        return None;
    };
    // if &*msg.member().unwrap() != "VerifyStatus" {
    //     return None;
    // };
    // let (result, done) = msg.get2::<&str, bool>();
    // Some((result, done))

    match msg.member().as_deref() {
        Some("VerifyFingerSelected") => {
            let finger_name = msg.get1::<String>();
            Some(FprintEvent::VerifyFingerSelected { finger_name })
        }
        Some("VerifyStatus") => {
            let (result, done) = msg.get2::<String, bool>();
            Some(FprintEvent::VerifyStatus { result, done })
        }
        _ => None,
    }
}

impl CompositorHandler for SimpleLayer {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
        // Not needed for this example.
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
        // Not needed for this example.
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        self.draw(qh);
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
        // Not needed for this example.
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
        // Not needed for this example.
    }
}

impl OutputHandler for SimpleLayer {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        if self
            .output_state
            .info(&output)
            .and_then(|info| info.name)
            .as_deref()
            == Some("eDP-1")
        {
            // use this display?
        }
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

impl LayerShellHandler for SimpleLayer {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        if configure.new_size.0 == 0 || configure.new_size.1 == 0 {
            self.width = 256;
            self.height = 256;
        } else {
            self.width = configure.new_size.0;
            self.height = configure.new_size.1;
        }

        // Initiate the first draw.
        if self.first_configure {
            self.first_configure = false;

            self.draw(qh);
        }
    }
}

impl SeatHandler for SimpleLayer {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            println!("Set keyboard capability");
            let keyboard = self
                .seat_state
                .get_keyboard(qh, &seat, None)
                .expect("Failed to create keyboard");
            self.keyboard = Some(keyboard);
        }

        if capability == Capability::Pointer && self.pointer.is_none() {
            println!("Set pointer capability");
            let pointer = self
                .seat_state
                .get_pointer(qh, &seat)
                .expect("Failed to create pointer");
            self.pointer = Some(pointer);
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_some() {
            println!("Unset keyboard capability");
            self.keyboard.take().unwrap().release();
        }

        if capability == Capability::Pointer && self.pointer.is_some() {
            println!("Unset pointer capability");
            self.pointer.take().unwrap().release();
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl KeyboardHandler for SimpleLayer {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        keysyms: &[Keysym],
    ) {
        if self.layer.wl_surface() == surface {
            println!("Keyboard focus on window with pressed syms: {keysyms:?}");
            self.keyboard_focus = true;
        }
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        if self.layer.wl_surface() == surface {
            println!("Release keyboard focus on window");
            self.keyboard_focus = false;
        }
    }

    fn press_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        println!("Key press: {event:?}");
        // press 'esc' to exit
        if event.keysym == Keysym::Escape {
            self.exit = true;
        }
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        println!("Key release: {event:?}");
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _serial: u32,
        modifiers: Modifiers,
        _layout: u32,
    ) {
        println!("Update modifiers: {modifiers:?}");
    }
}

impl PointerHandler for SimpleLayer {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        use PointerEventKind::*;
        for event in events {
            // Ignore events for other surfaces
            if &event.surface != self.layer.wl_surface() {
                continue;
            }
            match event.kind {
                Enter { .. } => {
                    println!("Pointer entered @{:?}", event.position);
                }
                Leave { .. } => {
                    println!("Pointer left");
                }
                Motion { .. } => {}
                Press { button, .. } => {
                    println!("Press {:x} @ {:?}", button, event.position);
                    self.shift = self.shift.xor(Some(0));
                }
                Release { button, .. } => {
                    println!("Release {:x} @ {:?}", button, event.position);
                }
                Axis {
                    horizontal,
                    vertical,
                    ..
                } => {
                    println!("Scroll H:{horizontal:?}, V:{vertical:?}");
                }
            }
        }
    }
}

impl Dispatch<WpViewporter, ()> for SimpleLayer {
    fn event(
        _state: &mut Self,
        _proxy: &WpViewporter,
        _event: wp_viewporter::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpViewport, ()> for SimpleLayer {
    fn event(
        _state: &mut Self,
        _proxy: &WpViewport,
        _event: wp_viewport::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpFractionalScaleManagerV1, ()> for SimpleLayer {
    fn event(
        _state: &mut Self,
        _proxy: &WpFractionalScaleManagerV1,
        _event: wp_fractional_scale_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        log::info!("manager");
    }
}

impl Dispatch<WpFractionalScaleV1, ()> for SimpleLayer {
    fn event(
        state: &mut Self,
        _proxy: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            state.scale = scale as f32 / 120.;
            state.layer.set_size(
                state.pos.win_width() * 120 / scale,
                state.pos.win_height() * 120 / scale,
            );
            state.viewport.set_destination(
                (state.pos.win_width() * 120 / scale) as i32,
                (state.pos.win_height() * 120 / scale) as i32,
            );
        }
    }
}

impl ShmHandler for SimpleLayer {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl SimpleLayer {
    pub fn draw(&mut self, qh: &QueueHandle<Self>) {
        let width = self.pos.win_width();
        let height = self.pos.win_height();
        let stride = width as i32 * 4;

        let (buffer, canvas) = self
            .pool
            .create_buffer(
                width as i32,
                height as i32,
                stride,
                wl_shm::Format::Argb8888,
            )
            .expect("create buffer");

        // Draw to the window:
        if self.prompt.is_some() {
            let shift = self.shift.unwrap_or(0);
            canvas
                .chunks_exact_mut(4)
                .enumerate()
                .for_each(|(index, chunk)| {
                    let _x = ((index + shift as usize) % width as usize) as u32;
                    let y = (index / width as usize) as u32;

                    let color: u32 = if y < self.pos.offset { 0x0 } else { 0xFFFFFFFF };

                    let array: &mut [u8; 4] = chunk.try_into().unwrap();
                    *array = color.to_le_bytes();
                });

            if let Some(shift) = &mut self.shift {
                *shift = (*shift + 1) % width;
            }
        } else {
            canvas.iter_mut().for_each(|pixel| *pixel = 0);
        }

        // Damage the entire window
        self.layer
            .wl_surface()
            .damage_buffer(0, 0, width as i32, height as i32);

        // Request our next frame
        self.layer
            .wl_surface()
            .frame(qh, self.layer.wl_surface().clone());

        // Attach and commit to present.
        buffer
            .attach_to(self.layer.wl_surface())
            .expect("buffer attach");
        self.layer.commit();

        // TODO save and reuse buffer when the window size is unchanged.  This is especially
        // useful if you do damage tracking, since you don't need to redraw the undamaged parts
        // of the canvas.
    }
}

delegate_compositor!(SimpleLayer);
delegate_output!(SimpleLayer);
delegate_shm!(SimpleLayer);

delegate_seat!(SimpleLayer);
delegate_keyboard!(SimpleLayer);
delegate_pointer!(SimpleLayer);

delegate_layer!(SimpleLayer);

delegate_registry!(SimpleLayer);

impl ProvidesRegistryState for SimpleLayer {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}
