mod clientside;
mod decoration;
mod dispatch;
mod event;
pub(crate) mod selection;
#[cfg(test)]
mod tests;

use self::event::*;
use crate::xstate::{
    Decorations, MoveResizeDirection, WindowDims, WindowRole, WmHints, WmName, WmNormalHints,
};
use crate::{X11Selection, XConnection, timespec_from_millis};
use clientside::MyWorld;
use decoration::{DecorationsData, DecorationsDataSatellite};
use hecs::Entity;
use log::{debug, error, warn};
use rustix::event::{PollFd, PollFlags, poll};
use rustix::fs::Timespec;
use smithay_client_toolkit::activation::ActivationState;
use std::collections::{HashMap, HashSet};
use std::ops::{Deref, DerefMut};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::time::Duration;
use wayland_client::protocol::wl_subcompositor::WlSubcompositor;
use wayland_client::{
    Connection, EventQueue, Proxy, QueueHandle,
    globals::{Global, registry_queue_init},
    protocol as client,
};
use wayland_protocols::xdg::decoration::zv1::client::zxdg_decoration_manager_v1::ZxdgDecorationManagerV1;
use wayland_protocols::xdg::decoration::zv1::client::zxdg_toplevel_decoration_v1::{self};
use wayland_protocols::xdg::shell::client::xdg_positioner::ConstraintAdjustment;
use wayland_protocols::{
    wp::{
        fractional_scale::v1::client::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
        linux_dmabuf::zv1::{client as c_dmabuf, server as s_dmabuf},
        linux_drm_syncobj::v1::server::wp_linux_drm_syncobj_manager_v1::WpLinuxDrmSyncobjManagerV1,
        pointer_constraints::zv1::{
            client::{zwp_confined_pointer_v1, zwp_locked_pointer_v1},
            server::zwp_pointer_constraints_v1::ZwpPointerConstraintsV1,
        },
        relative_pointer::zv1::{
            client::zwp_relative_pointer_v1,
            server::zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1,
        },
        tablet::zv2::client::{
            zwp_tablet_pad_group_v2, zwp_tablet_pad_ring_v2, zwp_tablet_pad_strip_v2,
            zwp_tablet_pad_v2, zwp_tablet_seat_v2, zwp_tablet_tool_v2, zwp_tablet_v2,
        },
        tablet::zv2::server::zwp_tablet_manager_v2::ZwpTabletManagerV2,
        viewporter::client::wp_viewporter::WpViewporter,
    },
    xdg::{
        shell::client::{
            xdg_popup::XdgPopup,
            xdg_positioner::{Anchor, Gravity, XdgPositioner},
            xdg_surface::XdgSurface,
            xdg_toplevel::{self, XdgToplevel},
            xdg_wm_base::XdgWmBase,
        },
        xdg_output::zv1::server::zxdg_output_manager_v1::ZxdgOutputManagerV1,
    },
    xwayland::shell::v1::server::xwayland_shell_v1::XwaylandShellV1,
};
use wayland_server::protocol::wl_seat::WlSeat;
use wayland_server::{
    Client, DisplayHandle, Resource, WEnum,
    backend::GlobalId,
    protocol::{
        wl_callback::WlCallback, wl_compositor::WlCompositor, wl_output::WlOutput, wl_shm::WlShm,
        wl_surface::WlSurface,
    },
};
use wl_drm::{client::wl_drm::WlDrm as WlDrmClient, server::wl_drm::WlDrm as WlDrmServer};
use xcb::x;
use zones::client::{
    xx_zone_item_v1::{self, XxZoneItemV1},
    xx_zone_manager_v1::XxZoneManagerV1,
    xx_zone_v1::{self, XxZoneV1},
};

impl From<&x::CreateNotifyEvent> for WindowDims {
    fn from(value: &x::CreateNotifyEvent) -> Self {
        Self {
            x: value.x(),
            y: value.y(),
            width: value.width(),
            height: value.height(),
        }
    }
}

type Request<T> = <T as Resource>::Request;

/// Converts a WEnum from its client side version to its server side version
fn convert_wenum<Client, Server>(wenum: WEnum<Client>) -> Server
where
    u32: From<WEnum<Client>>,
    Server: TryFrom<u32>,
    <Server as TryFrom<u32>>::Error: std::fmt::Debug,
{
    u32::from(wenum).try_into().unwrap()
}

#[derive(Default, Debug)]
struct WindowAttributes {
    acquire_input_via_wm: bool,
    has_take_focus: bool,
    role: WindowRole,
    dims: WindowDims,
    size_hints: Option<WmNormalHints>,
    title: Option<WmName>,
    class: Option<String>,
    group: Option<x::Window>,
    decorations: Option<Decorations>,
    transient_for: Option<x::Window>,
}

impl WindowAttributes {
    /// AKA "Passive" input model
    fn require_wm_focus(&self) -> bool {
        self.acquire_input_via_wm && !self.has_take_focus
    }
}

#[derive(Debug, Default, PartialEq, Eq, Copy, Clone)]
struct WindowOutputOffset {
    x: i32,
    y: i32,
}

#[derive(Debug)]
struct WindowData {
    mapped: bool,
    mapped_once: bool,
    position_requested: bool,
    attrs: WindowAttributes,
    output_offset: WindowOutputOffset,
    activation_token: Option<String>,
}

impl WindowData {
    fn new(override_redirect: bool, dims: WindowDims, activation_token: Option<String>) -> Self {
        Self {
            mapped: false,
            mapped_once: false,
            position_requested: false,
            attrs: WindowAttributes {
                role: WindowRole::new_basic(override_redirect),
                dims,
                ..Default::default()
            },
            output_offset: WindowOutputOffset::default(),
            activation_token,
        }
    }

    fn update_output_offset<C: XConnection>(
        &mut self,
        window: x::Window,
        offset: WindowOutputOffset,
        connection: &mut C,
    ) {
        log::trace!(target: "output_offset", "offset: {offset:?}");
        if offset == self.output_offset {
            return;
        }

        let dims = &mut self.attrs.dims;
        dims.x += (offset.x - self.output_offset.x) as i16;
        dims.y += (offset.y - self.output_offset.y) as i16;
        self.output_offset = offset;

        if connection.set_window_dims(
            window,
            PendingSurfaceState {
                x: dims.x as i32,
                y: dims.y as i32,
                width: self.attrs.dims.width as _,
                height: self.attrs.dims.height as _,
            },
        ) {
            debug!(target: "output_offset", "set {:?} offset to {:?}", window, self.output_offset);
        }
    }

    fn has_initial_position(&self) -> bool {
        self.position_requested
            || self
                .attrs
                .size_hints
                .is_some_and(|hints| hints.has_position)
    }

    fn record_position_request(&mut self, x: Option<i32>, y: Option<i32>) {
        self.position_requested = true;
        if let Some(x) = x {
            self.attrs.dims.x = x as i16;
        }
        if let Some(y) = y {
            self.attrs.dims.y = y as i16;
        }
    }
}

struct SurfaceAttach {
    buffer: Option<client::wl_buffer::WlBuffer>,
    x: i32,
    y: i32,
}

#[derive(PartialEq, Eq, Debug)]
struct SurfaceSerial([u32; 2]);

#[derive(Debug)]
enum SurfaceRole {
    Toplevel(Option<ToplevelData>),
    Popup(Option<PopupData>),
}

impl SurfaceRole {
    fn xdg(&self) -> Option<&XdgSurfaceData> {
        match self {
            SurfaceRole::Toplevel(t) => t.as_ref().map(|t| &t.xdg),
            SurfaceRole::Popup(p) => p.as_ref().map(|p| &p.xdg),
        }
    }

    fn xdg_mut(&mut self) -> Option<&mut XdgSurfaceData> {
        match self {
            SurfaceRole::Toplevel(t) => t.as_mut().map(|t| &mut t.xdg),
            SurfaceRole::Popup(p) => p.as_mut().map(|p| &mut p.xdg),
        }
    }

    fn destroy(&mut self) {
        match self {
            SurfaceRole::Toplevel(Some(t)) => {
                if let Some(zone_item) = t.zone_item.take() {
                    zone_item.item.destroy();
                }
                if let Some(decoration) = t.decoration.wl.take() {
                    decoration.destroy();
                }
                t.toplevel.destroy();
                t.xdg.surface.destroy();
            }
            SurfaceRole::Popup(Some(p)) => {
                p.positioner.destroy();
                p.popup.destroy();
                p.xdg.surface.destroy();
            }
            _ => {}
        }
    }
}

#[derive(Debug)]
struct XdgSurfaceData {
    surface: XdgSurface,
    configured: bool,
    pending: Option<PendingSurfaceState>,
}

