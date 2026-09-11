use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use calloop::generic::Generic;
use calloop::timer::{TimeoutAction, Timer};
use calloop::{EventLoop, Interest, LoopHandle, Mode, PostAction};
use gpui_util::ResultExt;

use super::egl::EglWindow;
use super::input::{self, KeyboardState};
use super::window::{FbdevDisplay, FbdevWindow};
use crate::linux::{LinuxClient, LinuxCommon, LinuxKeyboardLayout};
use gpui::{
    AnyWindowHandle, CursorStyle, DisplayId, PlatformDisplay, PlatformInput,
    PlatformKeyboardLayout, PlatformWindow, RequestFrameOptions, WindowParams,
};

const FRAME_INTERVAL: Duration = Duration::from_micros(16_667);

pub(crate) struct FbdevClientState {
    _loop_handle: LoopHandle<'static, FbdevClient>,
    event_loop: Option<EventLoop<'static, FbdevClient>>,
    common: LinuxCommon,
    display: Rc<dyn PlatformDisplay>,
    /// Created up front so the display size is known before the window opens; handed to the
    /// only window the framebuffer can show.
    egl: Option<EglWindow>,
    window: Option<FbdevWindow>,
    window_handle: Option<AnyWindowHandle>,
    keyboard: KeyboardState,
}

#[derive(Clone)]
pub(crate) struct FbdevClient(Rc<RefCell<FbdevClientState>>);

impl FbdevClient {
    pub(crate) fn new() -> anyhow::Result<Self> {
        let egl = EglWindow::new().context("Failed to create an EGL framebuffer surface")?;
        let (width, height) = egl.size();
        log::info!("Framebuffer is {width}x{height}");

        let event_loop = EventLoop::try_new()?;
        let (common, main_receiver, wake_receiver) = LinuxCommon::new(event_loop.get_signal());
        let handle = event_loop.handle();

        handle
            .insert_source(main_receiver, |event, _, _: &mut FbdevClient| {
                if let calloop::channel::Event::Msg(runnable) = event {
                    runnable.run();
                }
            })
            .map_err(|error| error.error)?;

        handle
            .insert_source(wake_receiver, |event, _, client: &mut FbdevClient| {
                if let calloop::channel::Event::Msg(()) = event {
                    client.with_common(|common| common.handle_system_wake());
                }
            })
            .map_err(|error| error.error)?;

        // Without a compositor there is no frame callback, so frames are requested on a fixed
        // cadence; presentation blocks on vsync (swap interval 1) anyway.
        handle
            .insert_source(Timer::immediate(), |deadline, _, client: &mut FbdevClient| {
                let (window, repeat) = {
                    let mut state = client.0.borrow_mut();
                    let repeat = state.keyboard.due_repeat(Instant::now());
                    (state.window.clone(), repeat)
                };
                if let Some(window) = window {
                    if let Some(repeat) = repeat {
                        window.handle_input(repeat);
                    }
                    window.refresh(RequestFrameOptions {
                        require_presentation: false,
                        force_render: false,
                    });
                }
                let now = Instant::now();
                let mut next_frame = deadline;
                while next_frame <= now {
                    next_frame += FRAME_INTERVAL;
                }
                TimeoutAction::ToInstant(next_frame)
            })
            .map_err(|error| error.error)?;

        let devices = input::open_devices();
        if devices.is_empty() {
            log::warn!("No keyboard or gamepad found under /dev/input");
        }
        for (file, mut device) in devices {
            handle
                .insert_source(
                    Generic::new(file, Interest::READ, Mode::Level),
                    move |_, file, client: &mut FbdevClient| {
                        let mut events = Vec::new();
                        let result = device.read_events(
                            file,
                            &mut client.0.borrow_mut().keyboard,
                            &mut events,
                        );
                        client.dispatch_input(events);
                        match result {
                            Ok(()) => Ok(PostAction::Continue),
                            Err(error) => {
                                log::error!("Stopped reading input device {}: {error}", device.name);
                                Ok(PostAction::Remove)
                            }
                        }
                    },
                )
                .map_err(|error| error.error)?;
        }

        Ok(Self(Rc::new(RefCell::new(FbdevClientState {
            _loop_handle: handle,
            event_loop: Some(event_loop),
            common,
            display: Rc::new(FbdevDisplay::new(width, height)),
            egl: Some(egl),
            window: None,
            window_handle: None,
            keyboard: KeyboardState::default(),
        }))))
    }

    fn dispatch_input(&self, events: Vec<PlatformInput>) {
        if events.is_empty() {
            return;
        }
        let window = self.0.borrow().window.clone();
        if let Some(window) = window {
            for event in events {
                window.handle_input(event);
            }
        }
    }
}

impl LinuxClient for FbdevClient {
    fn with_common<R>(&self, f: impl FnOnce(&mut LinuxCommon) -> R) -> R {
        f(&mut self.0.borrow_mut().common)
    }

    fn keyboard_layout(&self) -> Box<dyn PlatformKeyboardLayout> {
        Box::new(LinuxKeyboardLayout::new("unknown".into()))
    }

    fn displays(&self) -> Vec<Rc<dyn PlatformDisplay>> {
        vec![self.0.borrow().display.clone()]
    }

    fn primary_display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(self.0.borrow().display.clone())
    }

    fn display(&self, id: DisplayId) -> Option<Rc<dyn PlatformDisplay>> {
        let display = self.0.borrow().display.clone();
        (display.id() == id).then_some(display)
    }

    fn active_window(&self) -> Option<AnyWindowHandle> {
        self.0.borrow().window_handle
    }

    fn window_stack(&self) -> Option<Vec<AnyWindowHandle>> {
        None
    }

    fn open_window(
        &self,
        handle: AnyWindowHandle,
        _params: WindowParams,
    ) -> anyhow::Result<Box<dyn PlatformWindow>> {
        let mut state = self.0.borrow_mut();
        let egl = state
            .egl
            .take()
            .context("The framebuffer platform can only show a single window")?;
        let window = FbdevWindow::new(egl, state.display.clone())?;
        state.window = Some(window.clone());
        state.window_handle = Some(handle);
        Ok(Box::new(window))
    }

    fn compositor_name(&self) -> &'static str {
        "fbdev"
    }

    fn set_cursor_style(&self, _style: CursorStyle) {}

    fn open_uri(&self, _uri: &str) {}

    fn reveal_path(&self, _path: std::path::PathBuf) {}

    fn write_to_primary(&self, _item: gpui::ClipboardItem) {}

    fn write_to_clipboard(&self, _item: gpui::ClipboardItem) {}

    fn read_from_primary(&self) -> Option<gpui::ClipboardItem> {
        None
    }

    fn read_from_clipboard(&self) -> Option<gpui::ClipboardItem> {
        None
    }

    fn run(&self) {
        let mut event_loop = self
            .0
            .borrow_mut()
            .event_loop
            .take()
            .expect("App is already running");

        event_loop.run(None, &mut self.clone(), |_| {}).log_err();
    }
}
