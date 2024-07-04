use anyhow::{anyhow, Result};
use futures::StreamExt;
use once_cell::sync::Lazy;
use std::{
    collections::HashMap,
    io,
    os::{fd::OwnedFd, unix::net::UnixStream},
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc, RwLock,
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::task::JoinHandle;

use ashpd::{
    desktop::{
        remote_desktop::{DeviceType, RemoteDesktop},
        ResponseError,
    },
    WindowIdentifier,
};
use async_trait::async_trait;

use reis::{
    ei::{
        self, button::ButtonState, handshake::ContextType, keyboard::KeyState, Button, Keyboard,
        Pointer, Scroll,
    },
    event::{DeviceCapability, DeviceEvent, EiEvent, SeatEvent},
    tokio::{ei_handshake, EiConvertEventStream, EiEventStream},
};

use input_event::{Event, KeyboardEvent, PointerEvent};

use crate::error::EmulationError;

use super::{error::LibeiEmulationCreationError, EmulationHandle, InputEmulation};

static INTERFACES: Lazy<HashMap<&'static str, u32>> = Lazy::new(|| {
    let mut m = HashMap::new();
    m.insert("ei_connection", 1);
    m.insert("ei_callback", 1);
    m.insert("ei_pingpong", 1);
    m.insert("ei_seat", 1);
    m.insert("ei_device", 2);
    m.insert("ei_pointer", 1);
    m.insert("ei_pointer_absolute", 1);
    m.insert("ei_scroll", 1);
    m.insert("ei_button", 1);
    m.insert("ei_keyboard", 1);
    m.insert("ei_touchscreen", 1);
    m
});

#[derive(Clone, Default)]
struct Devices {
    pointer: Arc<RwLock<Option<(ei::Device, ei::Pointer)>>>,
    scroll: Arc<RwLock<Option<(ei::Device, ei::Scroll)>>>,
    button: Arc<RwLock<Option<(ei::Device, ei::Button)>>>,
    keyboard: Arc<RwLock<Option<(ei::Device, ei::Keyboard)>>>,
}

pub struct LibeiEmulation {
    context: ei::Context,
    devices: Devices,
    serial: AtomicU32,
    ei_task: JoinHandle<Result<()>>,
}

async fn get_ei_fd() -> Result<OwnedFd, ashpd::Error> {
    let proxy = RemoteDesktop::new().await?;

    // retry when user presses the cancel button
    let (session, _) = loop {
        log::debug!("creating session ...");
        let session = proxy.create_session().await?;

        log::debug!("selecting devices ...");
        proxy
            .select_devices(&session, DeviceType::Keyboard | DeviceType::Pointer)
            .await?;

        log::info!("requesting permission for input emulation");
        match proxy
            .start(&session, &WindowIdentifier::default())
            .await?
            .response()
        {
            Ok(d) => break (session, d),
            Err(ashpd::Error::Response(ResponseError::Cancelled)) => {
                log::warn!("request cancelled!");
                continue;
            }
            e => e?,
        };
    };

    proxy.connect_to_eis(&session).await
}

impl LibeiEmulation {
    pub async fn new() -> Result<Self, LibeiEmulationCreationError> {
        let eifd = get_ei_fd().await?;
        let stream = UnixStream::from(eifd);
        stream.set_nonblocking(true)?;
        let context = ei::Context::new(stream)?;
        context.flush().map_err(|e| io::Error::new(e.kind(), e))?;
        let mut events = EiEventStream::new(context.clone())?;
        let handshake = ei_handshake(
            &mut events,
            "de.feschber.LanMouse",
            ContextType::Sender,
            &INTERFACES,
        )
        .await?;
        let events = EiConvertEventStream::new(events, handshake.serial);
        let devices = Devices::default();
        let ei_task =
            tokio::task::spawn_local(ei_event_handler(events, context.clone(), devices.clone()));

        let serial = AtomicU32::new(handshake.serial);

        Ok(Self {
            serial,
            context,
            ei_task,
            devices,
        })
    }
}

impl Drop for LibeiEmulation {
    fn drop(&mut self) {
        self.ei_task.abort();
    }
}