#[derive(Debug)]
struct ToplevelData {
    toplevel: XdgToplevel,
    xdg: XdgSurfaceData,
    fullscreen: bool,
    decoration: decoration::DecorationsData,
    zone_item: Option<ZoneItemData>,
}

#[derive(Debug)]
struct ZoneItemData {
    item: XxZoneItemV1,
    target: ZoneTarget,
    associated: bool,
    association_pending: bool,
}

#[derive(Debug)]
struct PopupData {
    popup: XdgPopup,
    positioner: XdgPositioner,
    xdg: XdgSurfaceData,
}

trait Event {
    fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>);
}

struct ZoneItemEvent {
    item: XxZoneItemV1,
    event: xx_zone_item_v1::Event,
}

impl Event for ZoneItemEvent {
    fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>) {
        let is_current = state
            .world
            .get::<&SurfaceRole>(target)
            .ok()
            .is_some_and(|role| {
                matches!(
                    &*role,
                    SurfaceRole::Toplevel(Some(toplevel))
                        if toplevel
                            .zone_item
                            .as_ref()
                            .is_some_and(|zone_item| zone_item.item == self.item)
                )
            });
        if !is_current {
            debug!("ignoring event from stale zone item {:?}", self.item.id());
            return;
        }

        match self.event {
            xx_zone_item_v1::Event::FrameExtents {
                top,
                bottom,
                left,
                right,
            } => {
                debug!(
                    "zone frame extents for {target:?}: top={top}, bottom={bottom}, left={left}, right={right}"
                );
            }
            xx_zone_item_v1::Event::Position { x, y } => {
                let zone_target = state
                    .world
                    .get::<&SurfaceRole>(target)
                    .ok()
                    .and_then(|role| match &*role {
                        SurfaceRole::Toplevel(Some(toplevel)) => {
                            toplevel.zone_item.as_ref().map(|item| item.target)
                        }
                        _ => None,
                    })
                    .unwrap_or(ZoneTarget::Fallback);
                let data = state.world.entity(target).unwrap();
                let window = data.get::<&WindowData>().unwrap();
                let (x, y) = state.zone_to_x_position(zone_target, &window, x, y);
                drop(window);
                let mut window = data.get::<&mut WindowData>().unwrap();
                window.attrs.dims.x = x as i16;
                window.attrs.dims.y = y as i16;
                let pending = PendingSurfaceState {
                    x,
                    y,
                    width: window.attrs.dims.width.into(),
                    height: window.attrs.dims.height.into(),
                };
                drop(window);
                state.world.insert_one(target, pending).unwrap();
            }
            xx_zone_item_v1::Event::PositionFailed => {
                debug!("compositor rejected a zones position request for {target:?}");
                let data = state.world.entity(target).unwrap();
                let window = *data.get::<&x::Window>().unwrap();
                let dims = data.get::<&WindowData>().unwrap().attrs.dims;
                state.connection.send_configure_notify(
                    window,
                    PendingSurfaceState {
                        x: dims.x.into(),
                        y: dims.y.into(),
                        width: dims.width.into(),
                        height: dims.height.into(),
                    },
                );
            }
            xx_zone_item_v1::Event::Closed => {
                let Ok(mut role) = state.world.get::<&mut SurfaceRole>(target) else {
                    return;
                };
                if let SurfaceRole::Toplevel(Some(toplevel)) = &mut *role {
                    let zone_item = toplevel.zone_item.take().unwrap();
                    zone_item.item.destroy();
                }
            }
            _ => unreachable!(),
        }
    }
}

macro_rules! enum_try_from {
    (
        $(#[$meta:meta])*
        $pub:vis enum $enum:ident {
            $( $variant:ident($ty:ty) ),+
        }
    ) => {
        $(#[$meta])*
        $pub enum $enum {
            $( $variant($ty) ),+
        }

        $(
            impl TryFrom<$enum> for $ty {
                type Error = String;
                fn try_from(value: $enum) -> Result<Self, Self::Error> {
                    enum_try_from!(@variant_match value $enum $variant)
                }
            }

            impl<'a> TryFrom<&'a $enum> for &'a $ty {
                type Error = String;
                fn try_from(value: &'a $enum) -> Result<Self, Self::Error> {
                    enum_try_from!(@variant_match value $enum $variant)
                }
            }

            impl<'a> TryFrom<&'a mut $enum> for &'a mut $ty {
                type Error = String;
                fn try_from(value: &'a mut $enum) -> Result<Self, Self::Error> {
                    enum_try_from!(@variant_match value $enum $variant)
                }
            }

            impl From<$ty> for $enum {
                fn from(value: $ty) -> Self {
                    $enum::$variant(value)
                }
            }
        )+
    };
    (@variant_match $value:ident $enum:ident $variant:ident) => {
        match $value {
            $enum::$variant(obj) => Ok(obj),
            other => Err(format!("wrong variant type: {}", std::any::type_name_of_val(&other)))
        }
    }
}

