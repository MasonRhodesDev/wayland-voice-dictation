use std::cell::RefCell;
use std::time::Duration;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol};
use objc2::{define_class, msg_send, sel, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSControlStateValueOff, NSControlStateValueOn, NSEventMask,
    NSEventModifierFlags, NSEventType, NSImage, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem,
    NSVariableStatusItemLength,
};
use objc2_foundation::NSString;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::info;

use super::ui::ui_loop_running;
use crate::audio_backend::{list_devices_backend, BackendType, DeviceInfo};
use crate::control::{DaemonCommand, DaemonState};
use crate::platform::tray_menu::{TrayAction, TrayEntry, TrayModel};

const TITLE: &str = "Voice Dictation";
const DEVICE_REFRESH: Duration = Duration::from_secs(30);

thread_local! {
    static TRAY: RefCell<Option<Tray>> = const { RefCell::new(None) };
}

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    struct MenuTarget;

    unsafe impl NSObjectProtocol for MenuTarget {}

    impl MenuTarget {
        #[unsafe(method(activated:))]
        fn activated(&self, sender: &NSMenuItem) {
            activate(sender.tag());
        }

        #[unsafe(method(clicked:))]
        fn clicked(&self, _sender: Option<&AnyObject>) {
            click(self.mtm());
        }
    }
);

impl MenuTarget {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

struct Tray {
    status_item: Retained<NSStatusItem>,
    menu: Retained<NSMenu>,
    target: Retained<MenuTarget>,
    model: TrayModel,
    shown: Option<(DaemonState, Vec<TrayEntry>)>,
    actions: Vec<TrayAction>,
    command_tx: mpsc::Sender<DaemonCommand>,
}

fn status_symbol(state: DaemonState) -> (&'static str, &'static str) {
    match state {
        DaemonState::Idle => ("mic", "VD"),
        DaemonState::Recording => ("mic.fill", "REC"),
        DaemonState::Processing => ("ellipsis", "..."),
    }
}

impl Tray {
    fn render(&mut self, mtm: MainThreadMarker) {
        let entries = self.model.entries();
        let state = self.model.state;
        if self
            .shown
            .as_ref()
            .is_some_and(|(shown_state, shown)| *shown_state == state && *shown == entries)
        {
            return;
        }

        self.menu.removeAllItems();
        self.actions.clear();
        for entry in &entries {
            let item = match entry {
                TrayEntry::Item { label, enabled, action } => {
                    self.action_item(mtm, label, *enabled, false, action.clone())
                }
                TrayEntry::Devices { label, options, selected } => {
                    let devices = NSMenu::new(mtm);
                    devices.setAutoenablesItems(false);
                    for (index, option) in options.iter().enumerate() {
                        devices.addItem(&self.action_item(
                            mtm,
                            option,
                            true,
                            index == *selected,
                            TrayAction::SelectDevice(index),
                        ));
                    }
                    let submenu = NSMenuItem::new(mtm);
                    submenu.setTitle(&NSString::from_str(label));
                    submenu.setSubmenu(Some(&devices));
                    submenu
                }
                TrayEntry::Separator => NSMenuItem::separatorItem(mtm),
            };
            self.menu.addItem(&item);
        }

        if let Some(button) = self.status_item.button(mtm) {
            let (symbol, text) = status_symbol(state);
            let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(
                &NSString::from_str(symbol),
                Some(&NSString::from_str(TITLE)),
            );
            if let Some(image) = &image {
                image.setTemplate(true);
            }
            button.setImage(image.as_deref());
            button.setTitle(&NSString::from_str(if image.is_some() { "" } else { text }));
            button.setToolTip(Some(&NSString::from_str(&format!("{TITLE}: {state}"))));
        }

        self.shown = Some((state, entries));
    }

