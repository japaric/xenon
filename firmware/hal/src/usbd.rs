use pac::{CLOCK, POWER, USBD};

use crate::errata;

const READY_CLOCK: u8 = 1;
const READY_POWER: u8 = 1 << 1;
const READY_USB: u8 = 1 << 2;

static mut READY: u8 = 0;

#[allow(non_snake_case)]
#[no_mangle]
fn POWER_CLOCK() {
    // NOTE(unsafe) shared at the same priority level
    let ready = unsafe { &mut READY };

    POWER::borrow_unchecked(|power| {
        let usbdetected = power.EVENTS_USBDETECTED.read().bits();
        let usbremoved = power.EVENTS_USBREMOVED.read().bits();
        let usbpwrrdy = power.EVENTS_USBPWRRDY.read().bits();
        let hfclkstarted = CLOCK::borrow_unchecked(|clock| {
            clock.EVENTS_HFCLKSTARTED.read().bits()
        });

        semidap::trace!(
            "USBDETECTED: {}, USBREMOVED: {}, USBPWRRDY: {}, HFCLKSTARTED: {}",
            usbdetected,
            usbremoved,
            usbpwrrdy,
            hfclkstarted,
        );

        if usbdetected != 0 {
            power.EVENTS_USBDETECTED.zero();
            USBD::borrow_unchecked(|usbd| {
                // enable the USBD peripheral
                unsafe { errata::e187a() }
                usbd.ENABLE.write(|w| w.ENABLE(1));
            });
            CLOCK::borrow_unchecked(|clock| {
                // enable the external oscillator (crystal)
                clock.TASKS_HFCLKSTART.write(|w| w.TASKS_HFCLKSTART(1));
                *ready |= READY_CLOCK;
            });
            semidap::info!("enabled USB and started HFXO");
        }

        if hfclkstarted != 0 {
            CLOCK::borrow_unchecked(|clock| clock.EVENTS_HFCLKSTARTED.zero());
            semidap::info!("HFXO is stable");
        }

        if usbpwrrdy != 0 {
            power.EVENTS_USBPWRRDY.zero();
            *ready |= READY_POWER;
            semidap::info!("USB power supply ready");
        }

        if *ready == READY_CLOCK | READY_USB | READY_POWER {
            *ready = 0;
            USBD::borrow_unchecked(|usbd| {
                pullup(usbd);
            });
        }

        if usbremoved != 0 {
            semidap::abort();
        }
    });
}

// TODO enumeration
#[allow(non_snake_case)]
#[no_mangle]
fn USBD() {
    // NOTE(unsafe) shared at the same priority level
    let ready = unsafe { &mut READY };

    USBD::borrow_unchecked(|usbd| {
        let usbevent = usbd.EVENTS_USBEVENT.read().bits();
        let usbreset = usbd.EVENTS_USBRESET.read().bits();
        let started = usbd.EVENTS_STARTED.read().bits();
        let endepin0 = usbd.EVENTS_ENDEPIN0.read().bits();
        let ep0datadone = usbd.EVENTS_EP0DATADONE.read().bits();
        let endepout0 = usbd.EVENTS_ENDEPOUT0.read().bits();
        let ep0setup = usbd.EVENTS_EP0SETUP.read().bits();
        let epdata = usbd.EVENTS_EPDATA.read().bits();

        semidap::trace!(
            "USBRESET: {}, STARTED: {}, ENDEPIN0: {}, EP0DATADONE: {}, ENDEPOUT0: {}, USBEVENT: {}, EP0SETUP: {}, EPDATA: {}",
            usbreset,
            started,
            endepin0,
            ep0datadone,
            endepout0,
            usbevent,
            ep0setup,
            epdata,
        );

        if usbevent != 0 {
            usbd.EVENTS_USBEVENT.zero();
            let eventcause = usbd.EVENTCAUSE.read();
            semidap::trace!("{}", eventcause);

            if eventcause.READY() != 0 {
                usbd.EVENTCAUSE.write(|w| w.READY(1)); // clear
                unsafe { errata::e187b() }
                *ready |= READY_USB;
                semidap::info!("USB controller is ready");
            }

            if *ready == READY_CLOCK | READY_POWER | READY_USB {
                *ready = 0;
                pullup(&usbd);
            }

            if eventcause.SUSPEND() != 0 {
                usbd.EVENTCAUSE.write(|w| w.SUSPEND(1)); // clear
                usbd.LOWPOWER.write(|w| w.LOWPOWER(1));
            }

            if eventcause.RESUME() != 0 {
                usbd.LOWPOWER.write(|w| w.LOWPOWER(0));
                usbd.EVENTCAUSE.write(|w| w.RESUME(1)); // clear
            }

            if eventcause.USBWUALLOWED() != 0 || eventcause.ISOOUTCRC() != 0 {
                // TODO
                semidap::abort();
            }
        }

        if usbreset != 0
            || started != 0
            || endepin0 != 0
            || ep0datadone != 0
            || endepout0 != 0
            || ep0setup != 0
            || epdata != 0
        {
            // TODO
            semidap::abort();
        }
    });
}

fn pullup(usbd: &USBD) {
    usbd.USBPULLUP.write(|w| w.CONNECT(1));
    semidap::info!("pulled D+ up");
}