macro_rules! impl_event {
    (
        $(#[$meta:meta])*
        $pub:vis enum $enum:ident {
            $( $variant:ident($ty:ty) ),+
        }
    ) => {
        enum_try_from! {
            $(#[$meta])*
            $pub enum $enum {
                $( $variant($ty) ),+
            }
        }

        impl Event for $enum {
            fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>) {
                match self {
                    $(
                        Self::$variant(v) => {
                            v.handle(target, state)
                        }
                    ),+
                }
            }
        }
    }
}

impl_event! {
enum ObjectEvent {
    Surface(event::SurfaceEvents),
    Buffer(client::wl_buffer::Event),
    Seat(client::wl_seat::Event),
    Pointer(client::wl_pointer::Event),
    Keyboard(client::wl_keyboard::Event),
    Touch(client::wl_touch::Event),
    Output(event::OutputEvent),
    Drm(wl_drm::client::wl_drm::Event),
    DmabufFeedback(c_dmabuf::zwp_linux_dmabuf_feedback_v1::Event),
    RelativePointer(zwp_relative_pointer_v1::Event),
    LockedPointer(zwp_locked_pointer_v1::Event),
    ConfinedPointer(zwp_confined_pointer_v1::Event),
    TabletSeat(zwp_tablet_seat_v2::Event),
    Tablet(zwp_tablet_v2::Event),
    TabletPad(zwp_tablet_pad_v2::Event),
    TabletTool(zwp_tablet_tool_v2::Event),
    TabletPadGroup(zwp_tablet_pad_group_v2::Event),
    TabletPadRing(zwp_tablet_pad_ring_v2::Event),
    TabletPadStrip(zwp_tablet_pad_strip_v2::Event),
    ZoneItem(ZoneItemEvent)
}
}

fn handle_new_globals<'a, S: X11Selection + 'static>(
    globals_map: &mut HashMap<GlobalName, (Global, GlobalId)>,
    dh: &DisplayHandle,
    globals: impl IntoIterator<Item = &'a Global>,
) {
    for global in globals {
        macro_rules! server_global {
            ($($global:ty),+) => {
                match global.interface {
                    $(
                        ref x if x == <$global>::interface().name => {
                            let version = u32::min(global.version, <$global>::interface().version);
                            let global_id = dh.create_global::<InnerServerState<S>, $global, Global>(version, global.clone());
                            globals_map.insert(GlobalName(global.name), (global.clone(), global_id));
                        }
                    )+
                    _ => {}
                }
            }
        }

        server_global![
            WlCompositor,
            WlShm,
            WlSeat,
            WlOutput,
            ZwpRelativePointerManagerV1,
            WlDrmServer,
            s_dmabuf::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
            ZxdgOutputManagerV1,
            ZwpPointerConstraintsV1,
            ZwpTabletManagerV2,
            WpLinuxDrmSyncobjManagerV1
        ];
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub(super) struct GlobalName(pub u32);

struct FocusData {
    window: x::Window,
    output_name: Option<String>,
    is_popup: bool,
}

#[derive(Copy, Clone, Default)]
struct GlobalOutputOffsetDimension {
    owner: Option<Entity>,
    value: i32,
}

#[derive(Copy, Clone)]
struct GlobalOutputOffset {
    x: GlobalOutputOffsetDimension,
    y: GlobalOutputOffsetDimension,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum ZoneTarget {
    Fallback,
    Output(Entity),
}

struct ZoneState {
    zone: XxZoneV1,
    valid: bool,
    ready: bool,
    width: i32,
    height: i32,
}

struct OutputZone(ZoneState);

struct ZonesState {
    manager: XxZoneManagerV1,
    fallback: ZoneState,
}

/// The state of the X11 connection before XState has been fully initialized.
/// It implements XConnection minimally, gracefully doing nothing but logging the called functions.
pub struct NoConnection<S: X11Selection + 'static> {
    _p: std::marker::PhantomData<S>,
}
impl<S: X11Selection> XConnection for NoConnection<S> {
    type X11Selection = S;
    fn focus_window(&mut self, _: x::Window, _: Option<String>) {
        debug!("could not focus window without XWayland initialized");
    }
    fn close_window(&mut self, _: x::Window) {
        debug!("could not close window without XWayland initialized");
    }
    fn unmap_window(&mut self, _: x::Window) {
        debug!("could not unmap window without XWayland initialized");
    }
    fn raise_to_top(&mut self, _: x::Window) {
        debug!("could not raise window to top without XWayland initialized");
    }
    fn set_fullscreen(&mut self, _: x::Window, _: bool) {
        debug!("could not toggle fullscreen without XWayland initialized");
    }
    fn set_window_dims(&mut self, _: x::Window, _: crate::server::PendingSurfaceState) -> bool {
        debug!("could not set window dimensions without XWayland initialized");
        false
    }
    fn send_configure_notify(&mut self, _: x::Window, _: crate::server::PendingSurfaceState) {
        debug!("could not send ConfigureNotify without XWayland initialized");
    }
}

pub struct ServerState<C: XConnection> {
    inner: InnerServerState<C::X11Selection>,
    pub connection: C,
}
impl<C: XConnection> Deref for ServerState<C> {
    type Target = InnerServerState<C::X11Selection>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
impl<C: XConnection> DerefMut for ServerState<C> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

pub struct InnerServerState<S: X11Selection> {
    dh: DisplayHandle,
    windows: HashMap<x::Window, Entity>,
    pids: HashSet<u32>,

    world: MyWorld,
    queue: EventQueue<MyWorld>,
    qh: QueueHandle<MyWorld>,
    globals_map: HashMap<GlobalName, (Global, GlobalId)>,
    client: Client,
    to_focus: Option<FocusData>,
    unfocus: bool,
    last_focused_toplevel: Option<x::Window>,
    last_hovered: Option<x::Window>,

    xdg_wm_base: XdgWmBase,
    compositor: client::wl_compositor::WlCompositor,
    subcompositor: WlSubcompositor,
    shm: client::wl_shm::WlShm,
    viewporter: WpViewporter,
    fractional_scale: Option<WpFractionalScaleManagerV1>,
    decoration_manager: Option<ZxdgDecorationManagerV1>,
    zones: Option<ZonesState>,
    selection_states: selection::SelectionStates<S>,
    last_kb_serial: Option<(client::wl_seat::WlSeat, u32)>,
    activation_state: Option<ActivationState>,
    global_output_offset: GlobalOutputOffset,
    global_offset_updated: bool,
    updated_outputs: Vec<Entity>,
    new_scale: Option<f64>,
    current_scale: f64,
}

impl<S: X11Selection> ServerState<NoConnection<S>> {
    pub fn new(
        mut dh: DisplayHandle,
        server_connection: Option<UnixStream>,
        client: UnixStream,
    ) -> Self {
        let connection = if let Some(stream) = server_connection {
            Connection::from_socket(stream).unwrap()
        } else {
            Connection::connect_to_env().unwrap()
        };

        let (global_list, queue) = registry_queue_init::<MyWorld>(&connection).unwrap();
        let qh = queue.handle();

        let xdg_wm_base = global_list
            .bind::<XdgWmBase, _, _>(&qh, 2..=6, ())
            .expect("Could not bind xdg_wm_base");

        if xdg_wm_base.version() < 3 {
            warn!(
                "xdg_wm_base version 2 detected. Popup repositioning will not work, and some popups may not work correctly."
            );
        }

        let compositor = global_list
            .bind::<client::wl_compositor::WlCompositor, _, _>(&qh, 4..=6, ())
            .expect("Could not bind wl_compositor");

        let subcompositor = global_list
            .bind::<WlSubcompositor, _, _>(&qh, 1..=1, ())
            .expect("Could not bind wl_subcompositor");

        let shm = global_list
            .bind::<client::wl_shm::WlShm, _, _>(&qh, 1..=1, ())
            .expect("Could not bind wl_shm");

        let viewporter = global_list
            .bind::<WpViewporter, _, _>(&qh, 1..=1, ())
            .expect("Could not bind wp_viewporter");

        let fractional_scale = global_list
            .bind::<WpFractionalScaleManagerV1, _, _>(&qh, 1..=1, ())
            .inspect_err(|e| {
                warn!(
                    "Couldn't bind fractional scale manager: {e}. Fractional scaling will not work."
                )
            })
            .ok();

        let activation_state = ActivationState::bind(&global_list, &qh)
            .inspect_err(|e| {
                warn!("Could not bind xdg activation ({e:?}). Windows might not receive focus depending on compositor focus stealing policy.")
            })
            .ok();

        let decoration_manager = global_list
            .bind::<ZxdgDecorationManagerV1, _, _>(&qh, 1..=1, ())
            .ok();

        let zones = global_list
            .bind::<XxZoneManagerV1, _, _>(&qh, 1..=1, ())
            .map(|manager| {
                let zone = manager.get_zone(None, &qh, ZoneTarget::Fallback);
                ZonesState {
                    manager,
                    fallback: ZoneState {
                        zone,
                        valid: false,
                        ready: false,
                        width: 0,
                        height: 0,
                    },
                }
            })
            .inspect_err(|e| {
                debug!("Could not bind zones manager ({e:?}); explicit X11 window positioning will remain disabled")
            })
            .ok();

        let selection_states = selection::SelectionStates::new(&global_list, &qh);

        dh.create_global::<InnerServerState<S>, XwaylandShellV1, _>(1, ());

        let mut globals_map = HashMap::new();
        global_list
            .contents()
            .with_list(|globals| handle_new_globals::<S>(&mut globals_map, &dh, globals));

        let world = MyWorld::new(global_list);
        let client = dh.insert_client(client, std::sync::Arc::new(())).unwrap();

        let inner = InnerServerState {
            windows: HashMap::new(),
            pids: HashSet::new(),
            client,
            queue,
            qh,
            globals_map,
            dh,
            to_focus: None,
            unfocus: false,
            last_focused_toplevel: None,
            last_hovered: None,
            xdg_wm_base,
            compositor,
            subcompositor,
            shm,
            viewporter,
            fractional_scale,
            selection_states,
            last_kb_serial: None,
            activation_state,
            global_output_offset: GlobalOutputOffset {
                x: GlobalOutputOffsetDimension {
                    owner: None,
                    value: 0,
                },
                y: GlobalOutputOffsetDimension {
                    owner: None,
                    value: 0,
                },
            },
            global_offset_updated: false,
            updated_outputs: Vec::new(),
            new_scale: None,
            current_scale: 1.0,
            decoration_manager,
            zones,
            world,
        };
        Self {
            inner,
            connection: NoConnection {
                _p: std::marker::PhantomData,
            },
        }
    }

    pub fn upgrade_connection<C>(self, connection: C) -> ServerState<C>
    where
        C: XConnection<X11Selection = S>,
    {
        ServerState {
            inner: self.inner,
            connection,
        }
    }
}

impl<C: XConnection> ServerState<C> {
    pub fn run(&mut self) {
        if let Some(r) = self.queue.prepare_read() {
            let fd = r.connection_fd();
            let pollfd = PollFd::new(&fd, PollFlags::IN);
            let timeout = timespec_from_millis(0);
            if poll(&mut [pollfd], Some(&timeout)).unwrap() > 0 {
                let _ = r.read();
            }
        }
        let state = self.deref_mut();
        state
            .queue
            .dispatch_pending(&mut state.world)
            .expect("Failed dispatching client side Wayland events");
        self.handle_clientside_events();
    }

    pub fn handle_clientside_events(&mut self) {
        self.handle_globals();
        self.handle_zone_events();

        for (target, event) in self.world.read_events() {
            if !self.world.contains(target) {
                warn!("could not handle clientside event: stale object");
                continue;
            }
            event.handle(target, self);
        }

        let query = self.world.query_mut::<(&x::Window, &PendingSurfaceState)>();
        let iter = query
            .into_iter()
            .map(|(e, (win, dims))| (e, (*win, *dims)))
            .collect::<Vec<_>>();
        for (entity, (win, dims)) in iter.into_iter() {
            self.connection.set_window_dims(win, dims);
            self.world
                .remove_one::<PendingSurfaceState>(entity)
                .unwrap();
        }

        if self.global_output_offset.x.owner.is_none()
            || self.global_output_offset.y.owner.is_none()
        {
            self.calc_global_output_offset();
            self.global_offset_updated = true;
        }
        if self.global_offset_updated {
            debug!(
                target: "output_offset",
                "updated global output offset: {}x{}",
                self.global_output_offset.x.value, self.global_output_offset.y.value
            );
            let state = &self.inner;
            for (e, _) in state.world.query::<&WlOutput>().iter() {
                event::update_global_output_offset(
                    e,
                    &state.global_output_offset,
                    &state.world,
                    &mut self.connection,
                );
            }
            self.global_offset_updated = false;
        }

        if !self.updated_outputs.is_empty() {
            for output in std::mem::take(&mut self.updated_outputs).iter() {
                let Ok(output_scale) = self.world.get::<&OutputScaleFactor>(*output) else {
                    continue;
                };
                if matches!(*output_scale, OutputScaleFactor::Output(..)) {
                    let mut surface_query = self
                        .world
                        .query::<(&OnOutput, &mut SurfaceScaleFactor)>()
                        .with::<(&WindowData, &WlSurface)>();

                    let mut surfaces = vec![];
                    for (surface, (OnOutput(s_output), surface_scale)) in surface_query.iter() {
                        if s_output == output {
                            surface_scale.0 = output_scale.get();
                            surfaces.push(surface);
                        }
                    }

                    drop(surface_query);
                    for surface in surfaces {
                        update_surface_viewport(
                            &self.world,
                            self.world.query_one(surface).unwrap(),
                        );
                    }
                }
            }

            let mut mixed_scale = false;
            let mut scale;

            let mut outputs = self.world.query_mut::<&OutputScaleFactor>().into_iter();
            if let Some((_, output_scale)) = outputs.next() {
                scale = output_scale.get();

                for (_, output_scale) in outputs {
                    if output_scale.get() != scale {
                        mixed_scale = true;
                        scale = scale.min(output_scale.get());
                    }
                }

                if mixed_scale {
                    warn!(
                        "Mixed output scales detected, choosing to give apps the smallest detected scale ({scale}x)"
                    );
                }

                debug!("Using new scale {scale}");
                self.new_scale = Some(scale);
                self.current_scale = scale;
            }
        }

        {
            if let Some(FocusData {
                window,
                output_name,
                is_popup,
            }) = self.to_focus.take()
            {
                debug!(
                    "focusing {} {window:?}",
                    if is_popup { "popup" } else { "window" }
                );
                self.connection.focus_window(window, output_name);
                if !is_popup {
                    self.last_focused_toplevel = Some(window);
                }
            } else if self.unfocus {
                self.connection.focus_window(x::WINDOW_NONE, None);
            }
            self.unfocus = false;
        }

        self.handle_selection_events();
        self.handle_activations();
        if let Err(e) = self.queue.flush() {
            match e {
                wayland_client::backend::WaylandError::Io(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock =>
                {
                    let fd = PollFd::new(&self.queue, PollFlags::OUT);
                    match poll(
                        &mut [fd],
                        Some(&Timespec {
                            tv_sec: 0,
                            tv_nsec: Duration::from_millis(50).as_nanos() as _,
                        }),
                    ) {
                        Ok(0) => {
                            error!(
                                "Failed to flush clientside events (timeout)! Will try again later."
                            );
                        }
                        Ok(_) => {
                            self.queue.flush().unwrap();
                        }
                        Err(e) => {
                            error!(
                                "Failed to flush clientside events ({e})! Will try again later."
                            );
                        }
                    }
                }
                other => {
                    panic!("Failed flushing clientside events: {other:#?}");
                }
            }
        }
    }

    fn close_x_window(&mut self, window: x::Window) {
        debug!("sending close request to {window:?}");
        self.connection.close_window(window);
        if self.last_focused_toplevel == Some(window) {
            self.last_focused_toplevel.take();
        }
        if self.last_hovered == Some(window) {
            self.last_hovered.take();
        }
    }
}

impl<S: X11Selection + 'static> InnerServerState<S> {
    pub fn clientside_fd(&self) -> BorrowedFd<'_> {
        self.queue.as_fd()
    }

    fn handle_globals(&mut self) {
        let globals = std::mem::take(&mut self.world.new_globals);
        handle_new_globals::<S>(&mut self.globals_map, &self.dh, &globals);

        let globals = std::mem::take(&mut self.world.removed_globals);
        for global in globals {
            let (global_struct, global_id) = self.globals_map.remove(&global).unwrap();
            self.dh.disable_global::<InnerServerState<S>>(global_id);
            if global_struct.interface == <WlOutput>::interface().name {
                self.remove_output(global);
            }
        }
    }

    fn handle_zone_events(&mut self) {
        for (target, event) in std::mem::take(&mut self.world.zone_events) {
            match event {
                xx_zone_v1::Event::Size { width, height } => {
                    let valid = !(width < 0 && height < 0);
                    let updated = match target {
                        ZoneTarget::Fallback => self.zones.as_mut().map(|zones| {
                            zones.fallback.valid = valid;
                            zones.fallback.width = width;
                            zones.fallback.height = height;
                        }),
                        ZoneTarget::Output(output) => self
                            .world
                            .get::<&mut OutputZone>(output)
                            .ok()
                            .map(|mut zone| {
                                zone.0.valid = valid;
                                zone.0.width = width;
                                zone.0.height = height;
                            }),
                    };
                    if updated.is_some() && !valid {
                        warn!("The compositor denied creation of a window-positioning zone");
                    }
                }
                xx_zone_v1::Event::Done => {
                    let valid = match target {
                        ZoneTarget::Fallback => self.zones.as_mut().map(|zones| {
                            zones.fallback.ready = true;
                            zones.fallback.valid
                        }),
                        ZoneTarget::Output(output) => self
                            .world
                            .get::<&mut OutputZone>(output)
                            .ok()
                            .map(|mut zone| {
                                zone.0.ready = true;
                                zone.0.valid
                            }),
                    };
                    if valid != Some(true) {
                        continue;
                    }

                    let entities = self
                        .world
                        .query::<&SurfaceRole>()
                        .iter()
                        .filter_map(|(entity, role)| match role {
                            SurfaceRole::Toplevel(Some(toplevel))
                                if toplevel.zone_item.is_none() =>
                            {
                                let window = self.world.get::<&WindowData>(entity).ok()?;
                                (self.zone_target_for_position(
                                    entity,
                                    window.attrs.dims.x.into(),
                                    window.attrs.dims.y.into(),
                                ) == target)
                                    .then_some(entity)
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>();
                    for entity in entities {
                        self.attach_zone_item(entity, true);
                    }
                }
                xx_zone_v1::Event::ItemEntered { item } => {
                    self.set_zone_item_associated(target, &item, true);
                }
                xx_zone_v1::Event::ItemBlocked { item } | xx_zone_v1::Event::ItemLeft { item } => {
                    self.set_zone_item_associated(target, &item, false);
                }
                xx_zone_v1::Event::Handle { .. } => {}
                _ => unreachable!(),
            }
        }
    }

    fn zone_info(&self, target: ZoneTarget) -> Option<(XxZoneV1, bool, bool)> {
        match target {
            ZoneTarget::Fallback => self.zones.as_ref().map(|zones| {
                (
                    zones.fallback.zone.clone(),
                    zones.fallback.ready,
                    zones.fallback.valid,
                )
            }),
            ZoneTarget::Output(output) => self
                .world
                .get::<&OutputZone>(output)
                .ok()
                .map(|zone| (zone.0.zone.clone(), zone.0.ready, zone.0.valid)),
        }
    }

    fn zone_target_for_position(&self, entity: Entity, x: i32, y: i32) -> ZoneTarget {
        let current_output = self
            .world
            .get::<&OnOutput>(entity)
            .ok()
            .map(|output| output.0);
        let mut closest: Option<(i64, bool, Entity)> = None;

        for (output, (dimensions, scale, zone)) in self
            .world
            .query::<(&OutputDimensions, &OutputScaleFactor, &OutputZone)>()
            .iter()
        {
            // Xwayland sees output positions rebased to the top-left edge of the complete
            // layout. Zone positions, on the other hand, are local to the wl_output passed to
            // get_zone. Use the inverse of x_to_zone_position to find the owning output.
            let offset_x = dimensions.x - self.global_output_offset.x.value;
            let offset_y = dimensions.y - self.global_output_offset.y.value;
            let scale = scale.get();
            let width = if zone.0.width > 0 {
                (zone.0.width as f64 * scale) as i32
            } else {
                dimensions.width
            };
            let height = if zone.0.height > 0 {
                (zone.0.height as f64 * scale) as i32
            } else {
                dimensions.height
            };

            if x >= offset_x
                && x < offset_x.saturating_add(width)
                && y >= offset_y
                && y < offset_y.saturating_add(height)
            {
                return ZoneTarget::Output(output);
            }

            let dx = if x < offset_x {
                offset_x - x
            } else {
                x.saturating_sub(offset_x.saturating_add(width))
            };
            let dy = if y < offset_y {
                offset_y - y
            } else {
                y.saturating_sub(offset_y.saturating_add(height))
            };
            let distance = i64::from(dx).pow(2) + i64::from(dy).pow(2);
            let prefer_current = current_output == Some(output);
            if closest.is_none_or(|(best_distance, best_current, _)| {
                distance < best_distance
                    || (distance == best_distance && prefer_current && !best_current)
            }) {
                closest = Some((distance, prefer_current, output));
            }
        }

        closest
            .map(|(_, _, output)| ZoneTarget::Output(output))
            .unwrap_or(ZoneTarget::Fallback)
    }

    fn zone_transform(&self, target: ZoneTarget, window: &WindowData) -> (f64, WindowOutputOffset) {
        let ZoneTarget::Output(output) = target else {
            return (self.current_scale, window.output_offset);
        };
        let (Ok(dimensions), Ok(scale)) = (
            self.world.get::<&OutputDimensions>(output),
            self.world.get::<&OutputScaleFactor>(output),
        ) else {
            return (self.current_scale, window.output_offset);
        };
        // Do not use WindowData::output_offset here. A newly mapped window may not have received
        // wl_surface.enter yet, and an existing window may be changing zones between outputs.
        (
            scale.get(),
            WindowOutputOffset {
                x: dimensions.x - self.global_output_offset.x.value,
                y: dimensions.y - self.global_output_offset.y.value,
            },
        )
    }

    fn x_to_zone_position(
        &self,
        target: ZoneTarget,
        window: &WindowData,
        x: i32,
        y: i32,
    ) -> (i32, i32) {
        let (scale, offset) = self.zone_transform(target, window);
        (
            ((x - offset.x) as f64 / scale) as i32,
            ((y - offset.y) as f64 / scale) as i32,
        )
    }

    fn zone_to_x_position(
        &self,
        target: ZoneTarget,
        window: &WindowData,
        x: i32,
        y: i32,
    ) -> (i32, i32) {
        let (scale, offset) = self.zone_transform(target, window);
        (
            (x as f64 * scale) as i32 + offset.x,
            (y as f64 * scale) as i32 + offset.y,
        )
    }

    fn set_zone_item_associated(
        &mut self,
        target: ZoneTarget,
        item: &XxZoneItemV1,
        associated: bool,
    ) {
        let Some(entity) = item.data::<Entity>().copied() else {
            return;
        };
        let Ok(mut role) = self.world.get::<&mut SurfaceRole>(entity) else {
            return;
        };
        let SurfaceRole::Toplevel(Some(toplevel)) = &mut *role else {
            return;
        };
        if let Some(zone_item) = toplevel.zone_item.as_mut() {
            if zone_item.item == *item && zone_item.target == target {
                zone_item.associated = associated;
                zone_item.association_pending = false;
            }
        }
    }

    fn attach_zone_item(&mut self, entity: Entity, commit: bool) {
        let Ok(window) = self.world.get::<&WindowData>(entity) else {
            return;
        };
        let target = self.zone_target_for_position(
            entity,
            window.attrs.dims.x.into(),
            window.attrs.dims.y.into(),
        );
        let Some((zone, true, true)) = self.zone_info(target) else {
            return;
        };
        let manager = self.zones.as_ref().unwrap().manager.clone();
        drop(window);

        let Ok(mut query) = self.world.query_one::<(
            &WindowData,
            &mut SurfaceRole,
            &client::wl_surface::WlSurface,
        )>(entity) else {
            return;
        };
        let Some((window, role, surface)) = query.get() else {
            return;
        };
        let SurfaceRole::Toplevel(Some(toplevel)) = role else {
            return;
        };
        if toplevel.zone_item.is_some() {
            return;
        }

        let item = manager.get_zone_item(&toplevel.toplevel, &self.qh, entity);
        let association_pending = window.has_initial_position();
        if association_pending {
            zone.add_item(&item);
            let (x, y) = self.x_to_zone_position(
                target,
                window,
                window.attrs.dims.x.into(),
                window.attrs.dims.y.into(),
            );
            item.set_position(x, y);
        }
        toplevel.zone_item = Some(ZoneItemData {
            item,
            target,
            associated: false,
            association_pending,
        });
        if commit {
            surface.commit();
        }
    }

    fn remove_output(&mut self, global: GlobalName) {
        let query = self
            .world
            .query_mut::<(&WlOutput, &GlobalName)>()
            .into_iter()
            .map(|(e, (_, name))| (e, *name))
            .collect::<Vec<_>>();
        for (entity, name) in query.iter() {
            if *name == global {
                self.updated_outputs.push(*entity);
                self.world
                    .remove::<(OutputScaleFactor, OutputDimensions)>(*entity)
                    .unwrap();
                let query = self
                    .world
                    .query_mut::<&OnOutput>()
                    .into_iter()
                    .map(|(e, on_out)| (e, *on_out))
                    .collect::<Vec<_>>();
                for (e, on_out) in query.iter() {
                    if *on_out == OnOutput(*entity) {
                        self.world.remove_one::<OnOutput>(*e).unwrap();
                    }
                }
                if self.global_output_offset.x.owner == Some(*entity) {
                    self.global_offset_updated = true;
                    self.global_output_offset.x.owner = None;
                }
                if self.global_output_offset.y.owner == Some(*entity) {
                    self.global_offset_updated = true;
                    self.global_output_offset.y.owner = None;
                }
                break;
            }
        }
    }

    pub fn new_window(
        &mut self,
        window: x::Window,
        override_redirect: bool,
        dims: WindowDims,
        pid: Option<u32>,
    ) {
        let activation_token = pid
            .filter(|pid| self.pids.insert(*pid))
            .and_then(|pid| std::fs::read(format!("/proc/{pid}/environ")).ok())
            .and_then(|environ| {
                environ
                    .split(|byte| *byte == 0)
                    .find_map(|line| line.strip_prefix(b"XDG_ACTIVATION_TOKEN="))
                    .and_then(|token| String::from_utf8(token.to_vec()).ok())
            });

        let id = self.world.spawn((
            window,
            WindowData::new(override_redirect, dims, activation_token),
        ));

        self.windows.insert(window, id);
    }

    pub fn set_window_role(&mut self, window: x::Window, role: WindowRole) {
        let Some(id) = self.windows.get(&window).copied() else {
            debug!("not setting popup for unknown window {window:?}");
            return;
        };

        self.world.get::<&mut WindowData>(id).unwrap().attrs.role = role;
    }

    pub fn set_win_title(&mut self, window: x::Window, name: WmName) {
        let Some(data) = self
            .windows
            .get(&window)
            .copied()
            .and_then(|id| self.world.entity(id).ok())
        else {
            debug!("not setting title for unknown window {window:?}");
            return;
        };

        let mut win = data.get::<&mut WindowData>().unwrap();

        let new_title = match &mut win.attrs.title {
            Some(w) => {
                if matches!(w, WmName::NetWmName(_)) && matches!(name, WmName::WmName(_)) {
                    debug!(
                        "skipping setting window name to {name:?} because a _NET_WM_NAME title is already set"
                    );
                    None
                } else {
                    debug!("setting {window:?} title to {name:?}");
                    *w = name;
                    Some(w)
                }
            }
            None => Some(win.attrs.title.insert(name)),
        };

        let Some(title) = new_title else {
            return;
        };

        if let Some(mut role) = data.get::<&mut SurfaceRole>() {
            if let SurfaceRole::Toplevel(Some(data)) = &mut *role {
                data.toplevel.set_title(title.name().to_string());
                if let Some(d) = &mut data.decoration.satellite {
                    d.set_title(&self.world, title.name());
                }
            }
        }
    }

    pub fn set_win_class(&mut self, window: x::Window, class: String) {
        let Some(data) = self
            .windows
            .get(&window)
            .copied()
            .and_then(|id| self.world.entity(id).ok())
        else {
            debug!("not setting class for unknown window {window:?}");
            return;
        };

        let mut win = data.get::<&mut WindowData>().unwrap();

        let class = win.attrs.class.insert(class);
        if let Some(role) = data.get::<&SurfaceRole>() {
            if let SurfaceRole::Toplevel(Some(data)) = &*role {
                data.toplevel.set_app_id(class.to_string());
            }
        }
    }

    pub fn set_win_hints(&mut self, window: x::Window, hints: WmHints) {
        let Some(id) = self.windows.get(&window).copied() else {
            debug!("not setting hints for unknown window {window:?}");
            return;
        };

        let attrs = &mut self.world.get::<&mut WindowData>(id).unwrap().attrs;
        attrs.group = hints.window_group;
        attrs.acquire_input_via_wm = hints.acquire_input_via_wm;
    }

    pub fn set_take_focus(&mut self, window: x::Window, has_take_focus: bool) {
        let Some(id) = self.windows.get(&window).copied() else {
            debug!("not setting hints for unknown window {window:?}");
            return;
        };

        let attrs = &mut self.world.get::<&mut WindowData>(id).unwrap().attrs;
        attrs.has_take_focus = has_take_focus;
    }

    pub fn set_size_hints(&mut self, window: x::Window, hints: WmNormalHints) {
        let Some(data) = self
            .windows
            .get(&window)
            .copied()
            .and_then(|id| self.world.entity(id).ok())
        else {
            debug!("not setting size hints for unknown window {window:?}");
            return;
        };

        let mut win = data.get::<&mut WindowData>().unwrap();

        if win.attrs.size_hints.is_none_or(|h| h != hints) {
            debug!("setting {window:?} hints {hints:?}");
            let mut query = data.query::<(&SurfaceRole, &SurfaceScaleFactor)>();
            if let Some((SurfaceRole::Toplevel(Some(data)), scale_factor)) = query.get() {
                event::update_size_hints(data, &hints, scale_factor.0);
            }
            win.attrs.size_hints = Some(hints);
        }
    }

    pub fn set_win_decorations(&mut self, window: x::Window, decorations: Decorations) {
        let Some(data) = self
            .windows
            .get(&window)
            .copied()
            .and_then(|id| self.world.entity(id).ok())
        else {
            debug!("not setting decorations for unknown window {window:?}");
            return;
        };

        let mut win = data.get::<&mut WindowData>().unwrap();

        if win.attrs.decorations != Some(decorations) {
            debug!("setting {window:?} decorations {decorations:?}");
            if let Some(role) = data.get::<&SurfaceRole>() {
                if let SurfaceRole::Toplevel(Some(data)) = &*role {
                    if let Some(decoration) = &data.decoration.wl {
                        decoration.set_mode(decorations.into());
                    }
                }
            }
            win.attrs.decorations = Some(decorations);
        }
    }

    pub fn set_window_serial(&mut self, window: x::Window, serial: [u32; 2]) {
        let Some(id) = self.windows.get(&window).copied() else {
            warn!("Tried to set serial for unknown window {window:?}");
            return;
        };

        self.world.insert(id, (SurfaceSerial(serial),)).unwrap();
    }

    /// Requests an X11 window position through the zones protocol. Returns whether the caller
    /// should configure the X window's position directly instead.
    pub fn request_window_position(
        &mut self,
        window: x::Window,
        x: Option<i32>,
        y: Option<i32>,
    ) -> bool {
        let Some(entity) = self.windows.get(&window).copied() else {
            return true;
        };
        let Some(mut win) = self
            .windows
            .get(&window)
            .copied()
            .and_then(|id| self.world.entity(id).ok())
            .map(|data| data.get::<&mut WindowData>().unwrap())
        else {
            return true;
        };

        if win.attrs.role.is_popup() {
            return true;
        }

        if !win.mapped {
            win.record_position_request(x, y);
            return true;
        }
        drop(win);

        // X MapNotify and the xwayland-shell surface association are independent event streams.
        // A client can issue a ConfigureRequest after the X window is mapped but before its
        // xdg_toplevel and zone item exist. Preserve that request in X until the Wayland role is
        // ready instead of claiming it was handled by zones and silently dropping it.
        let has_toplevel = self
            .world
            .get::<&SurfaceRole>(entity)
            .ok()
            .is_some_and(|role| matches!(&*role, SurfaceRole::Toplevel(Some(_))));
        if !has_toplevel {
            let mut win = self.world.get::<&mut WindowData>(entity).unwrap();
            win.record_position_request(x, y);
            return true;
        }

        if self.zones.is_none() {
            // Preserve the old behavior on compositors without zones: mapped toplevels cannot
            // be positioned with xdg-shell alone.
            return false;
        }

        let window = self.world.get::<&WindowData>(entity).unwrap();
        let requested_x = x.unwrap_or(window.attrs.dims.x.into());
        let requested_y = y.unwrap_or(window.attrs.dims.y.into());
        drop(window);
        let target = self.zone_target_for_position(entity, requested_x, requested_y);
        let Some((zone, ready, valid)) = self.zone_info(target) else {
            return false;
        };
        if !ready {
            let mut win = self.world.get::<&mut WindowData>(entity).unwrap();
            win.record_position_request(x, y);
            return true;
        }
        if !valid {
            return false;
        }

        let old_target =
            self.world
                .get::<&SurfaceRole>(entity)
                .ok()
                .and_then(|role| match &*role {
                    SurfaceRole::Toplevel(Some(toplevel)) => {
                        toplevel.zone_item.as_ref().map(|item| item.target)
                    }
                    _ => None,
                });
        let old_zone = old_target.and_then(|target| self.zone_info(target).map(|info| info.0));

        if old_target.is_none() {
            self.attach_zone_item(entity, false);
        }

        let mut query = self
            .world
            .query_one::<(
                &WindowData,
                &mut SurfaceRole,
                &client::wl_surface::WlSurface,
            )>(entity)
            .unwrap();
        let Some((window, role, surface)) = query.get() else {
            return false;
        };
        let SurfaceRole::Toplevel(Some(toplevel)) = role else {
            return false;
        };
        let Some(zone_item) = toplevel.zone_item.as_mut() else {
            drop(query);
            let mut win = self.world.get::<&mut WindowData>(entity).unwrap();
            win.record_position_request(x, y);
            return true;
        };

        if zone_item.target != target {
            if zone_item.associated || zone_item.association_pending {
                if let Some(old_zone) = old_zone {
                    old_zone.remove_item(&zone_item.item);
                }
            }
            zone.add_item(&zone_item.item);
            zone_item.target = target;
            zone_item.associated = false;
            zone_item.association_pending = true;
        }
        if !zone_item.associated && !zone_item.association_pending {
            zone.add_item(&zone_item.item);
            zone_item.association_pending = true;
        }
        let (x, y) = self.x_to_zone_position(target, window, requested_x, requested_y);
        zone_item.item.set_position(x, y);
        surface.commit();
        false
    }

    pub fn reconfigure_window(&mut self, event: x::ConfigureNotifyEvent) {
        let Some((mut win, data)) = self
            .windows
            .get(&event.window())
            .copied()
            .and_then(|id| self.world.entity(id).ok())
            .and_then(|d| Some((d.get::<&mut WindowData>()?, d)))
        else {
            debug!("not reconfiguring unknown window {:?}", event.window());
            return;
        };

        let dims = WindowDims {
            x: event.x(),
            y: event.y(),
            width: event.width(),
            height: event.height(),
        };
        if dims == win.attrs.dims {
            return;
        } else if win.attrs.role.is_popup() {
            win.attrs.dims = dims;
        }

        debug!("Reconfiguring {:?} {:?}", event.window(), dims);

        if !win.mapped {
            win.attrs.dims = dims;
            return;
        }

        if self.xdg_wm_base.version() < 3 {
            return;
        }

        let mut query = data.query::<(&mut SurfaceRole, &SurfaceScaleFactor)>();
        let Some((role, scale_factor)) = query.get() else {
            return;
        };

        match role {
            SurfaceRole::Popup(Some(popup)) => {
                popup.positioner.set_offset(
                    ((event.x() as i32 - win.output_offset.x) as f64 / scale_factor.0) as i32,
                    ((event.y() as i32 - win.output_offset.y) as f64 / scale_factor.0) as i32,
                );
                popup.positioner.set_size(
                    1.max((event.width() as f64 / scale_factor.0) as i32),
                    1.max((event.height() as f64 / scale_factor.0) as i32),
                );
                popup.popup.reposition(&popup.positioner, 0);
            }
            SurfaceRole::Toplevel(Some(_)) => {
                win.attrs.dims.width = dims.width;
                win.attrs.dims.height = dims.height;
                drop(query);
                drop(win);
                update_surface_viewport(&self.world, self.world.query_one(data.entity()).unwrap());
            }
            other => warn!("Non popup ({other:?}) being reconfigured, behavior may be off."),
        }
    }

    pub fn map_window(&mut self, window: x::Window) {
        debug!("mapping {window:?}");

        let Some(mut win) = self
            .windows
            .get(&window)
            .copied()
            .and_then(|id| self.world.entity(id).ok())
            .map(|data| data.get::<&mut WindowData>().unwrap())
        else {
            debug!("not mapping unknown window {window:?}");
            return;
        };

        if win.mapped_once {
            // A withdrawn and remapped X window retains its X geometry, just as it would under a
            // traditional X11 window manager. This is distinct from trusting the initial (often
            // 0,0) geometry of a newly-created window.
            win.position_requested = true;
        }
        win.mapped_once = true;
        win.mapped = true;
    }

    pub fn unmap_window(&mut self, window: x::Window) {
        let entity = self.windows.get(&window).copied();

        {
            let Some(data) = entity.and_then(|id| self.world.entity(id).ok()) else {
                return;
            };

            let mut win = data.get::<&mut WindowData>().unwrap();
            if !win.mapped {
                return;
            }
            debug!("unmapping {window:?}");

            if matches!(self.last_focused_toplevel, Some(x) if x == window) {
                self.last_focused_toplevel.take();
            }
            if self.last_hovered == Some(window) {
                self.last_hovered.take();
            }
            win.mapped = false;
        }

        if let Ok(mut role) = self.world.remove_one::<SurfaceRole>(entity.unwrap()) {
            role.destroy();
        }
    }

    /// Returns the window to restore focus to when the active window is unmapped.
    /// If a toplevel was previously focused, returns it; otherwise returns `WINDOW_NONE`.
    pub fn focus_restore_target(&self) -> x::Window {
        self.last_focused_toplevel.unwrap_or(x::WINDOW_NONE)
    }

    pub fn set_fullscreen(&mut self, window: x::Window, state: super::xstate::SetState) {
        let Some(data) = self
            .windows
            .get(&window)
            .copied()
            .and_then(|id| self.world.entity(id).ok())
        else {
            warn!("Tried to set unknown window {window:?} fullscreen");
            return;
        };

        let Some(role) = data.get::<&SurfaceRole>() else {
            warn!("Tried to set window without role fullscreen: {window:?}");
            return;
        };

        let SurfaceRole::Toplevel(Some(toplevel)) = &*role else {
            warn!("Tried to set an unmapped toplevel or non toplevel fullscreen: {window:?}");
            return;
        };

        use crate::xstate::SetState;
        match state {
            SetState::Add => toplevel.toplevel.set_fullscreen(None),
            SetState::Remove => toplevel.toplevel.unset_fullscreen(),
            SetState::Toggle => {
                if toplevel.fullscreen {
                    toplevel.toplevel.unset_fullscreen()
                } else {
                    toplevel.toplevel.set_fullscreen(None)
                }
            }
        }
    }

    pub fn set_transient_for(&mut self, window: x::Window, parent: x::Window) {
        let Some(mut win) = self
            .windows
            .get(&window)
            .copied()
            .and_then(|id| self.world.entity(id).ok())
            .map(|data| data.get::<&mut WindowData>().unwrap())
        else {
            return;
        };

        win.attrs.transient_for = Some(parent);
    }

    pub fn activate_window(&mut self, window: x::Window) {
        let Some(activation_state) = self.activation_state.as_ref() else {
            return;
        };

        let Some(last_focused_toplevel) = self.last_focused_toplevel else {
            warn!("No last focused toplevel, cannot focus window {window:?}");
            return;
        };

        let Some(data) = self
            .windows
            .get(&last_focused_toplevel)
            .copied()
            .and_then(|id| self.world.entity(id).ok())
        else {
            warn!("Unknown last focused toplevel, cannot focus window {window:?}");
            return;
        };

        let Some(surface) = data.get::<&client::wl_surface::WlSurface>() else {
            warn!("Last focused toplevel has no surface, cannot focus window {window:?}");
            return;
        };
        activation_state.request_token_with_data(
            &self.qh,
            clientside::ActivationData::new(
                window,
                smithay_client_toolkit::activation::RequestData {
                    app_id: data.get::<&WindowData>().unwrap().attrs.class.clone(),
                    seat_and_serial: self.last_kb_serial.clone(),
                    surface: Some((*surface).clone()),
                },
            ),
        );
    }

    pub fn move_window(&mut self, window: x::Window) {
        let Some(data) = self
            .windows
            .get(&window)
            .copied()
            .and_then(|e| self.world.entity(e).ok())
        else {
            warn!("Requested move of unknown window {window:?}");
            return;
        };

        let Some(last_click_data) = data.get::<&LastClickSerial>() else {
            warn!("Requested move of window {window:?} but we don't have a click serial for it");
            return;
        };

        let role = data.get::<&SurfaceRole>();
        let Some(SurfaceRole::Toplevel(Some(data))) = role.as_deref() else {
            warn!("Requested move of non toplevel {window:?} ({role:?})");
            return;
        };

        data.toplevel._move(&last_click_data.0, last_click_data.1);
    }

    pub fn resize_window(&mut self, window: x::Window, direction: MoveResizeDirection) {
        let Some(data) = self
            .windows
            .get(&window)
            .copied()
            .and_then(|e| self.world.entity(e).ok())
        else {
            warn!("Requested resize of unknown window {window:?}");
            return;
        };

        let Some(last_click_data) = data.get::<&LastClickSerial>() else {
            warn!("Requested resize of window {window:?} but we don't have a click serial for it");
            return;
        };

        let role = data.get::<&SurfaceRole>();
        let Some(SurfaceRole::Toplevel(Some(data))) = role.as_deref() else {
            warn!("Requested resize of non toplevel {window:?} ({role:?})");
            return;
        };

        let edge = match direction {
            MoveResizeDirection::SizeTopLeft => xdg_toplevel::ResizeEdge::TopLeft,
            MoveResizeDirection::SizeTop => xdg_toplevel::ResizeEdge::Top,
            MoveResizeDirection::SizeTopRight => xdg_toplevel::ResizeEdge::TopRight,
            MoveResizeDirection::SizeRight => xdg_toplevel::ResizeEdge::Right,
            MoveResizeDirection::SizeBottomRight => xdg_toplevel::ResizeEdge::BottomRight,
            MoveResizeDirection::SizeBottom => xdg_toplevel::ResizeEdge::Bottom,
            MoveResizeDirection::SizeBottomLeft => xdg_toplevel::ResizeEdge::BottomLeft,
            MoveResizeDirection::SizeLeft => xdg_toplevel::ResizeEdge::Left,
            MoveResizeDirection::MoveKeyboard
            | MoveResizeDirection::SizeKeyboard
            | MoveResizeDirection::Move
            | MoveResizeDirection::Cancel => unreachable!(),
        };

        data.toplevel
            .resize(&last_click_data.0, last_click_data.1, edge);
    }

    pub fn destroy_window(&mut self, window: x::Window) {
        if let Some(id) = self.windows.remove(&window) {
            self.world.remove::<(x::Window, WindowData)>(id).unwrap();
            if self.world.entity(id).unwrap().is_empty() {
                self.world.despawn(id).unwrap();
            }
        }
    }

    pub fn new_global_scale(&mut self) -> Option<f64> {
        self.new_scale.take()
    }

    fn handle_activations(&mut self) {
        let Some(activation_state) = self.activation_state.as_ref() else {
            return;
        };

        self.world.pending_activations.retain(|(window, token)| {
            if let Some(surface) = self.windows.get(window).copied().and_then(|id| {
                self.world
                    .world
                    .get::<&client::wl_surface::WlSurface>(id)
                    .ok()
            }) {
                activation_state.activate::<Self>(&surface, token.clone());
                return false;
            }
            true
        });
    }

    fn calc_global_output_offset(&mut self) {
        self.global_output_offset.x.value = i32::MAX;
        self.global_output_offset.y.value = i32::MAX;
        for (entity, dimensions) in self.world.query_mut::<&OutputDimensions>() {
            if dimensions.x < self.global_output_offset.x.value {
                self.global_output_offset.x = GlobalOutputOffsetDimension {
                    owner: Some(entity),
                    value: dimensions.x,
                }
            }
            if dimensions.y < self.global_output_offset.y.value {
                self.global_output_offset.y = GlobalOutputOffsetDimension {
                    owner: Some(entity),
                    value: dimensions.y,
                }
            }
        }
    }

    /// Creates the appropriate xdg role (toplevel or popup) for the given window.
    /// Returns `true` if the created window is a toplevel.
    fn create_role_window(&mut self, window: x::Window, entity: Entity) -> bool {
        let xdg_surface;
        let mut popup_for = None;
        let mut fullscreen = false;
        let splash;

        {
            let data = self.world.entity(entity).unwrap();
            let surface = data.get::<&client::wl_surface::WlSurface>().unwrap();
            surface.attach(None, 0, 0);
            surface.commit();

            xdg_surface = self.xdg_wm_base.get_xdg_surface(&surface, &self.qh, entity);

            let window_data = data.get::<&WindowData>().unwrap();
            if window_data.attrs.role.is_popup() {
                popup_for = self.last_hovered.or(self.last_focused_toplevel);
            }
            splash = window_data.attrs.role == WindowRole::Splash;

            let (width, height) = (window_data.attrs.dims.width, window_data.attrs.dims.height);
            for (_, dimensions) in self.world.query::<&OutputDimensions>().iter() {
                if dimensions.width == width as i32 && dimensions.height == height as i32 {
                    fullscreen = true;
                    popup_for = None;
                    break;
                }
            }
        }

        let (role, is_toplevel) = if let Some(parent) = popup_for {
            let data = self.create_popup(entity, xdg_surface, parent);
            (SurfaceRole::Popup(Some(data)), false)
        } else {
            let data = self.create_toplevel(entity, xdg_surface, fullscreen, splash);
            (SurfaceRole::Toplevel(Some(data)), true)
        };

        let (surface_role, client) = self
            .world
            .query_one_mut::<(Option<&SurfaceRole>, &client::wl_surface::WlSurface)>(entity)
            .unwrap();

        let new_role_type = std::mem::discriminant(&role);
        if let Some(role) = surface_role {
            let old_role_type = std::mem::discriminant(role);
            assert_eq!(
                new_role_type, old_role_type,
                "Surface for {window:?} already had a role: {role:?}"
            );
        }

        client.commit();
        self.world.insert(entity, (role,)).unwrap();

        is_toplevel
    }

    fn create_toplevel(
        &mut self,
        entity: Entity,
        xdg: XdgSurface,
        fullscreen: bool,
        splash: bool,
    ) -> ToplevelData {
        let window = self.world.get::<&WindowData>(entity).unwrap();
        debug!(
            "creating toplevel for {:?} fullscreen: {fullscreen:?}",
            *self.world.get::<&x::Window>(entity).unwrap()
        );

        let toplevel = xdg.get_toplevel(&self.qh, entity);
        if let Some(hints) = &window.attrs.size_hints {
            if let Some(min) = &hints.min_size {
                toplevel.set_min_size(min.width, min.height);
            }
            if let Some(max) = &hints.max_size {
                toplevel.set_max_size(max.width, max.height);
            }
        }
        // Application splash windows are usually startup displays, so reporting their dimensions
        // as fixed has no downside. The upside is tiling Wayland compositors use fixed size as a
        // heurisitc to display those windows on a seperate floating level.
        // https://yalter.github.io/niri/Floating-Windows.html
        if splash {
            let dims = window.attrs.dims;
            toplevel.set_min_size(dims.width.into(), dims.height.into());
            toplevel.set_max_size(dims.width.into(), dims.height.into());
        }

        let group = window.attrs.group.and_then(|win| {
            let id = self.windows.get(&win).copied()?;
            Some(self.world.get::<&WindowData>(id).unwrap())
        });
        if let Some(class) = window
            .attrs
            .class
            .as_ref()
            .or(group.as_ref().and_then(|g| g.attrs.class.as_ref()))
        {
            toplevel.set_app_id(class.to_string());
        }
        if let Some(title) = window
            .attrs
            .title
            .as_ref()
            .or(group.as_ref().and_then(|g| g.attrs.title.as_ref()))
        {
            toplevel.set_title(title.name().to_string());
        }

        if fullscreen {
            toplevel.set_fullscreen(None);
        }

        let wl_decoration = self.decoration_manager.as_ref().map(|decoration_manager| {
            let decoration =
                decoration_manager.get_toplevel_decoration(&toplevel, &self.qh, entity);
            decoration.set_mode(
                window
                    .attrs
                    .decorations
                    .map_or(zxdg_toplevel_decoration_v1::Mode::ServerSide, From::from),
            );
            decoration
        });

        // X11 side wants server side decorations, but compositor won't provide them
        // so we provide our own

        let surface = self
            .world
            .get::<&client::wl_surface::WlSurface>(entity)
            .unwrap();
        let needs_satellite_decorations =
            wl_decoration.is_none() && window.attrs.decorations.is_none_or(|d| d.is_serverside());
        let (sat_decoration, buf) = needs_satellite_decorations
            .then(|| {
                DecorationsDataSatellite::try_new(
                    self,
                    &surface,
                    window.attrs.title.as_ref().map(WmName::name),
                )
            })
            .flatten()
            .unzip();

        if let (Some(activation_state), Some(token)) = (
            self.activation_state.as_ref(),
            window.activation_token.clone(),
        ) {
            activation_state.activate::<Self>(&surface, token);
        }

        if let Some(parent) = window.attrs.transient_for {
            // TODO: handle transient_for window not being mapped/not a toplevel
            'b: {
                let Some(parent_id) = self.windows.get(&parent).copied() else {
                    warn!(
                        "Window {:?} is marked transient for unknown window {:?}",
                        *self.world.get::<&x::Window>(entity).unwrap(),
                        parent
                    );
                    break 'b;
                };

                let role = self.world.get::<&SurfaceRole>(parent_id);
                let Ok(SurfaceRole::Toplevel(Some(parent_toplevel))) = role.as_deref() else {
                    warn!("Window {parent:?} was not an active toplevel, not setting as parent");
                    break 'b;
                };

                toplevel.set_parent(Some(&parent_toplevel.toplevel));
            }
        }

        let zone_target = self.zone_target_for_position(
            entity,
            window.attrs.dims.x.into(),
            window.attrs.dims.y.into(),
        );
        let zone_info = self.zone_info(zone_target);
        let zone_item = self.zones.as_ref().and_then(|zones| {
            zone_info.and_then(|(zone, ready, valid)| {
                (ready && valid).then(|| {
                    let item = zones.manager.get_zone_item(&toplevel, &self.qh, entity);
                    let association_pending = window.has_initial_position();
                    if association_pending {
                        zone.add_item(&item);
                        let (x, y) = self.x_to_zone_position(
                            zone_target,
                            &window,
                            window.attrs.dims.x.into(),
                            window.attrs.dims.y.into(),
                        );
                        item.set_position(x, y);
                    }
                    ZoneItemData {
                        item,
                        target: zone_target,
                        associated: false,
                        association_pending,
                    }
                })
            })
        });

        drop(window);
        drop(group);
        drop(surface);
        if let Some(mut b) = buf.flatten() {
            b.run_on(&mut self.world);
        }

        ToplevelData {
            xdg: XdgSurfaceData {
                surface: xdg,
                configured: false,
                pending: None,
            },
            toplevel,
            fullscreen: false,
            decoration: DecorationsData {
                wl: wl_decoration,
                satellite: sat_decoration,
            },
            zone_item,
        }
    }

    fn create_popup(&mut self, entity: Entity, xdg: XdgSurface, parent: x::Window) -> PopupData {
        let mut query = self
            .world
            .query_one::<(&WindowData, &mut SurfaceScaleFactor)>(entity)
            .unwrap();

        let (window, scale) = query.get().unwrap();
        let mut parent_query = self
            .world
            .query_one::<(&WindowData, &SurfaceScaleFactor, &SurfaceRole)>(self.windows[&parent])
            .unwrap();
        let (parent_window, parent_scale, parent_role) = parent_query.get().unwrap();
        let parent_dims = parent_window.attrs.dims;
        let initial_scale = parent_scale.0;
        *scale = *parent_scale;

        debug!(
            "creating popup ({:?}) {:?} {:?} {:?} {entity:?} (scale: {initial_scale})",
            *self.world.get::<&x::Window>(entity).unwrap(),
            parent,
            window.attrs.dims,
            xdg.id()
        );

        let positioner = self.xdg_wm_base.create_positioner(&self.qh, ());
        positioner.set_size(
            1.max((window.attrs.dims.width as f64 / initial_scale) as i32),
            1.max((window.attrs.dims.height as f64 / initial_scale) as i32),
        );
        let x = ((window.attrs.dims.x - parent_dims.x) as f64 / initial_scale) as i32;
        let y = ((window.attrs.dims.y - parent_dims.y) as f64 / initial_scale) as i32;
        positioner.set_offset(x, y);
        positioner.set_anchor(Anchor::TopLeft);
        positioner.set_gravity(Gravity::BottomRight);
        positioner.set_anchor_rect(
            0,
            0,
            (parent_window.attrs.dims.width as f64 / initial_scale) as i32,
            (parent_window.attrs.dims.height as f64 / initial_scale) as i32,
        );
        positioner
            .set_constraint_adjustment(ConstraintAdjustment::SlideX | ConstraintAdjustment::SlideY);
        let popup = xdg.get_popup(
            Some(&parent_role.xdg().unwrap().surface),
            &positioner,
            &self.qh,
            entity,
        );

        PopupData {
            popup,
            positioner,
            xdg: XdgSurfaceData {
                surface: xdg,
                configured: false,
                pending: None,
            },
        }
    }
}

#[derive(Default, Debug, Copy, Clone)]
pub struct PendingSurfaceState {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}
