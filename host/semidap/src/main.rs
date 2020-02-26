#![deny(warnings)]

use core::{
    cmp,
    convert::{TryFrom, TryInto},
    fmt, mem,
    ops::Range,
    str,
    sync::atomic::{AtomicBool, Ordering},
};
use std::{
    borrow::Cow,
    collections::btree_map::{self, BTreeMap},
    fs,
    io::{self, Write},
    path::PathBuf,
    process,
    time::Instant,
};

use anyhow::{anyhow, bail};
use cm::scb::{cpuid, CPUID};
use cmsis_dap::{cortex_m, Dap};
use gimli::{
    read::{CfaRule, DebugFrame, UnwindSection},
    BaseAddresses, EndianSlice, LittleEndian, RegisterRule,
    UninitializedUnwindContext,
};
use log::{debug, error, info};
use rustyline::Editor;
use structopt::StructOpt;
use xmas_elf::{
    sections::{SectionData, SHF_ALLOC},
    symbol_table::Entry,
    ElfFile,
};

#[derive(StructOpt)]
struct Opts {
    #[structopt(short, long, parse(try_from_str = parse_hex))]
    vendor: u16,

    #[structopt(short, long, parse(try_from_str = parse_hex))]
    product: u16,

    #[structopt(long)]
    verify: bool,

    #[structopt(name = "ELF", parse(from_os_str))]
    elf: PathBuf,
}

fn parse_hex(s: &str) -> Result<u16, anyhow::Error> {
    u16::from_str_radix(s, 16).map_err(|e| e.into())
}

struct Section<'a> {
    address: u32,
    bytes: &'a [u8],
    name: &'a str,
}

fn main() -> Result<(), anyhow::Error> {
    process::exit(not_main()?)
}

