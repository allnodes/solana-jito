#![allow(clippy::arithmetic_side_effects)]

use {
    crate::device::NetworkDevice,
    aya::{
        Ebpf, EbpfLoader,
        programs::{Xdp, xdp::XdpMode},
    },
    std::io::{Cursor, Write},
};

macro_rules! write_fields {
    ($w:expr, $($x:expr),*) => {
        $(
            $w.write_all(&$x.to_le_bytes())?;
        )*
    };
}

const SHT_NULL: u32 = 0;
// text section
const SHT_PROGBITS: u32 = 1;
// symbol table
const SHT_SYMTAB: u32 = 2;
// string table
const SHT_STRTAB: u32 = 3;

// flags required for the text section
const SHF_ALLOC: u64 = 1 << 1;
const SHF_EXECINSTR: u64 = 1 << 2;

// symbol visibility
const STB_GLOBAL: u8 = 1 << 4;
// symbol type
const STT_FUNC: u8 = 2;

// we just let all packets in
const XDP_PROG: &[u8] = &[
    0xb7, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, // r0 = XDP_PASS
    0x95, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // exit
];

// the string table
const STRTAB: &[u8] = b"\0xdp\0.symtab\0.strtab\0agave_xdp\0";

pub fn load_xdp_program(dev: &NetworkDevice) -> Result<Ebpf, Box<dyn std::error::Error>> {
    let mut loader = EbpfLoader::new();
    let broken_frags = dev.driver()? == "i40e";
    let mut ebpf = if broken_frags {
        loader.override_global("AGAVE_XDP_DROP_MULTI_FRAGS", &1u8, true);
        loader.load(agave_xdp_ebpf::AGAVE_XDP_EBPF_PROGRAM)
    } else {
        loader.load(&generate_xdp_elf())
    }?;
    let p: &mut Xdp = ebpf.program_mut("agave_xdp").unwrap().try_into().unwrap();
    p.load()?;

    p.attach_to_if_index(dev.if_index(), XdpMode::Driver)?;

    Ok(ebpf)
}

pub fn load_rx_program(
    dev: &NetworkDevice,
    require_native: bool,
) -> Result<Ebpf, Box<dyn std::error::Error>> {
    let mut loader = EbpfLoader::new();
    if dev.driver().map(|d| d == "i40e").unwrap_or(false) {
        loader.override_global("AGAVE_XDP_DROP_MULTI_FRAGS", &1u8, true);
    }
    let mut ebpf = loader.load(agave_xdp_ebpf::AGAVE_XDP_EBPF_PROGRAM)?;
    {
        let p: &mut Xdp = ebpf.program_mut("agave_xdp_rx").unwrap().try_into().unwrap();
        p.load()?;
        attach_rx(p, dev, require_native)?;
    }
    Ok(ebpf)
}

fn attach_rx(
    p: &mut Xdp,
    dev: &NetworkDevice,
    require_native: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    match p.attach_to_if_index(dev.if_index(), XdpMode::Driver) {
        Ok(_) => Ok(()),
        Err(e) if is_hook_busy(&e) => Err(hook_conflict_error(dev)),
        Err(e) if require_native => Err(format!(
            "--xdp-zero-copy requires the XDP program to attach in native (driver) mode, which \
             failed on {}: {e}. The driver may not support native XDP — start without \
             --xdp-zero-copy to run in copy mode instead",
            dev.name()
        )
        .into()),
        Err(e) => {
            log::debug!("xdp rx: native attach to {} failed ({e}); using skb mode", dev.name());
            match p.attach_to_if_index(dev.if_index(), XdpMode::Skb) {
                Ok(_) => Ok(()),
                Err(e) if is_hook_busy(&e) => Err(hook_conflict_error(dev)),
                Err(e) => Err(e.into()),
            }
        }
    }
}

fn is_hook_busy(e: &aya::programs::ProgramError) -> bool {
    matches!(
        e,
        aya::programs::ProgramError::SyscallError(se)
            if se.io_error.raw_os_error() == Some(libc::EBUSY)
    )
}

fn hook_conflict_error(dev: &NetworkDevice) -> Box<dyn std::error::Error> {
    let name = dev.name();
    let who = match crate::netlink::xdp_prog_id(dev.if_index()) {
        Ok(Some(id)) => format!("(program id {id}) "),
        _ => String::new(),
    };
    format!(
        "interface {name} already has an XDP program attached {who}\
         — start with --xdp-chain-loading to run alongside it, detach it with \
         `ip link set dev {name} xdp off`, or start with --no-xdp"
    )
    .into()
}

pub fn add_rx_port(ebpf: &mut Ebpf, port: u16) -> Result<(), Box<dyn std::error::Error>> {
    let map = ebpf.map_mut("RX_PORTS").ok_or("RX_PORTS map not found")?;
    let mut ports: aya::maps::HashMap<_, u16, u8> = aya::maps::HashMap::try_from(map)?;
    ports.insert(port, 1, 0)?;
    Ok(())
}

pub fn register_xsk(
    ebpf: &mut Ebpf,
    queue: u32,
    fd: std::os::fd::RawFd,
) -> Result<(), Box<dyn std::error::Error>> {
    let map = ebpf.map_mut("XSKS").ok_or("XSKS map not found in xdp program")?;
    let mut xsks = aya::maps::XskMap::try_from(map)?;
    xsks.set(queue, fd, 0)?;
    Ok(())
}

