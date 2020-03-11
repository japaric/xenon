use core::{
    cell::{Cell, UnsafeCell},
    cmp,
    convert::TryFrom,
    mem::MaybeUninit,
    ptr,
    sync::atomic::{self, Ordering},
};

use binfmt::derive::binDebug;
use pac::{CLOCK, POWER, USBD};
use usbd::{
    bMaxPacketSize0, bRequest, bcdUSB, DescriptorType, DeviceDesc, State,
};

use crate::errata;

const DEVICE_DESC: DeviceDesc = DeviceDesc {
    // Interface Association Descriptor
    bDeviceClass: 239,
    bDeviceSubClass: 2,
    bDeviceProtocol: 1,

    bMaxPacketSize0: bMaxPacketSize0::B64,
    bNumConfigurations: 1,
    bcdDevice: 0x00_00,
    bcdUSB: bcdUSB::V20,
    iManufacturer: 0,
    iProduct: 0,
    iSerialNumber: 0,
    idProduct: consts::PID,
    idVendor: consts::VID,
};

const READY_CLOCK: u8 = 1;
const READY_POWER: u8 = 1 << 1;
const READY_USB: u8 = 1 << 2;
static mut READY: Cell<u8> = Cell::new(0);

static mut STATE: Cell<State> = Cell::new(State::Default);
#[link_section = ".uninit.EP0BUFFER"]
static mut EP0BUFFER: UnsafeCell<MaybeUninit<[u8; 64]>> =
    UnsafeCell::new(MaybeUninit::uninit());
static mut EP0BUFFER_IN_USE: Cell<bool> = Cell::new(false);

#[allow(non_snake_case)]
#[derive(binDebug)]
struct PowerClockEvents {
    USBDETECTED: bool,
    USBREMOVED: bool,
    USBPWRRDY: bool,
    HFCLKSTARTED: bool,
}

#[allow(non_snake_case)]
#[no_mangle]
fn POWER_CLOCK() {
    // NOTE(unsafe) shared at the same priority level
    let ready = unsafe { &READY };

    POWER::borrow_unchecked(|power| {
        let usbdetected = power.EVENTS_USBDETECTED.read().bits();
        let usbremoved = power.EVENTS_USBREMOVED.read().bits();
        let usbpwrrdy = power.EVENTS_USBPWRRDY.read().bits();
        let hfclkstarted = CLOCK::borrow_unchecked(|clock| {
            clock.EVENTS_HFCLKSTARTED.read().bits()
        });

        semidap::trace!(
            "{}",
            PowerClockEvents {
                USBDETECTED: usbdetected != 0,
                USBREMOVED: usbremoved != 0,
                USBPWRRDY: usbpwrrdy != 0,
                HFCLKSTARTED: hfclkstarted != 0,
            }
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
                ready.set(ready.get() | READY_CLOCK);
            });
            semidap::info!("enabled USB and started HFXO");
        }

        if hfclkstarted != 0 {
            CLOCK::borrow_unchecked(|clock| clock.EVENTS_HFCLKSTARTED.zero());
            semidap::info!("HFXO is stable");
        }

        if usbpwrrdy != 0 {
            power.EVENTS_USBPWRRDY.zero();
            ready.set(ready.get() | READY_POWER);
            semidap::info!("USB power supply ready");
        }

        if ready.get() == READY_CLOCK | READY_USB | READY_POWER {
            ready.set(0);
            USBD::borrow_unchecked(|usbd| {
                pullup(usbd);
            });
        }

        if usbremoved != 0 {
            USBD::borrow_unchecked(|usbd| unimplemented(usbd))
        }
    });
}

#[allow(non_snake_case)]
#[derive(binDebug)]
struct UsbdEvents {
    ENDEPIN0: bool,
    ENDEPOUT0: bool,
    EP0DATADONE: bool,
    EP0SETUP: bool,
    EPDATA: bool,
    USBEVENT: bool,
    USBRESET: bool,
}

