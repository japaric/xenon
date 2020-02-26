#![deny(rust_2018_compatibility)]
#![deny(rust_2018_idioms)]
#![deny(warnings)]

// TODO split most modules into library

pub mod cm;
pub mod codegen;
mod fmt;
pub mod ir;
pub mod opt;
mod translate;
mod verify;

use std::{fs, path::Path, process::Command};

use anyhow::bail;

fn main() -> Result<(), anyhow::Error> {
    gen_cm(Path::new("../../shared/cm/src/lib.rs"))?;
    gen_nrf52(Path::new("../../firmware/pac/src/lib.rs"))?;

    Ok(())
}

// Audited register writes
const AUDITED: &[&str] = &["CLOCK", "P0", "RTC0", "TWIM0"];

fn gen_nrf52(lib: &Path) -> Result<(), anyhow::Error> {
    let xml = fs::read_to_string("nrf52.svd")?;
    let dev = svd_parser::parse(&xml)?;
    let mut dev = translate::svd::device(&dev, AUDITED);
    audit_nrf52(&mut dev);
    gen(dev, lib)?;
    check_lib(lib)
}

fn audit_nrf52(dev: &mut ir::Device<'_>) {
    for periph in &mut dev.peripherals {
        match &*periph.name {
            "RTC0" => {
                for reg in &mut periph.registers {
                    match &*reg.name {
                        // enabling interrupts can break critical sections
                        "INTENSET" => {
                            reg.access.make_write_unsafe();
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
}

fn gen_cm(lib: &Path) -> Result<(), anyhow::Error> {
    let dev = cm::device();
    gen(dev, lib)?;
    check_lib(lib)
}

fn gen(mut dev: ir::Device<'_>, lib: &Path) -> Result<(), anyhow::Error> {
    assert!(lib.is_file());

    dev.verify()?;
    opt::device(&mut dev);
    let krate = codegen::device(&dev);
    fs::write(lib, krate)?;
    Ok(())
}

fn check_lib(lib: &Path) -> Result<(), anyhow::Error> {
    let dir = lib.parent().expect("UNREACHABLE");

    if !Command::new("rustfmt").arg(lib).status()?.success() {
        bail!("`rustfmt` failed");
    }

    if !Command::new("cargo")
        .arg("clippy")
        .current_dir(dir)
        .status()?
        .success()
    {
        bail!("`cargo` failed");
    }

    if !Command::new("cargo")
        .arg("doc")
        .current_dir(dir)
        .status()?
        .success()
    {
        bail!("`cargo` failed");
    }

    Ok(())
}