#[async_trait]
impl InputEmulation for LibeiEmulation {
    async fn consume(
        &mut self,
        event: Event,
        _handle: EmulationHandle,
    ) -> Result<(), EmulationError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_micros() as u64;
        match event {
            Event::Pointer(p) => match p {
                PointerEvent::Motion {
                    time: _,
                    relative_x,
                    relative_y,
                } => {
                    let pointer_device = self.devices.pointer.read().unwrap();
                    if let Some((d, p)) = pointer_device.as_ref() {
                        p.motion_relative(relative_x as f32, relative_y as f32);
                        d.frame(self.serial.load(Ordering::SeqCst), now);
                    }
                }
                PointerEvent::Button {
                    time: _,
                    button,
                    state,
                } => {
                    let button_device = self.devices.button.read().unwrap();
                    if let Some((d, b)) = button_device.as_ref() {
                        b.button(
                            button,
                            match state {
                                0 => ButtonState::Released,
                                _ => ButtonState::Press,
                            },
                        );
                        d.frame(self.serial.load(Ordering::SeqCst), now);
                    }
                }
                PointerEvent::Axis {
                    time: _,
                    axis,
                    value,
                } => {
                    let scroll_device = self.devices.scroll.read().unwrap();
                    if let Some((d, s)) = scroll_device.as_ref() {
                        match axis {
                            0 => s.scroll(0., value as f32),
                            _ => s.scroll(value as f32, 0.),
                        }
                        d.frame(self.serial.load(Ordering::SeqCst), now);
                    }
                }
                PointerEvent::AxisDiscrete120 { axis, value } => {
                    let scroll_device = self.devices.scroll.read().unwrap();
                    if let Some((d, s)) = scroll_device.as_ref() {
                        match axis {
                            0 => s.scroll_discrete(0, value),
                            _ => s.scroll_discrete(value, 0),
                        }
                        d.frame(self.serial.load(Ordering::SeqCst), now);
                    }
                }
                PointerEvent::Frame {} => {}
            },
            Event::Keyboard(k) => match k {
                KeyboardEvent::Key {
                    time: _,
                    key,
                    state,
                } => {
                    let keyboard_device = self.devices.keyboard.read().unwrap();
                    if let Some((d, k)) = keyboard_device.as_ref() {
                        k.key(
                            key,
                            match state {
                                0 => KeyState::Released,
                                _ => KeyState::Press,
                            },
                        );
                        d.frame(self.serial.load(Ordering::SeqCst), now);
                    }
                }
                KeyboardEvent::Modifiers { .. } => {}
            },
            _ => {}
        }
        self.context
            .flush()
            .map_err(|e| io::Error::new(e.kind(), e))?;
        Ok(())
    }

    async fn create(&mut self, _: EmulationHandle) {}
    async fn destroy(&mut self, _: EmulationHandle) {}
}

async fn ei_event_handler(
    mut events: EiConvertEventStream,
    context: ei::Context,
    devices: Devices,
) -> Result<()> {
    loop {
        let event = events
            .next()
            .await
            .ok_or(anyhow!("ei stream closed"))?
            .map_err(|e| anyhow!("libei error: {e:?}"))?;
        const CAPABILITIES: &[DeviceCapability] = &[
            DeviceCapability::Pointer,
            DeviceCapability::PointerAbsolute,
            DeviceCapability::Keyboard,
            DeviceCapability::Touch,
            DeviceCapability::Scroll,
            DeviceCapability::Button,
        ];
        log::debug!("{event:?}");
        match event {
            EiEvent::Disconnected(e) => {
                log::debug!("ei disconnected: {e:?}");
                break;
            }
            EiEvent::SeatAdded(e) => {
                e.seat().bind_capabilities(CAPABILITIES);
            }
            EiEvent::SeatRemoved(e) => {
                log::debug!("seat removed: {:?}", e.seat());
            }
            EiEvent::DeviceAdded(e) => {
                let device_type = e.device().device_type();
                log::debug!("device added: {device_type:?}");
                e.device().device();
                let device = e.device();
                if let Some(pointer) = e.device().interface::<Pointer>() {
                    devices
                        .pointer
                        .write()
                        .unwrap()
                        .replace((device.device().clone(), pointer));
                }
                if let Some(keyboard) = e.device().interface::<Keyboard>() {
                    devices
                        .keyboard
                        .write()
                        .unwrap()
                        .replace((device.device().clone(), keyboard));
                }
                if let Some(scroll) = e.device().interface::<Scroll>() {
                    devices
                        .scroll
                        .write()
                        .unwrap()
                        .replace((device.device().clone(), scroll));
                }
                if let Some(button) = e.device().interface::<Button>() {
                    devices
                        .button
                        .write()
                        .unwrap()
                        .replace((device.device().clone(), button));
                }
            }
            EiEvent::DeviceRemoved(e) => {
                log::debug!("device removed: {:?}", e.device().device_type());
            }
            EiEvent::DevicePaused(e) => {
                log::debug!("device paused: {:?}", e.device().device_type());
            }
            EiEvent::DeviceResumed(e) => {
                log::debug!("device resumed: {:?}", e.device().device_type());
                e.device().device().start_emulating(0, 0);
            }
            EiEvent::KeyboardModifiers(e) => {
                log::debug!("modifiers: {e:?}");
            }
            // only for receiver context
            // EiEvent::Frame(_) => { },
            // EiEvent::DeviceStartEmulating(_) => { },
            // EiEvent::DeviceStopEmulating(_) => { },
            // EiEvent::PointerMotion(_) => { },
            // EiEvent::PointerMotionAbsolute(_) => { },
            // EiEvent::Button(_) => { },
            // EiEvent::ScrollDelta(_) => { },
            // EiEvent::ScrollStop(_) => { },
            // EiEvent::ScrollCancel(_) => { },
            // EiEvent::ScrollDiscrete(_) => { },
            // EiEvent::KeyboardKey(_) => { },
            // EiEvent::TouchDown(_) => { },
            // EiEvent::TouchUp(_) => { },
            // EiEvent::TouchMotion(_) => { },
            _ => unreachable!("unexpected ei event"),
        }
        context.flush()?;
    }
    Ok(())
}