#[allow(non_snake_case)]
#[no_mangle]
fn USBD() {
    // NOTE(unsafe) shared at the same priority level
    let ready = unsafe { &READY };
    let state = unsafe { &STATE };
    let ep0buffer = unsafe { EP0BUFFER.get() as *mut u8 };
    let ep0buffer_in_use = unsafe { &EP0BUFFER_IN_USE };

    USBD::borrow_unchecked(|usbd| {
        let endepin0 = usbd.EVENTS_ENDEPIN0.read().bits();
        let endepout0 = usbd.EVENTS_ENDEPOUT0.read().bits();
        let ep0setup = usbd.EVENTS_EP0SETUP.read().bits();
        let epdata = usbd.EVENTS_EPDATA.read().bits();
        let usbevent = usbd.EVENTS_USBEVENT.read().bits();
        let usbreset = usbd.EVENTS_USBRESET.read().bits();
        let ep0datadone = usbd.EVENTS_EP0DATADONE.read().bits();

        semidap::trace!(
            "{}",
            UsbdEvents {
                USBRESET: usbreset != 0,
                ENDEPIN0: endepin0 != 0,
                ENDEPOUT0: endepout0 != 0,
                USBEVENT: usbevent != 0,
                EP0SETUP: ep0setup != 0,
                EPDATA: epdata != 0,
                EP0DATADONE: ep0datadone != 0,
            }
        );

        if usbreset != 0 {
            usbd.EVENTS_USBRESET.zero();
            // TODO cancel transfers; etc
            state.set(State::Default);

            semidap::info!("USB reset");
        }

        if endepin0 != 0 {
            semidap::info!("reclaiming buffer");

            // ownership of the buffer has been given back to us
            atomic::compiler_fence(Ordering::Acquire);
            usbd.EVENTS_ENDEPIN0.zero();

            ep0buffer_in_use.set(false);
        }

        if ep0setup != 0 {
            usbd.EVENTS_EP0SETUP.zero();

            let bmrequesttype = usbd.BMREQUESTTYPE.read();
            let brequest = usbd.BREQUEST.read();
            semidap::debug!("{}, {}", bmrequesttype, brequest);

            match (bmrequesttype.bits(), bRequest::from(brequest.bits())) {
                (0b1000_0000, bRequest::GET_DESCRIPTOR) => {
                    let desc_type = usbd.WVALUEH.read().bits();
                    let desc_index = usbd.WVALUEL.read().bits();
                    let language_id = u16::from(usbd.WINDEXL.read().bits())
                        | (u16::from(usbd.WINDEXH.read().bits()) << 8);
                    let wlength = u16::from(usbd.WLENGTHL.read().bits())
                        | (u16::from(usbd.WLENGTHH.read().bits()) << 8);

                    if let Ok(desc_type) = DescriptorType::try_from(desc_type) {
                        semidap::info!(
                            "SETUP: GET_DESCRIPTOR {} {} (lang={}, length={})",
                            desc_type,
                            desc_index,
                            language_id,
                            wlength
                        );

                        match desc_type {
                            DescriptorType::DEVICE
                                if desc_index == 0
                                    && language_id == 0
                                    && !ep0buffer_in_use.get() =>
                            {
                                // control read transfer
                                let bytes = DEVICE_DESC.bytes();
                                let desc_len = bytes.len();

                                if desc_len <= wlength.into() {
                                    // done in a single transfer
                                    usbd.SHORTS
                                        .rmw(|_, w| w.EP0DATADONE_EP0STATUS(1));

                                    if desc_len
                                        == DEVICE_DESC.bMaxPacketSize0 as usize
                                    {
                                        // short packet required
                                        unimplemented(usbd)
                                    }
                                }

                                let tlen = cmp::min(
                                    cmp::min(desc_len as u16, wlength),
                                    DEVICE_DESC.bMaxPacketSize0 as u16,
                                )
                                    as u8;

                                semidap::info!("sending {}B of data", tlen);

                                unsafe {
                                    ptr::copy_nonoverlapping(
                                        bytes.as_ptr(),
                                        ep0buffer,
                                        tlen.into(),
                                    );
                                    epin0(usbd, ep0buffer, tlen);
                                }
                            }

                            // not supported; we are a full-speed device
                            DescriptorType::DEVICE_QUALIFIER => stall0(usbd),

                            _ => unimplemented(usbd),
                        }
                    } else {
                        semidap::error!(
                            "GET_DESCRIPTOR with invalid descriptor type"
                        );
                        // XXX are we supposed to STALL or remain idle on
                        // invalid input?
                        stall0(usbd)
                    }
                }

                (0, bRequest::SET_ADDRESS) => {
                    let addr = u16::from(usbd.WVALUEL.read().bits())
                        | (u16::from(usbd.WVALUEH.read().bits()) << 8);
                    let windex = u16::from(usbd.WINDEXL.read().bits())
                        | (u16::from(usbd.WINDEXH.read().bits()) << 8);
                    let wlength = u16::from(usbd.WLENGTHL.read().bits())
                        | (u16::from(usbd.WLENGTHH.read().bits()) << 8);

                    if addr < 128 && windex == 0 && wlength == 0 {
                        // no need to issue a status stage; the peripheral takes
                        // care of that
                        state.set(State::Address);
                        semidap::info!("SET_ADDRESS {}", addr);
                    } else {
                        stall0(usbd)
                    }
                }

                _ => unimplemented(usbd),
            }
        }

        if usbevent != 0 {
            usbd.EVENTS_USBEVENT.zero();
            let eventcause = usbd.EVENTCAUSE.read();
            semidap::trace!("{}", eventcause);

            if eventcause.READY() != 0 {
                usbd.EVENTCAUSE.write(|w| w.READY(1)); // clear
                unsafe { errata::e187b() }
                ready.set(ready.get() | READY_USB);
                semidap::info!("USB controller is ready");
            }

            if ready.get() == READY_CLOCK | READY_POWER | READY_USB {
                ready.set(0);
                pullup(&usbd);
            }

            if eventcause.SUSPEND() != 0 {
                usbd.EVENTCAUSE.write(|w| w.SUSPEND(1)); // clear
                semidap::info!("entering low power mode");
                usbd.LOWPOWER.write(|w| w.LOWPOWER(1));
            }

            if eventcause.RESUME() != 0 {
                usbd.EVENTCAUSE.write(|w| w.RESUME(1)); // clear
                semidap::info!("leaving low power mode");
                usbd.LOWPOWER.write(|w| w.LOWPOWER(0));
            }

            if eventcause.USBWUALLOWED() != 0 || eventcause.ISOOUTCRC() != 0 {
                unimplemented(usbd)
            }
        }

        // TODO can we remove this and mask the interrupt?
        if ep0datadone != 0 {
            usbd.EVENTS_EP0DATADONE.zero();
            semidap::info!("data transmitted");
        }

        if endepout0 != 0 || epdata != 0 {
            unimplemented(usbd)
        }
    });
}

unsafe fn epin0(usbd: &USBD, p: *const u8, len: u8) {
    usbd.EPIN0_MAXCNT.write(|w| w.MAXCNT(len));
    usbd.EPIN0_PTR.write(|w| w.PTR(p as u32));

    // the next write transfer ownership of the buffer to the DMA
    atomic::compiler_fence(Ordering::Release);
    usbd.TASKS_STARTEPIN0.write(|w| w.TASKS_STARTEPIN(1));
}

fn pullup(usbd: &USBD) {
    usbd.USBPULLUP.write(|w| w.CONNECT(1));
    semidap::info!("pulled D+ up");
}

fn stall0(usbd: &USBD) {
    semidap::info!("stalling EP0");
    usbd.TASKS_EP0STALL.write(|w| w.TASKS_EP0STALL(1));
}

fn unimplemented(usbd: &USBD) -> ! {
    // simulate a disconnect so the host doesn't retry enumeration while the
    // device is halted
    usbd.USBPULLUP.zero();
    semidap::panic!("unimplemented; detaching from the bus")
}