fn not_main() -> Result<i32, anyhow::Error> {
    let beginning = Instant::now();
    env_logger::init();

    let opts = Opts::from_args();

    let bytes = fs::read(opts.elf)?;
    debug!("parsing ELF file");
    let elf = &ElfFile::new(&bytes).map_err(anyhow::Error::msg)?;

    debug!("extracting allocatable sections from the ELF file");
    let mut compressed_strings = BTreeMap::new();
    let mut sections = vec![];
    let mut semidap_cursor = None;
    let mut semidap_buffer = None;
    let mut debug_frame = None;
    let mut range_names = vec![];
    let log_shndx = elf
        .section_iter()
        .zip(0..)
        .filter_map(|(sect, shndx)| {
            if sect.get_name(elf) == Ok(".log") {
                Some(shndx)
            } else {
                None
            }
        })
        .next();
    let text_shndx = elf
        .section_iter()
        .zip(0..)
        .filter_map(|(sect, shndx)| {
            if sect.get_name(elf) == Ok(".text") {
                Some(shndx)
            } else {
                None
            }
        })
        .next();
    for sect in elf.section_iter() {
        let is_allocatable = sect.flags() & SHF_ALLOC != 0;

        let size = sect.size();
        if is_allocatable && size != 0 {
            let name = sect.get_name(elf).map_err(anyhow::Error::msg)?;

            if name == ".uninit" {
                // we never load this section
                continue;
            }

            let address = sect.address();
            let max = u64::from(u32::max_value());
            if address > max || address + size > max {
                return Err(anyhow!(
                    " section `{}` is outside the 32-bit address space",
                    name
                ));
            }

            let align = mem::size_of::<u32>() as u64;
            if address % align != 0 || size % align != 0 {
                return Err(anyhow!(
                    " section `{}` is not 4-byte aligned (start = {:#010x}, size = {})",
                    name,
                    address,
                    size
                ));
            }

            sections.push(Section {
                address: address as u32,
                bytes: sect.raw_data(elf),
                name,
            })
        } else if sect.get_name(elf) == Ok(".symtab") {
            if let Ok(symtab) = sect.get_data(elf) {
                if let SectionData::SymbolTable32(entries) = symtab {
                    for entry in entries {
                        if let Ok(name) = entry.get_name(elf) {
                            if Some(entry.shndx() as u32) == log_shndx {
                                compressed_strings.insert(entry.value(), name);
                            } else if Some(entry.shndx() as u32) == text_shndx
                                && entry.size() != 0
                            {
                                // clear the thumb bit
                                let mut name =
                                    rustc_demangle::demangle(name).to_string();
                                let start = entry.value() & !1;

                                // strip the hash (e.g. `::hd881d91ced85c2b0`)
                                let hash_len = "::hd881d91ced85c2b0".len();
                                if let Some(pos) =
                                    name.len().checked_sub(hash_len)
                                {
                                    let maybe_hash = &name[pos..];
                                    if maybe_hash.starts_with("::h") {
                                        // FIXME do not allocate again
                                        name = name[..pos].to_string();
                                    }
                                }

                                range_names
                                    .push((start..start + entry.size(), name));
                            }

                            if name == "SEMIDAP_CURSOR" {
                                if let Ok(addr) = u32::try_from(entry.value()) {
                                    semidap_cursor = Some(addr);
                                }
                            } else if name == "SEMIDAP_BUFFER" {
                                let size = entry.size();
                                if size.is_power_of_two() {
                                    if let (Ok(addr), Ok(len)) = (
                                        u32::try_from(entry.value()),
                                        u32::try_from(size),
                                    ) {
                                        semidap_buffer = Some((addr, len));
                                    }
                                } else {
                                    error!(
                                        "malformed SEMIDAP_BUFFER (len={})",
                                        size
                                    );
                                }
                            }
                        }
                    }
                }
            }
        } else if sect.get_name(elf) == Ok(".debug_frame") {
            let mut df = DebugFrame::new(sect.raw_data(elf), LittleEndian);
            // 32-bit ARM
            df.set_address_size(4);
            debug_frame = Some(df);
        }
    }

    range_names.sort_unstable_by(|a, b| a.0.start.cmp(&b.0.start));

    let mut dap = Dap::open(opts.vendor, opts.product)?;
    let debug_frame = debug_frame
        .ok_or_else(|| anyhow!("`.debug_frame` section is missing"))?;

    dap.default_swd_configuration()?;

    let cpuid = dap.memory_read_word(CPUID::address() as usize as u32)?;
    info!("target: {} (CPUID = {:#010x})", Part::from(cpuid), cpuid);

    dap.halt()?;

    debug!("loading ELF into the target's memory");
    let mut total_bytes = 0;
    let start = Instant::now();
    for section in sections {
        let start = Instant::now();
        dap.memory_write(section.address, section.bytes)?;
        let end = Instant::now();
        let bytes = section.bytes.len();
        total_bytes += bytes as u64;

        let dur = end - start;
        info!("loaded `{}` ({} B) in {:?}", section.name, bytes, dur);

        if opts.verify {
            // verify write
            let start = Instant::now();
            let bytes = dap.memory_read::<u8>(
                section.address,
                section.bytes.len().try_into().expect("UNIMPLEMENTED"),
            )?;

            if bytes != section.bytes {
                return Err(anyhow!(
                    "verification of section `{}` failed",
                    section.name
                ));
            }
            let end = Instant::now();

            info!("verified section `{}` in {:?}", section.name, end - start);
        }
    }

    let end = Instant::now();

    let dur = end - start;
    const NANOS: u64 = 1_000_000_000;
    let speed = total_bytes * NANOS
        / (dur.as_secs() * NANOS + u64::from(dur.subsec_nanos()));
    info!("loaded {} bytes in {:?} ({} B/s)", total_bytes, dur, speed);

    info!("booting program (start to end: {:?})", end - beginning);

    debug!("resetting the target");
    dap.sysresetreq()?;

    static CONTINUE: AtomicBool = AtomicBool::new(true);
    let mut twice = false;
    let mut stdout_buffer = vec![];
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    ctrlc::set_handler(|| CONTINUE.store(false, Ordering::Relaxed))?;
    while CONTINUE.load(Ordering::Relaxed) {
        fn drain(
            cursorp: u32,
            bufferp: u32,
            cap: u32,
            hbuffer: &mut Vec<u8>,
            dap: &mut Dap,
        ) -> Result<u32, anyhow::Error> {
            let readp = cursorp;
            // TODO use atomic commands to read the cursor in a single DAP (HID)
            // transaction
            let words = dap.memory_read::<u32>(cursorp, 2)?;
            let read = words[0];
            let write = words[1];
            let available = write.wrapping_sub(read);
            let cursor = read % cap;

            // TODO use atomic commands to read the buffer and update the `read`
            // pointer in a single DAP (HID) transaction
            if cursor + available > cap {
                // the readable part wraps around the end of the buffer: do a
                // split transfer
                let pivot = cursor + available - cap;
                let first_half = dap.memory_read(bufferp + cursor, pivot)?;
                dap.memory_write_word(readp, read + pivot)?;
                let second_half =
                    dap.memory_read(bufferp, available - pivot)?;
                dap.memory_write_word(readp, read + available)?;
                hbuffer.extend_from_slice(&first_half);
                hbuffer.extend_from_slice(&second_half);
            } else {
                // single transfer
                let bytes = dap.memory_read(bufferp + cursor, available)?;
                dap.memory_write_word(readp, read + available)?;
                hbuffer.extend_from_slice(&bytes);
            }

            Ok(available)
        }

        let available = if let (Some(cursor), Some((bufferp, cap))) =
            (semidap_cursor, semidap_buffer)
        {
            let available =
                drain(cursor, bufferp, cap, &mut stdout_buffer, &mut dap)?;
            let mut n = 0;
            let mut bytes = &*stdout_buffer;
            let total = bytes.len();

            while n < total {
                if let Err(e) = str::from_utf8(&bytes) {
                    let pos = e.valid_up_to();
                    stdout.write_all(&bytes[..pos])?;
                    n += pos;

                    bytes = &bytes[pos..];
                    let first = bytes.first().cloned();

                    // check for compressed string
                    if first == Some(consts::UTF8_SYMTAB_STRING) {
                        let addr = if let Some(byte) = bytes.get(1) {
                            *byte as u64
                        } else {
                            break;
                        };

                        n += 2;
                        bytes = &bytes[2..];

                        if let Some(level) = Level::try_from(addr) {
                            write!(stdout, "{} ", level)?
                        } else {
                            write!(stdout, "{}", compressed_strings[&addr])?;
                        }
                    } else if first == Some(consts::UTF8_TIMESTAMP) {
                        let timestamp = if let Some(bytes) = bytes.get(1..4) {
                            u32::from(bytes[0])
                                | (u32::from(bytes[1]) << 8)
                                | (u32::from(bytes[2]) << 16)
                        } else {
                            break;
                        };

                        n += 4;
                        bytes = &bytes[4..];

                        write!(stdout, "{} ", Timestamp(timestamp))?
                    } else {
                        // incomplete UTF-8 code-point
                        break;
                    }
                } else {
                    stdout.write_all(bytes)?;
                    n += bytes.len();
                }
            }

            if n == total {
                stdout_buffer.clear();
            } else {
                stdout_buffer = stdout_buffer[n..].to_owned();
            }

            if available != 0 {
                twice = false;
            }

            available
        } else {
            0
        };

        // only attempt to handle syscalls whne the log buffer appears to be
        // empty two times in a row
        if available == 0 {
            if !twice {
                twice = true;
                // look at the buffer one more time
                continue;
            }

            if let Some(code) =
                handle_syscall(&mut dap, &debug_frame, &range_names)?
            {
                if !stdout_buffer.is_empty() {
                    stdout.write_all(
                        String::from_utf8_lossy(&stdout_buffer[..]).as_bytes(),
                    )?;
                }
                return Ok(code);
            }
        }
    }

    Ok(0)
}