fn generate_xdp_elf() -> Vec<u8> {
    let mut buffer = vec![0u8; 4096];
    let mut cursor = Cursor::new(&mut buffer);

    // start after the header
    let xdp_off = 64;
    cursor.set_position(xdp_off);
    cursor.write_all(XDP_PROG).unwrap();
    let xdp_size = cursor.position() - xdp_off;

    // write the string table
    let strtab_off = cursor.position();
    cursor.write_all(STRTAB).unwrap();
    let strtab_size = cursor.position() - strtab_off;

    // write the symbol table
    let symtab_off = align_cursor(&mut cursor, 8);
    write_symbol(&mut cursor, 0, 0, 0, 0, 0, 0).unwrap();
    write_symbol(
        &mut cursor,
        21, // index
        0,
        XDP_PROG.len() as u64,
        STB_GLOBAL | STT_FUNC,
        0,
        1, // section index
    )
    .unwrap();
    let symtab_size = cursor.position() - symtab_off;

    // write the section headers
    let shdrs_off = align_cursor(&mut cursor, 8);
    write_section_headers(
        &mut cursor,
        xdp_off,
        xdp_size,
        strtab_off,
        strtab_size,
        symtab_off,
        symtab_size,
    )
    .unwrap();

    // finally go back and write the header
    const SECTIONS: u16 = 4;
    const STRTAB_INDEX: u16 = 2;
    cursor.set_position(0);
    write_elf_header(&mut cursor, shdrs_off, SECTIONS, STRTAB_INDEX).unwrap();

    buffer
}

fn align_cursor(cursor: &mut Cursor<&mut Vec<u8>>, alignment: usize) -> u64 {
    let pos = cursor.position() as usize;
    let padding = (alignment - (pos % alignment)) % alignment;
    cursor.set_position((pos + padding) as u64);
    cursor.position()
}

fn write_elf_header(
    w: &mut impl Write,
    sh_offset: u64,
    sh_num: u16,
    sh_strndx: u16,
) -> std::io::Result<()> {
    let mut header = [
        0x7f, 0x45, 0x4c, 0x46, // EI_MAG: 0x7F 'ELF'
        0x02, 0x01, 0x01, 0x00, // CLASS64, LSB, Version1
        0x00, 0x00, 0x00, 0x00, // EI_PAD
        0x00, 0x00, 0x00, 0x00, // EI_PAD
        0x01, 0x00, // e_type: ET_REL
        0xf7, 0x00, // e_machine: EM_BPF
        0x01, 0x00, 0x00, 0x00, // e_version: EV_CURRENT
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // e_entry
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // e_phoff
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // e_shoff
        0x00, 0x00, 0x00, 0x00, // e_flags
        0x40, 0x00, // e_ehsize: 64
        0x00, 0x00, // e_phentsize
        0x00, 0x00, // e_phnum
        0x40, 0x00, // e_shentsize: 64
        0x00, 0x00, // e_shnum
        0x00, 0x00, // e_shstrndx
    ];

    header[40..48].copy_from_slice(&sh_offset.to_le_bytes());
    header[60..62].copy_from_slice(&sh_num.to_le_bytes());
    header[62..64].copy_from_slice(&sh_strndx.to_le_bytes());

    w.write_all(&header)
}

#[allow(clippy::too_many_arguments)]
fn write_section_header(
    w: &mut impl Write,
    name: u32,
    type_: u32,
    flags: u64,
    addr: u64,
    offset: u64,
    size: u64,
    link: u32,
    info: u32,
    addralign: u64,
    entsize: u64,
) -> std::io::Result<()> {
    write_fields!(
        w, name, type_, flags, addr, offset, size, link, info, addralign, entsize
    );

    Ok(())
}

fn write_symbol(
    w: &mut impl Write,
    name: u32,
    value: u64,
    size: u64,
    info: u8,
    other: u8,
    shndx: u16,
) -> std::io::Result<()> {
    write_fields!(
        w,
        name,
        ((other as u16) << 8) | info as u16,
        shndx,
        value,
        size
    );

    Ok(())
}

// don't format the write_section_headers calls 1-2 digit arguments are annoying
#[rustfmt::skip]
fn write_section_headers(
    w: &mut impl Write,
    xdp_off: u64,
    xdp_size: u64,
    strtab_off: u64,
    strtab_size: u64,
    symtab_off: u64,
    symtab_size: u64,
) -> std::io::Result<()> {
    const STRTAB_XDP_OFF: u32 = 1;
    const STRTAB_SYMTAB_OFF: u32 = 5;
    const STRTAB_STRTAB_OFF: u32 = 13;
    write_section_header(w, 0, SHT_NULL, 0, 0, 0, 0, 0, 0, 0, 0)?;
    write_section_header(w, STRTAB_XDP_OFF, SHT_PROGBITS, SHF_ALLOC | SHF_EXECINSTR, 0, xdp_off, xdp_size, 0, 0, 0, 0)?;
    write_section_header(w, STRTAB_STRTAB_OFF, SHT_STRTAB, 0, 0, strtab_off, strtab_size, 0, 0, 0, 0)?;
    write_section_header(w, STRTAB_SYMTAB_OFF, SHT_SYMTAB, 0, 0, symtab_off, symtab_size, 2, 1, 0, 0)?;
    Ok(())
}
