#![allow(clippy::arithmetic_side_effects)]

use std::{
    ffi::CString,
    io,
    os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd},
    path::Path,
};

const BPF_MAP_LOOKUP_ELEM: u32 = 1;
const BPF_MAP_UPDATE_ELEM: u32 = 2;
const BPF_MAP_DELETE_ELEM: u32 = 3;
const BPF_MAP_GET_NEXT_KEY: u32 = 4;
const BPF_OBJ_PIN: u32 = 6;
const BPF_OBJ_GET: u32 = 7;
const BPF_PROG_GET_FD_BY_ID: u32 = 13;
const BPF_MAP_GET_FD_BY_ID: u32 = 14;
const BPF_OBJ_GET_INFO_BY_FD: u32 = 15;
const BPF_BTF_GET_FD_BY_ID: u32 = 19;
const BPF_LINK_CREATE: u32 = 28;

const OBJ_NAME_LEN: usize = 16;

const OFF_ID: usize = 4;
const OFF_NR_MAP_IDS: usize = 52;
const OFF_MAP_IDS: usize = 56;
const OFF_NAME: usize = 64;
const OFF_BTF_ID: usize = 128;
const PROG_INFO_LEN: usize = 256;
const MAX_MAP_IDS: usize = 16;

const MAP_OFF_ID: usize = 4;
const MAP_OFF_VALUE_SIZE: usize = 12;
const MAP_OFF_NAME: usize = 24;
const MAP_INFO_LEN: usize = 128;