struct Timestamp(u32);

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let timestamp = (self.0 as f64) / 32_768.0;
        write!(f, "{:>10.6}", timestamp)
    }
}

enum Level {
    Debug,
    Error,
    Info,
    Trace,
    Warn,
}

impl Level {
    fn try_from(addr: u64) -> Option<Self> {
        const ADDR_ERROR: u64 = 0;
        const ADDR_WARN: u64 = 1;
        const ADDR_INFO: u64 = 2;
        const ADDR_DEBUG: u64 = 3;
        const ADDR_TRACE: u64 = 4;

        Some(if addr == ADDR_ERROR {
            Level::Error
        } else if addr == ADDR_WARN {
            Level::Warn
        } else if addr == ADDR_INFO {
            Level::Info
        } else if addr == ADDR_DEBUG {
            Level::Debug
        } else if addr == ADDR_TRACE {
            Level::Trace
        } else {
            return None;
        })
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use colored::*;

        match self {
            Level::Debug => f.write_str("DEBUG"),
            Level::Error => write!(f, "{}", "ERROR".red()),
            Level::Info => write!(f, "{} ", "INFO".green()),
            Level::Trace => write!(f, "{}", "TRACE".dimmed()),
            Level::Warn => write!(f, "{} ", "WARN".yellow()),
        }
    }
}