    fn action_item(
        &mut self,
        mtm: MainThreadMarker,
        label: &str,
        enabled: bool,
        checked: bool,
        action: TrayAction,
    ) -> Retained<NSMenuItem> {
        let item = NSMenuItem::new(mtm);
        item.setTitle(&NSString::from_str(label));
        item.setEnabled(enabled);
        item.setState(if checked { NSControlStateValueOn } else { NSControlStateValueOff });
        item.setTag(self.actions.len() as isize);
        let target: &AnyObject = &self.target;
        unsafe {
            item.setTarget(Some(target));
            item.setAction(Some(sel!(activated:)));
        }
        self.actions.push(action);
        item
    }
}

fn install(model: TrayModel, command_tx: mpsc::Sender<DaemonCommand>) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let status_item =
        NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    let menu = NSMenu::new(mtm);
    menu.setAutoenablesItems(false);
    let target = MenuTarget::new(mtm);
    if let Some(button) = status_item.button(mtm) {
        let receiver: &AnyObject = &target;
        unsafe {
            button.setTarget(Some(receiver));
            button.setAction(Some(sel!(clicked:)));
        }
        button.sendActionOn(NSEventMask::LeftMouseUp | NSEventMask::RightMouseUp);
    }
    let mut tray =
        Tray { status_item, menu, target, model, shown: None, actions: Vec::new(), command_tx };
    tray.render(mtm);
    TRAY.with_borrow_mut(|slot| *slot = Some(tray));
    info!("Menu bar status item active");
}

fn update(state: Option<DaemonState>, devices: Option<Vec<DeviceInfo>>) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    TRAY.with_borrow_mut(|tray| {
        let Some(tray) = tray else {
            return;
        };
        if let Some(state) = state {
            tray.model.state = state;
        }
        if let Some(devices) = devices {
            tray.model.devices = devices;
        }
        tray.render(mtm);
    });
}

fn click(mtm: MainThreadMarker) {
    let wants_menu = NSApplication::sharedApplication(mtm).currentEvent().is_some_and(|event| {
        event.r#type() == NSEventType::RightMouseUp
            || event.modifierFlags().contains(NSEventModifierFlags::Control)
    });
    if !wants_menu {
        TRAY.with_borrow_mut(|tray| {
            if let Some(tray) = tray {
                tray.model.activate(&tray.command_tx);
            }
        });
        return;
    }
    let Some((status_item, menu)) = TRAY.with_borrow(|tray| {
        tray.as_ref().map(|tray| (tray.status_item.clone(), tray.menu.clone()))
    }) else {
        return;
    };
    status_item.setMenu(Some(&menu));
    if let Some(button) = status_item.button(mtm) {
        unsafe { button.performClick(None) };
    }
    status_item.setMenu(None);
}

fn activate(tag: isize) {
    TRAY.with_borrow_mut(|tray| {
        let Some(tray) = tray else {
            return;
        };
        let action = usize::try_from(tag).ok().and_then(|index| tray.actions.get(index)).cloned();
        if let Some(action) = action {
            tray.model.perform(action, &tray.command_tx);
        }
    });
    slint_gui::run_on_ui_thread(|| update(None, None));
}

pub fn spawn_tray(
    mut state_rx: watch::Receiver<DaemonState>,
    command_tx: mpsc::Sender<DaemonCommand>,
    backend_type: BackendType,
    initial_device: Option<String>,
) -> Option<JoinHandle<()>> {
    if !ui_loop_running() {
        return None;
    }
    Some(tokio::spawn(async move {
        let model = TrayModel {
            state: *state_rx.borrow_and_update(),
            devices: list_devices_backend(backend_type).unwrap_or_default(),
            selected_device: initial_device,
        };
        slint_gui::run_on_ui_thread(move || install(model, command_tx));

        let mut refresh = tokio::time::interval(DEVICE_REFRESH);
        refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        refresh.tick().await;

        loop {
            let state = tokio::select! {
                changed = state_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    Some(*state_rx.borrow_and_update())
                }
                _ = refresh.tick() => None,
            };
            let devices = match state {
                Some(DaemonState::Recording | DaemonState::Processing) => None,
                Some(DaemonState::Idle) | None => list_devices_backend(backend_type).ok(),
            };
            slint_gui::run_on_ui_thread(move || update(state, devices));
        }
    }))
}