unsafe fn bpf(cmd: u32, attr: *mut libc::c_void, size: usize) -> io::Result<i64> {
    let ret = unsafe { libc::syscall(libc::SYS_bpf, cmd as libc::c_long, attr, size) };
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

pub fn obj_get(path: &Path) -> io::Result<OwnedFd> {
    use std::os::unix::ffi::OsStrExt as _;
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let mut attr = [0u8; 16];
    attr[0..8].copy_from_slice(&(c_path.as_ptr() as u64).to_ne_bytes());
    let fd = unsafe { bpf(BPF_OBJ_GET, attr.as_mut_ptr().cast(), attr.len())? };
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

pub struct ProgInfo {
    pub id: u32,
    pub name: String,
    pub btf_id: u32,
    pub map_ids: Vec<u32>,
}

pub fn prog_info(fd: BorrowedFd<'_>) -> io::Result<ProgInfo> {
    let mut info = [0u8; PROG_INFO_LEN];
    let mut map_ids = [0u32; MAX_MAP_IDS];
    info[OFF_NR_MAP_IDS..OFF_NR_MAP_IDS + 4].copy_from_slice(&(MAX_MAP_IDS as u32).to_ne_bytes());
    info[OFF_MAP_IDS..OFF_MAP_IDS + 8].copy_from_slice(&(map_ids.as_mut_ptr() as u64).to_ne_bytes());
    let mut attr = [0u8; 16];
    attr[0..4].copy_from_slice(&(fd.as_raw_fd() as u32).to_ne_bytes());
    attr[4..8].copy_from_slice(&(PROG_INFO_LEN as u32).to_ne_bytes());
    attr[8..16].copy_from_slice(&(info.as_mut_ptr() as u64).to_ne_bytes());
    unsafe { bpf(BPF_OBJ_GET_INFO_BY_FD, attr.as_mut_ptr().cast(), attr.len())? };

    let id = u32::from_ne_bytes(info[OFF_ID..OFF_ID + 4].try_into().unwrap());
    let btf_id = u32::from_ne_bytes(info[OFF_BTF_ID..OFF_BTF_ID + 4].try_into().unwrap());
    let nr = u32::from_ne_bytes(info[OFF_NR_MAP_IDS..OFF_NR_MAP_IDS + 4].try_into().unwrap());
    let name_bytes = &info[OFF_NAME..OFF_NAME + OBJ_NAME_LEN];
    let end = name_bytes.iter().position(|&b| b == 0).unwrap_or(OBJ_NAME_LEN);
    let name = String::from_utf8_lossy(&name_bytes[..end]).into_owned();
    Ok(ProgInfo {
        id,
        name,
        btf_id,
        map_ids: map_ids[..(nr as usize).min(MAX_MAP_IDS)].to_vec(),
    })
}

pub fn pinned_prog_name(path: &Path) -> io::Result<String> {
    let fd = obj_get(path)?;
    Ok(prog_info(fd.as_fd())?.name)
}

pub fn pinned_prog_id(path: &Path) -> io::Result<u32> {
    let fd = obj_get(path)?;
    Ok(prog_info(fd.as_fd())?.id)
}

pub fn map_update(fd: BorrowedFd<'_>, key: &[u8], value: &[u8]) -> io::Result<()> {
    let mut attr = [0u8; 32];
    attr[0..4].copy_from_slice(&(fd.as_raw_fd() as u32).to_ne_bytes());
    attr[8..16].copy_from_slice(&(key.as_ptr() as u64).to_ne_bytes());
    attr[16..24].copy_from_slice(&(value.as_ptr() as u64).to_ne_bytes());
    unsafe { bpf(BPF_MAP_UPDATE_ELEM, attr.as_mut_ptr().cast(), attr.len())? };
    Ok(())
}

pub fn map_delete(fd: BorrowedFd<'_>, key: &[u8]) -> io::Result<()> {
    let mut attr = [0u8; 32];
    attr[0..4].copy_from_slice(&(fd.as_raw_fd() as u32).to_ne_bytes());
    attr[8..16].copy_from_slice(&(key.as_ptr() as u64).to_ne_bytes());
    unsafe { bpf(BPF_MAP_DELETE_ELEM, attr.as_mut_ptr().cast(), attr.len())? };
    Ok(())
}

pub fn map_next_key(
    fd: BorrowedFd<'_>,
    prev: Option<&[u8]>,
    key_len: usize,
) -> io::Result<Option<Vec<u8>>> {
    let mut next = vec![0u8; key_len];
    let mut attr = [0u8; 32];
    attr[0..4].copy_from_slice(&(fd.as_raw_fd() as u32).to_ne_bytes());
    if let Some(prev) = prev {
        attr[8..16].copy_from_slice(&(prev.as_ptr() as u64).to_ne_bytes());
    }
    attr[16..24].copy_from_slice(&(next.as_mut_ptr() as u64).to_ne_bytes());
    match unsafe { bpf(BPF_MAP_GET_NEXT_KEY, attr.as_mut_ptr().cast(), attr.len()) } {
        Ok(_) => Ok(Some(next)),
        Err(err) if err.raw_os_error() == Some(libc::ENOENT) => Ok(None),
        Err(err) => Err(err),
    }
}

pub fn obj_pin(fd: BorrowedFd<'_>, path: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt as _;
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let mut attr = [0u8; 16];
    attr[0..8].copy_from_slice(&(c_path.as_ptr() as u64).to_ne_bytes());
    attr[8..12].copy_from_slice(&(fd.as_raw_fd() as u32).to_ne_bytes());
    unsafe { bpf(BPF_OBJ_PIN, attr.as_mut_ptr().cast(), attr.len())? };
    Ok(())
}

pub fn prog_fd_by_id(id: u32) -> io::Result<OwnedFd> {
    fd_by_id(BPF_PROG_GET_FD_BY_ID, id)
}

pub fn map_fd_by_id(id: u32) -> io::Result<OwnedFd> {
    fd_by_id(BPF_MAP_GET_FD_BY_ID, id)
}

fn fd_by_id(cmd: u32, id: u32) -> io::Result<OwnedFd> {
    let mut attr = [0u8; 12];
    attr[0..4].copy_from_slice(&id.to_ne_bytes());
    let fd = unsafe { bpf(cmd, attr.as_mut_ptr().cast(), attr.len())? };
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

pub const BPF_MAP_TYPE_ARRAY: u32 = 2;

pub struct MapInfo {
    pub map_type: u32,
    pub name: String,
    pub value_size: u32,
    pub id: u32,
}

pub fn map_info(fd: BorrowedFd<'_>) -> io::Result<MapInfo> {
    let mut info = [0u8; MAP_INFO_LEN];
    let mut attr = [0u8; 16];
    attr[0..4].copy_from_slice(&(fd.as_raw_fd() as u32).to_ne_bytes());
    attr[4..8].copy_from_slice(&(MAP_INFO_LEN as u32).to_ne_bytes());
    attr[8..16].copy_from_slice(&(info.as_mut_ptr() as u64).to_ne_bytes());
    unsafe { bpf(BPF_OBJ_GET_INFO_BY_FD, attr.as_mut_ptr().cast(), attr.len())? };
    let map_type = u32::from_ne_bytes(info[0..4].try_into().unwrap());
    let value_size =
        u32::from_ne_bytes(info[MAP_OFF_VALUE_SIZE..MAP_OFF_VALUE_SIZE + 4].try_into().unwrap());
    let name_bytes = &info[MAP_OFF_NAME..MAP_OFF_NAME + OBJ_NAME_LEN];
    let end = name_bytes.iter().position(|&b| b == 0).unwrap_or(OBJ_NAME_LEN);
    let name = String::from_utf8_lossy(&name_bytes[..end]).into_owned();
    let id = u32::from_ne_bytes(info[MAP_OFF_ID..MAP_OFF_ID + 4].try_into().unwrap());
    Ok(MapInfo {
        map_type,
        name,
        value_size,
        id,
    })
}

pub fn map_lookup_first(fd: BorrowedFd<'_>, value_len: usize) -> io::Result<Vec<u8>> {
    let key: u32 = 0;
    let mut value = vec![0u8; value_len];
    let mut attr = [0u8; 32];
    attr[0..4].copy_from_slice(&(fd.as_raw_fd() as u32).to_ne_bytes());
    attr[8..16].copy_from_slice(&(&key as *const u32 as u64).to_ne_bytes());
    attr[16..24].copy_from_slice(&(value.as_mut_ptr() as u64).to_ne_bytes());
    unsafe { bpf(BPF_MAP_LOOKUP_ELEM, attr.as_mut_ptr().cast(), attr.len())? };
    Ok(value)
}

pub fn btf_bytes_by_id(btf_id: u32) -> io::Result<Vec<u8>> {
    let fd = fd_by_id(BPF_BTF_GET_FD_BY_ID, btf_id)?;
    let read_info = |btf_ptr: u64, cap: u32| -> io::Result<[u8; 32]> {
        let mut info = [0u8; 32];
        info[0..8].copy_from_slice(&btf_ptr.to_ne_bytes());
        info[8..12].copy_from_slice(&cap.to_ne_bytes());
        let mut attr = [0u8; 16];
        attr[0..4].copy_from_slice(&(fd.as_raw_fd() as u32).to_ne_bytes());
        attr[4..8].copy_from_slice(&(info.len() as u32).to_ne_bytes());
        attr[8..16].copy_from_slice(&(info.as_mut_ptr() as u64).to_ne_bytes());
        unsafe { bpf(BPF_OBJ_GET_INFO_BY_FD, attr.as_mut_ptr().cast(), attr.len())? };
        Ok(info)
    };
    let sized = read_info(0, 0)?;
    let size = u32::from_ne_bytes(sized[8..12].try_into().unwrap()) as usize;
    let mut buf = vec![0u8; size];
    read_info(buf.as_mut_ptr() as u64, size as u32)?;
    Ok(buf)
}

const BTF_MAGIC: u16 = 0xEB9F;
const BTF_KIND_FUNC: u32 = 12;

pub fn btf_func_id(btf: &[u8], name: &str) -> Option<u32> {
    if btf.len() < 24 || u16::from_ne_bytes(btf[0..2].try_into().ok()?) != BTF_MAGIC {
        return None;
    }
    let rd = |off: usize| u32::from_ne_bytes(btf[off..off + 4].try_into().unwrap());
    let hdr_len = rd(4) as usize;
    let type_off = hdr_len + rd(8) as usize;
    let type_len = rd(12) as usize;
    let str_off = hdr_len + rd(16) as usize;
    let str_len = rd(20) as usize;
    let types_end = type_off.checked_add(type_len)?;
    let str_end = str_off.checked_add(str_len)?;
    if types_end > btf.len() || str_end > btf.len() {
        return None;
    }
    let name_at = |off: usize| -> Option<&str> {
        let start = str_off.checked_add(off)?;
        if start >= str_end {
            return None;
        }
        let s = &btf[start..str_end];
        let end = s.iter().position(|&b| b == 0)?;
        std::str::from_utf8(&s[..end]).ok()
    };

    let mut cursor = type_off;
    let mut type_id: u32 = 1;
    while cursor + 12 <= types_end {
        let name_off = u32::from_ne_bytes(btf[cursor..cursor + 4].try_into().unwrap());
        let info = u32::from_ne_bytes(btf[cursor + 4..cursor + 8].try_into().unwrap());
        let vlen = (info & 0xffff) as usize;
        let kind = (info >> 24) & 0x1f;
        if kind == BTF_KIND_FUNC && name_at(name_off as usize) == Some(name) {
            return Some(type_id);
        }
        let trailing = btf_kind_trailing(kind, vlen)?;
        cursor = cursor.checked_add(12)?.checked_add(trailing)?;
        type_id = type_id.checked_add(1)?;
    }
    None
}

fn btf_kind_trailing(kind: u32, vlen: usize) -> Option<usize> {
    Some(match kind {
        1 => 4,
        14 => 4,
        17 => 4,
        3 => 12,
        4 | 5 => vlen * 12,
        6 => vlen * 8,
        13 => vlen * 8,
        15 => vlen * 12,
        19 => vlen * 12,
        2 | 7 | 8 | 9 | 10 | 11 | 12 | 16 | 18 => 0,
        _ => return None,
    })
}

pub fn link_create_freplace(
    prog_fd: BorrowedFd<'_>,
    target_prog_fd: BorrowedFd<'_>,
    target_btf_id: u32,
) -> io::Result<OwnedFd> {
    let mut attr = [0u8; 48];
    attr[0..4].copy_from_slice(&(prog_fd.as_raw_fd() as u32).to_ne_bytes());
    attr[4..8].copy_from_slice(&(target_prog_fd.as_raw_fd() as u32).to_ne_bytes());
    attr[16..20].copy_from_slice(&target_btf_id.to_ne_bytes());
    let fd = unsafe { bpf(BPF_LINK_CREATE, attr.as_mut_ptr().cast(), attr.len())? };
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