// if the target device is halted it is because it performed a system call using
// the BKPT instruction. The immediate value passed to the BKPT instruction will
// tell us which system call to service. All system calls are 'diverging' from
// the point of view of the device; system calls also terminate this `semidap`
// instance
fn handle_syscall(
    dap: &mut Dap,
    debug_frame: &DebugFrame<EndianSlice<LittleEndian>>,
    range_names: &[(Range<u64>, String)],
) -> Result<Option<i32>, anyhow::Error> {
    if dap.is_halted()? {
        const SYS_ABORT: u16 = 0xbeaa; // BKPT 0xAA
        const SYS_EXCEPTION: u16 = 0xbeff; // BKPT 0xFF
        const SYS_EXIT: u16 = 0xbeab; // BKPT 0xAB

        let pc = dap.read_core_register(cortex_m::Register::PC)?;
        let insn = dap.memory_read::<u16>(pc, 1)?[0];

        match insn {
            SYS_EXIT => {
                let r0 = dap.read_core_register(cortex_m::Register::R0)?;
                return Ok(Some(r0 as i32));
            }

            SYS_EXCEPTION => {
                return handle_exception(dap, debug_frame, range_names)
                    .map(Some)
            }

            SYS_ABORT => {
                let sp = dap.read_core_register(cortex_m::Register::SP)?;
                let lr = dap.read_core_register(cortex_m::Register::LR)?;
                backtrace(dap, debug_frame, range_names, lr, pc, sp)?;
                return Ok(Some(134));
            }

            _ => {
                error!("unknown instruction: {:#06x}", insn);
                return Ok(Some(1));
            }
        }
    }

    Ok(None)
}

fn backtrace(
    dap: &mut Dap,
    debug_frame: &DebugFrame<EndianSlice<LittleEndian>>,
    range_names: &[(Range<u64>, String)],
    lr: u32,
    mut pc: u32,
    sp: u32,
) -> Result<(), anyhow::Error> {
    fn gimli2cortex(reg: &gimli::Register) -> cortex_m::Register {
        if reg.0 == 13 {
            Register::SP
        } else if reg.0 == 14 {
            Register::LR
        } else if reg.0 == 7 {
            Register::R7
        } else {
            panic!("unknown: {:?}", reg);
        }
    }

    // Lazily evaluated registers
    #[derive(Debug, Default)]
    struct Registers {
        cache: BTreeMap<Register, u32>,
    }

    impl Registers {
        fn new(lr: u32, sp: u32) -> Self {
            let mut cache = BTreeMap::new();
            cache.insert(Register::LR, lr);
            cache.insert(Register::SP, sp);
            Self { cache }
        }

        fn get(
            &mut self,
            reg: cortex_m::Register,
            dap: &mut Dap,
        ) -> Result<u32, anyhow::Error> {
            Ok(match self.cache.entry(reg) {
                btree_map::Entry::Occupied(entry) => *entry.get(),
                btree_map::Entry::Vacant(entry) => {
                    *entry.insert(dap.read_core_register(reg)?)
                }
            })
        }

        fn insert(&mut self, reg: cortex_m::Register, val: u32) {
            self.cache.insert(reg, val);
        }

        fn update_cfa(
            &mut self,
            rule: &CfaRule<EndianSlice<LittleEndian>>,
            dap: &mut Dap,
        ) -> Result</* cfa_changed: */ bool, anyhow::Error> {
            debug!("Registers::update_cfg(self={:?}, rule={:?})", self, rule);

            match rule {
                CfaRule::RegisterAndOffset { register, offset } => {
                    let cfa =
                        (i64::from(self.get(gimli2cortex(register), dap)?)
                            + offset) as u32;
                    let ok = self.cache.get(&Register::SP) != Some(&cfa);
                    self.cache.insert(Register::SP, cfa);
                    Ok(ok)
                }

                CfaRule::Expression(_) => unimplemented!("CfaRule::Expression"),
            }
        }

        fn update(
            &mut self,
            reg: &gimli::Register,
            rule: &RegisterRule<EndianSlice<LittleEndian>>,
            dap: &mut Dap,
        ) -> Result<(), anyhow::Error> {
            let reg = gimli2cortex(reg);
            debug!(
                "Registers::update(self={:?}, reg={:?}, rule={:?})",
                self, reg, rule
            );

            match rule {
                RegisterRule::Undefined => unreachable!(),

                RegisterRule::Offset(offset) => {
                    let cfa = self.get(Register::SP, dap)?;
                    let addr = (i64::from(cfa) + offset) as u32;
                    self.cache.insert(reg, dap.memory_read_word(addr)?);
                }

                _ => unimplemented!(),
            }

            Ok(())
        }
    }

    use cortex_m::Register;

    // statically linked binary -- there are no relative addresses
    let bases = &BaseAddresses::default();
    let ctx = &mut UninitializedUnwindContext::new();

    println!("stack backtrace:");
    // the reset value of the Link Register; this indicates the end of the stack
    const LR_RESET: u32 = 0xffff_ffff;
    let mut frame = 0;
    let mut registers = Registers::new(lr, sp);
    loop {
        println!(
            "{:>4}: {:#010x} - {}",
            frame,
            pc,
            rustc_demangle::demangle(
                range_names
                    .binary_search_by(|rn| if rn.0.contains(&u64::from(pc)) {
                        cmp::Ordering::Equal
                    } else if u64::from(pc) < rn.0.start {
                        cmp::Ordering::Greater
                    } else {
                        cmp::Ordering::Less
                    })
                    .map(|idx| &*range_names[idx].1)
                    .unwrap_or("<unknown>")
            )
        );

        let fde = debug_frame.fde_for_address(
            bases,
            pc.into(),
            DebugFrame::cie_from_offset,
        )?;
        let uwt_row =
            fde.unwind_info_for_address(debug_frame, bases, ctx, pc.into())?;

        let cfa_changed = registers.update_cfa(uwt_row.cfa(), dap)?;

        for (reg, rule) in uwt_row.registers() {
            registers.update(reg, rule, dap)?;
        }

        let lr = registers.get(Register::LR, dap)?;
        if lr == LR_RESET {
            break;
        }

        if !cfa_changed && lr == pc {
            println!(
                "error: the stack appears to be corrupted beyond this point"
            );
            return Ok(());
        }

        if lr > 0xffff_fff0 {
            println!("      <exception entry>");

            let sp = registers.get(Register::SP, dap)?;
            let stacked = Stacked::read(dap, sp)?;

            // XXX insert other registers?
            registers.insert(Register::LR, stacked.lr);
            // adjust the stack pointer for stacked registers
            registers
                .insert(Register::SP, sp + mem::size_of::<Stacked>() as u32);
            pc = stacked.pc;
        } else {
            if lr & 1 == 0 {
                bail!("bug? LR ({:#010x}) didn't have the Thumb bit set", lr)
            }
            pc = lr & !1;
        }

        frame += 1;
    }

    Ok(())
}

fn handle_exception(
    dap: &mut Dap,
    debug_frame: &DebugFrame<EndianSlice<LittleEndian>>,
    range_names: &[(Range<u64>, String)],
) -> Result<i32, anyhow::Error> {
    use cortex_m::Register;

    fn read_register(
        dap: &mut Dap,
        reg: Register,
    ) -> Result<(Register, u32), anyhow::Error> {
        let val = dap.read_core_register(reg)?;
        Ok((reg, val))
    }

    const SCB_ICSR: u32 = 0xE000_ED04;

    let icsr = dap.memory_read_word(SCB_ICSR)?;
    let vectactive = icsr as u8;

    if vectactive == 0 {
        println!("error: SYS_EXCEPTION called from thread mode");
        return Ok(1);
    }

    // XXX we are assuming SP has not been modified since exception
    // entry
    let sp = dap.read_core_register(Register::SP)?;

    // these 8 registers are pushed onto the stack on exception entry
    let stacked = Stacked::read(dap, sp)?;
    let r0 = dap.read_core_register(Register::R0)?;
    let r1 = dap.read_core_register(Register::R1)?;
    let r2 = dap.read_core_register(Register::R2)?;
    let r3 = dap.read_core_register(Register::R3)?;
    let r12 = dap.read_core_register(Register::R12)?;
    // XXX unclear whether the XPSR values are supposed to match; the IPSR
    // part of xPSR will certainly be different
    // let xpsr = dap.read_core_register(Register::XPSR)?;

    let stack_overflow = stacked.r0 != r0
        || stacked.r1 != r1
        || stacked.r2 != r2
        || stacked.r3 != r3
        || stacked.r12 != r12;

    let mut registers = vec![
        (Register::R0, r0),
        (Register::R1, r1),
        (Register::R2, r2),
        (Register::R3, r3),
    ];

    registers.push(read_register(dap, Register::R4)?);
    registers.push(read_register(dap, Register::R5)?);
    registers.push(read_register(dap, Register::R6)?);
    registers.push(read_register(dap, Register::R7)?);
    registers.push(read_register(dap, Register::R8)?);
    registers.push(read_register(dap, Register::R9)?);
    registers.push(read_register(dap, Register::R10)?);
    registers.push(read_register(dap, Register::R11)?);
    registers.push((Register::R12, r12));

    // correct for stacked registers
    registers.push((Register::SP, sp + mem::size_of::<Stacked>() as u32));

    // on stack overflow we can NOT rely on `pushed_registers` because they
    // could have been pushed to invalid memory and the DAP would read them
    // as `0`
    if !stack_overflow {
        registers.push((Register::PC, stacked.pc));
        registers.push((Register::LR, stacked.lr));
        registers.push((Register::XPSR, stacked.xpsr));
    }

    let cfbp = dap.read_core_register(Register::CFBP)?;

    println!("\n------------------------------------------");
    if stack_overflow {
        println!("{:^42}", "stack overflow detected");
    } else {
        let exception: Cow<_> = match vectactive {
            2 => "NMI".into(),
            3 => "HardFault".into(),
            4 => "MemManage".into(),
            5 => "BusFault".into(),
            6 => "UsageFault".into(),
            11 => "SVCall".into(),
            // XXX unreachable?
            12 => "DebugMonitor".into(),
            14 => "PendSV".into(),
            15 => "SysTick".into(),
            irqn if irqn > 16 => format!("IRQ{}", irqn - 16).into(),
            _ => format!("??? (ICSR.VECTACTIVE = {})", vectactive).into(),
        };

        println!("{:^42}", "unhandled exception");
        println!("{:^42}", exception);
    }

    println!();

    for pairs in registers.chunks(2) {
        print!("{:>7}: {:#010x}", format!("{:?}", pairs[0].0), pairs[0].1);

        if let Some(second) = pairs.get(1) {
            println!("  {:>9}: {:#010x}", format!("{:?}", second.0), second.1);
        } else {
            println!();
        }
    }

    let control = cfbp >> 24;
    let faultmask = (cfbp >> 16) & 0xff;
    let basepri = (cfbp >> 8) & 0xff;
    let primask = cfbp & 0xff;

    println!(
        "CONTROL: {:#04x}        FAULTMASK: {:#04x}",
        control, faultmask
    );
    println!(
        "BASEPRI: {:#04x}          PRIMASK: {:#04x}",
        basepri, primask
    );

    if !stack_overflow {
        println!("------------------------------------------");

        backtrace(dap, debug_frame, range_names, stacked.lr, stacked.pc, sp)?;
    }

    prompt(dap)?;

    return Ok(0);
}

fn prompt(dap: &mut Dap) -> Result<(), anyhow::Error> {
    println!("------------------------------------------");

    let mut rl = Editor::<()>::new();
    while let Ok(line) = rl.readline("\n> ") {
        let mut line = line.trim();
        // remove comments
        line = line.splitn(2, '#').next().unwrap_or("");

        if line.is_empty() {
            // just a comment; nothing to do
            continue;
        } else if line == "help" {
            println!(
                "\
commands:
  help                        Displays this text
  show <address> <i16>        Displays memory
  show <address> -<u16> <u16> Displays memory
  exit                        Exits the debugger
  quit                        Alias for `exit`"
            );
        } else if line == "quit" {
            break;
        } else if line.starts_with("show ") {
            let mut parts = line["show ".len()..].trim().splitn(3, ' ');
            let addr = parts.next().and_then(|s| {
                if s.starts_with("0x") {
                    u32::from_str_radix(&s["0x".len()..].replace('_', ""), 16)
                        .ok()
                } else {
                    s.parse::<u32>().ok()
                }
            });

            let range = match (parts.next(), parts.next()) {
                (Some(n), None) => {
                    n.parse::<i32>()
                        .ok()
                        .map(|n| if n < 0 { n..1 } else { 0..n })
                }

                (Some(m), Some(n)) => {
                    if m.starts_with("-") && !n.starts_with("-") {
                        m.parse::<i32>().ok().and_then(|m| {
                            n.parse::<i32>().ok().map(|n| m..n + 1)
                        })
                    } else {
                        None
                    }
                }

                _ => None,
            };

            if let (Some(addr), Some(Range { start, end })) = (addr, range) {
                if addr % 4 == 0 {
                    let n = (end - start) as u32;
                    if n == 0 {
                        continue;
                    }

                    let start_addr = (addr as i32 + 4 * start) as u32;
                    let end_addr = (addr as i32 + 4 * end) as u32;
                    let words = dap.memory_read::<u32>(start_addr, n)?;

                    let mut i = 0;
                    let mut cursor = start_addr & !0xf;
                    while cursor < end_addr {
                        print!("{:#010x}:", cursor);

                        for _ in 0..4 {
                            if cursor >= start_addr && cursor < end_addr {
                                if cursor == addr {
                                    use colored::*;

                                    print!(
                                        " {}",
                                        format!("{:#010x}", words[i]).bold()
                                    );
                                } else {
                                    print!(" {:#010x}", words[i]);
                                }

                                i += 1;
                            } else {
                                print!("           ");
                            }

                            cursor += 4;
                        }
                        println!();
                    }
                } else {
                    println!("error: address must be 4-byte aligned");
                }
            } else {
                println!(
                    "\
error: invalid syntax. try `show 0 16` or `show 0x2000_0000 -2 2`"
                )
            }
        } else {
            println!("unknown command; try `help`");
        }
    }

    Ok(())
}

/// Part number
pub enum Part {
    /// ARM Cortex-M0
    CortexM0,

    /// ARM Cortex-M0+
    CortexM0Plus,

    /// ARM Cortex-M3
    CortexM3,

    /// ARM Cortex-M4
    CortexM4,

    /// ARM Cortex-M7
    CortexM7,

    /// ARM Cortex-M23
    CortexM23,

    /// ARM Cortex-M33
    CortexM33,

    /// Unknown part
    Unknown,
}

impl From<u32> for Part {
    fn from(bits: u32) -> Self {
        let r = cpuid::R::from(bits);

        const ARM: u8 = 0x41;

        if r.IMPLEMENTER() != ARM {
            return Part::Unknown;
        }

        const ARMV6M: u8 = 0xc;
        const ARMV7M: u8 = 0xf;

        let arch = r.ARCHITECTURE();
        let partno = r.PARTNO();
        if arch == ARMV6M {
            if partno == 0xc20 {
                Part::CortexM0
            } else if partno == 0xc60 {
                Part::CortexM0Plus
            } else if partno == 0xd20 {
                Part::CortexM23
            } else {
                Part::Unknown
            }
        } else if arch == ARMV7M {
            if partno == 0xc23 {
                Part::CortexM3
            } else if partno == 0xC24 {
                Part::CortexM4
            } else if partno == 0xc27 {
                Part::CortexM7
            } else if partno == 0xd21 {
                Part::CortexM33
            } else {
                Part::Unknown
            }
        } else {
            Part::Unknown
        }
    }
}

impl fmt::Display for Part {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let s = match *self {
            Part::CortexM0 => "ARM Cortex-M0",
            Part::CortexM0Plus => "ARM Cortex-M0+",
            Part::CortexM3 => "ARM Cortex-M3",
            Part::CortexM4 => "ARM Cortex-M4",
            Part::CortexM7 => "ARM Cortex-M7",
            Part::CortexM23 => "ARM Cortex-M23",
            Part::CortexM33 => "ARM Cortex-M33",
            Part::Unknown => "unknown part",
        };

        f.write_str(s)
    }
}

#[derive(Debug)]
struct Stacked {
    r0: u32,
    r1: u32,
    r2: u32,
    r3: u32,
    r12: u32,
    lr: u32,
    pc: u32,
    xpsr: u32,
}

impl Stacked {
    fn read(dap: &mut Dap, sp: u32) -> Result<Self, anyhow::Error> {
        let registers = dap.memory_read::<u32>(sp, 8)?;

        Ok(Stacked {
            r0: registers[0],
            r1: registers[1],
            r2: registers[2],
            r3: registers[3],
            r12: registers[4],
            lr: registers[5],
            pc: registers[6],
            xpsr: registers[7],
        })
    }
}