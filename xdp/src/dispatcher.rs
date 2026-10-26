#![allow(clippy::arithmetic_side_effects)]

use {
    crate::{
        bpf_sys,
        device::NetworkDevice,
        netlink::{XdpAttachment, netlink_attach_xdp, netlink_detach_xdp, xdp_attachment},
    },
    aya::{
        Ebpf, EbpfLoader,
        programs::{Extension, Xdp, extension::ExtensionLinkId, links::FdLink},
    },
    std::{
        error::Error,
        fs, io,
        os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd},
        path::{Path, PathBuf},
    },
};

const DROP_MULTI_FRAGS_GLOBAL: &str = "AGAVE_XDP_DROP_MULTI_FRAGS";

const DISPATCHER_MAGIC: u8 = 236;
const DISPATCHER_VERSION: u8 = 3;
const DISPATCHER_RETVAL: u32 = 31;
pub const MAX_DISPATCHER_SLOTS: usize = 10;

const XDP_PASS: u32 = 2;

const BPFFS_XDP_DIR: &str = "/sys/fs/bpf/xdp";

const OUR_MEMBER_NAME: &str = "agave_xdp_rx";

const XSKS_MAP: &str = "XSKS";
const PORTS_MAP: &str = "RX_PORTS";

const PRESENCE_PREFIX: &str = "agave-";


fn member_dir(if_index: u32, prog_id: u32) -> PathBuf {
    presence_dir(if_index).join(format!("m{prog_id}"))
}

fn object_hash() -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in agave_xdp_ebpf::AGAVE_XDP_EBPF_PROGRAM.iter() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    format!("{hash:016x}")
}

pub struct MemberMarker {
    _fd: OwnedFd,
}

impl MemberMarker {
    pub fn claim(if_index: u32, prog_id: u32) -> Result<Self, Box<dyn Error>> {
        let dir = member_dir(if_index, prog_id);
        let build = dir.join(object_hash());
        fs::create_dir_all(&build).map_err(|err| bpffs_dir_error(&build.to_string_lossy(), err))?;
        Ok(Self {
            _fd: hold_shared(&dir)?,
        })
    }
}

fn hold_shared(dir: &Path) -> Result<OwnedFd, Box<dyn Error>> {
    let fd = open_dir(dir)?;
    for _ in 0..100 {
        if unsafe { libc::flock(fd.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } == 0 {
            return Ok(fd);
        }
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::WouldBlock {
            return Err(format!("lock on {} failed: {err}", dir.display()).into());
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    Err(format!(
        "lock on {} stayed held for a second; another process has it exclusively",
        dir.display()
    )
    .into())
}

pub fn any_member_live(if_index: u32) -> bool {
    let Ok(entries) = fs::read_dir(presence_dir(if_index)) else {
        return false;
    };
    entries.flatten().any(|entry| {
        entry
            .file_name()
            .to_string_lossy()
            .strip_prefix('m')
            .and_then(|id| id.parse::<u32>().ok())
            .is_some_and(|id| member_is_live(if_index, id))
    })
}

pub fn member_is_live(if_index: u32, prog_id: u32) -> bool {
    let dir = member_dir(if_index, prog_id);
    if !dir.is_dir() {
        return true;
    }
    let Ok(fd) = open_dir(&dir) else {
        return true;
    };
    let free = unsafe { libc::flock(fd.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    !free
}

pub fn member_build_matches(if_index: u32, prog_id: u32) -> bool {
    let wanted = object_hash();
    fs::read_dir(member_dir(if_index, prog_id))
        .map(|entries| {
            entries
                .flatten()
                .any(|entry| entry.file_name().to_string_lossy() == wanted)
        })
        .unwrap_or(false)
}

fn presence_dir(if_index: u32) -> PathBuf {
    Path::new(BPFFS_XDP_DIR).join(format!("{PRESENCE_PREFIX}{if_index}"))
}

fn open_dir(dir: &Path) -> Result<OwnedFd, Box<dyn Error>> {
    let path = std::ffi::CString::new(dir.to_string_lossy().as_bytes())?;
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY) };
    if fd < 0 {
        return Err(format!("open {}: {}", dir.display(), std::io::Error::last_os_error()).into());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct XdpDispatcherConfig {
    magic: u8,
    dispatcher_version: u8,
    num_progs_enabled: u8,
    is_xdp_frags: u8,
    chain_call_actions: [u32; MAX_DISPATCHER_SLOTS],
    run_prios: [u32; MAX_DISPATCHER_SLOTS],
    program_flags: [u32; MAX_DISPATCHER_SLOTS],
    is_xdp_devbound: u8,
    _pad: [u8; 3],
}

unsafe impl aya::Pod for XdpDispatcherConfig {}

impl XdpDispatcherConfig {
    fn pass_through(num_progs: u8) -> Self {
        let mut chain_call_actions = [0u32; MAX_DISPATCHER_SLOTS];
        let mut run_prios = [0u32; MAX_DISPATCHER_SLOTS];
        for i in 0..num_progs as usize {
            chain_call_actions[i] = (1 << XDP_PASS) | (1 << DISPATCHER_RETVAL);
            run_prios[i] = 50;
        }
        Self {
            magic: DISPATCHER_MAGIC,
            dispatcher_version: DISPATCHER_VERSION,
            num_progs_enabled: num_progs,
            is_xdp_frags: 0,
            chain_call_actions,
            run_prios,
            program_flags: [0; MAX_DISPATCHER_SLOTS],
            is_xdp_devbound: 0,
            _pad: [0; 3],
        }
    }

    fn from_foreign_plus_ours(foreign: &[MemberEntry]) -> Self {
        let mut chain_call_actions = [0u32; MAX_DISPATCHER_SLOTS];
        let mut run_prios = [0u32; MAX_DISPATCHER_SLOTS];
        let mut program_flags = [0u32; MAX_DISPATCHER_SLOTS];
        for (i, e) in foreign.iter().enumerate() {
            chain_call_actions[i] = e.action;
            run_prios[i] = e.prio;
            program_flags[i] = e.flags;
        }
        let ours = foreign.len();
        chain_call_actions[ours] = (1 << XDP_PASS) | (1 << DISPATCHER_RETVAL);
        run_prios[ours] = 50;
        Self {
            magic: DISPATCHER_MAGIC,
            dispatcher_version: DISPATCHER_VERSION,
            num_progs_enabled: (ours + 1) as u8,
            is_xdp_frags: 0,
            chain_call_actions,
            run_prios,
            program_flags,
            is_xdp_devbound: 0,
            _pad: [0; 3],
        }
    }
}

#[derive(Clone, Copy)]
struct MemberEntry {
    action: u32,
    prio: u32,
    flags: u32,
}

const CFG_CHAIN_CALL_ACTIONS: usize = std::mem::offset_of!(XdpDispatcherConfig, chain_call_actions);
const CFG_RUN_PRIOS: usize = std::mem::offset_of!(XdpDispatcherConfig, run_prios);
const CFG_PROGRAM_FLAGS: usize = std::mem::offset_of!(XdpDispatcherConfig, program_flags);
const CFG_IS_XDP_DEVBOUND: usize = std::mem::offset_of!(XdpDispatcherConfig, is_xdp_devbound);
const CFG_V2_LEN: usize = CFG_IS_XDP_DEVBOUND;
const CFG_V3_LEN: usize = size_of::<XdpDispatcherConfig>();
const _: () = assert!(CFG_V3_LEN == 128);
const _: () = assert!(CFG_CHAIN_CALL_ACTIONS == 4);
const _: () = assert!(CFG_RUN_PRIOS == 44);
const _: () = assert!(CFG_PROGRAM_FLAGS == 84);
const _: () = assert!(CFG_IS_XDP_DEVBOUND == 124);

fn parse_incumbent_config(bytes: &[u8]) -> Result<Vec<MemberEntry>, String> {
    if bytes.len() < CFG_V2_LEN || bytes[0] != DISPATCHER_MAGIC {
        return Err("not a libxdp dispatcher config (bad magic/length)".into());
    }
    let version = bytes[1];
    if version != 2 && version != 3 {
        return Err(format!("unsupported dispatcher config version {version} (expected 2 or 3)"));
    }
    if bytes[3] != 0 {
        return Err("xdp-frags-aware dispatcher is not supported for joining".into());
    }
    if version == 3 && bytes.len() >= CFG_V3_LEN && bytes[CFG_IS_XDP_DEVBOUND] != 0 {
        return Err("device-bound dispatcher is not supported for joining".into());
    }
    let num = bytes[2] as usize;
    if num > MAX_DISPATCHER_SLOTS {
        return Err(format!("dispatcher reports {num} members (> {MAX_DISPATCHER_SLOTS})"));
    }
    let rd = |off: usize| u32::from_ne_bytes(bytes[off..off + 4].try_into().unwrap());
    Ok((0..num)
        .map(|i| MemberEntry {
            action: rd(CFG_CHAIN_CALL_ACTIONS + i * 4),
            prio: rd(CFG_RUN_PRIOS + i * 4),
            flags: rd(CFG_PROGRAM_FLAGS + i * 4),
        })
        .collect())
}

struct Member {
    ebpf: Ebpf,
    prog_name: String,
    link_id: Option<ExtensionLinkId>,
}

fn bpffs_dir_error(path: &str, err: std::io::Error) -> Box<dyn Error> {
    match err.kind() {
        std::io::ErrorKind::NotFound => format!(
            "--xdp-chain-loading requires a bpf filesystem mounted at /sys/fs/bpf: that is \
             where the shared dispatcher is pinned, and it is how other XDP loaders find our \
             program. {path} cannot be created because /sys/fs/bpf is not a mounted bpf \
             filesystem. Mount it with `mount -t bpf bpf /sys/fs/bpf`; in a container, \
             bind-mount the host's and check with `mount | grep bpf` INSIDE the container that \
             it really arrived"
        ),
        std::io::ErrorKind::PermissionDenied => format!(
            "--xdp-chain-loading requires write access to /sys/fs/bpf, where the shared \
             dispatcher is pinned. {path} cannot be created: the filesystem is mounted, but it \
             is mode 700 and owned by root by default. Create /sys/fs/bpf/xdp and give it to the \
             user the validator runs as (`install -d -o <user> /sys/fs/bpf/xdp`); running the \
             validator as root would also work, but it does not need to be root for anything else"
        ),
        _ => format!(
            "--xdp-chain-loading requires {path} on the bpf filesystem, where the shared \
             dispatcher is pinned: {err}"
        ),
    }
    .into()
}

struct BpffsLock {
    fd: RawFd,
}

impl BpffsLock {
    fn acquire() -> Result<Self, Box<dyn Error>> {
        fs::create_dir_all(BPFFS_XDP_DIR).map_err(|err| bpffs_dir_error(BPFFS_XDP_DIR, err))?;
        let fd = unsafe {
            let path = std::ffi::CString::new(BPFFS_XDP_DIR)?;
            libc::open(path.as_ptr(), libc::O_DIRECTORY | libc::O_RDONLY)
        };
        if fd < 0 {
            return Err(format!("open {BPFFS_XDP_DIR}: {}", std::io::Error::last_os_error()).into());
        }
        if unsafe { libc::flock(fd, libc::LOCK_EX) } != 0 {
            let err = std::io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(format!("flock {BPFFS_XDP_DIR}: {err}").into());
        }
        Ok(Self { fd })
    }
}

impl Drop for BpffsLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.fd, libc::LOCK_UN);
            libc::close(self.fd);
        }
    }
}

pub struct Dispatcher {
    ebpf: Ebpf,
    members: Vec<Member>,
    slots: u8,
    our_slot: usize,
    prog_id: Option<u32>,
    if_index: Option<u32>,
    mode_drv: bool,
    has_foreign: bool,
    pin_dir: Option<PathBuf>,
    #[allow(dead_code)]
    marker: Option<MemberMarker>,
}

impl Dispatcher {
    pub fn load(num_progs: u8) -> Result<Self, Box<dyn Error>> {
        Self::load_with_config(XdpDispatcherConfig::pass_through(num_progs))
    }

    fn load_with_config(config: XdpDispatcherConfig) -> Result<Self, Box<dyn Error>> {
        let num_progs = config.num_progs_enabled;
        if num_progs as usize > MAX_DISPATCHER_SLOTS {
            return Err(format!("dispatcher supports at most {MAX_DISPATCHER_SLOTS} programs").into());
        }
        let mut ebpf = EbpfLoader::new()
            .override_global("conf", &config, true)
            .load(agave_xdp_ebpf::AGAVE_XDP_DISPATCHER_PROGRAM)?;
        let p: &mut Xdp = ebpf
            .program_mut("xdp_dispatcher")
            .ok_or("xdp_dispatcher program not found")?
            .try_into()?;
        p.load()?;
        let prog_id = {
            let p: &Xdp = ebpf.program("xdp_dispatcher").ok_or("dispatcher not loaded")?.try_into()?;
            p.info()?.id()
        };
        Ok(Self {
            ebpf,
            members: Vec::new(),
            slots: num_progs,
            our_slot: 0,
            prog_id: Some(prog_id),
            if_index: None,
            mode_drv: true,
            has_foreign: false,
            pin_dir: None,
            marker: None,
        })
    }

    fn build_and_attach_fresh(
        dev: &NetworkDevice,
        require_native: bool,
        replace: Option<XdpAttachment>,
        lock: &BpffsLock,
    ) -> Result<Self, Box<dyn Error>> {
        let drop_frags = is_i40e(dev);
        let mut me = Self::load(1)?;
        let (member, link_id) =
            me.load_freplace_member(agave_xdp_ebpf::AGAVE_XDP_EBPF_PROGRAM, "agave_xdp_rx", "prog0", drop_frags)?;
        me.members.push(Member {
            ebpf: member,
            prog_name: "agave_xdp_rx".to_string(),
            link_id: Some(link_id),
        });
        me.pin_and_attach(dev, require_native, replace, &[], &[], lock)?;
        Ok(me)
    }

    fn build_and_attach_joined(
        dev: &NetworkDevice,
        require_native: bool,
        att: XdpAttachment,
        incumbent_dir: &Path,
        foreign_slots: &[usize],
        lock: &BpffsLock,
    ) -> Result<Self, Box<dyn Error>> {
        let cfg_bytes = read_dispatcher_config(att.prog_id)?;
        let entries = parse_incumbent_config(&cfg_bytes).map_err(|e| {
            format!(
                "--xdp-chain-loading can only join a dispatcher it understands, and the one on \
                 {name} cannot be read: {e}. Detach it with `ip link set dev {name} xdp off`, \
                 or start the validator with --no-xdp and leave the interface to it",
                name = dev.name()
            )
        })?;
        let foreign_entries: Vec<MemberEntry> = foreign_slots
            .iter()
            .map(|&s| {
                entries
                    .get(s)
                    .copied()
                    .ok_or_else(|| format!("peer dispatcher member slot {s} missing from its config"))
            })
            .collect::<Result<_, _>>()?;
        let num_foreign = foreign_entries.len();
        if num_foreign + 1 > MAX_DISPATCHER_SLOTS {
            return Err(format!(
                "--xdp-chain-loading requires a free slot in the XDP dispatcher on {name}, and \
                 that dispatcher is full: its {num_foreign} members occupy all \
                 {MAX_DISPATCHER_SLOTS} slots libxdp allows. Detach one of them with \
                 `xdp-loader unload {name} --id <ID>`, listing them with `xdp-loader status`. If \
                 they are all ours, they are the members of earlier runs: while another instance \
                 is using this interface, a restarting one cannot tell that instance's member \
                 from its own previous one, so it keeps both. Stopping every instance on {name} \
                 clears the chain",
                name = dev.name()
            )
            .into());
        }

        let foreign: Vec<OwnedFd> = foreign_slots
            .iter()
            .map(|&s| bpf_sys::obj_get(&incumbent_dir.join(format!("prog{s}-prog"))))
            .collect::<Result<Vec<_>, _>>()?;

        let config = XdpDispatcherConfig::from_foreign_plus_ours(&foreign_entries);
        let mut me = Self::load_with_config(config)?;
        me.our_slot = num_foreign;
        me.has_foreign = true;
        let disp_fd = {
            let p: &Xdp = me.ebpf.program("xdp_dispatcher").ok_or("dispatcher not loaded")?.try_into()?;
            p.fd()?.try_clone()?
        };

        let btf = bpf_sys::btf_bytes_by_id(bpf_sys::prog_info(disp_fd.as_fd())?.btf_id)?;
        let mut foreign_links = Vec::with_capacity(num_foreign);
        for (k, ffd) in foreign.iter().enumerate() {
            let target = bpf_sys::btf_func_id(&btf, &format!("prog{k}"))
                .ok_or_else(|| format!("prog{k} not found in dispatcher BTF"))?;
            foreign_links.push(bpf_sys::link_create_freplace(ffd.as_fd(), disp_fd.as_fd(), target)?);
        }

        let (member, link_id) = me.load_freplace_member(
            agave_xdp_ebpf::AGAVE_XDP_EBPF_PROGRAM,
            "agave_xdp_rx",
            &format!("prog{num_foreign}"),
            is_i40e(dev),
        )?;
        me.members.push(Member {
            ebpf: member,
            prog_name: "agave_xdp_rx".to_string(),
            link_id: Some(link_id),
        });

        me.pin_and_attach(dev, require_native, Some(att), &foreign, &foreign_links, lock)?;
        Ok(me)
    }

    fn load_freplace_member(
        &self,
        object: &[u8],
        prog_name: &str,
        target_func: &str,
        drop_multi_frags: bool,
    ) -> Result<(Ebpf, ExtensionLinkId), Box<dyn Error>> {
        let mut loader = EbpfLoader::new();
        loader.extension(prog_name);
        if drop_multi_frags {
            loader.override_global(DROP_MULTI_FRAGS_GLOBAL, &1u8, true);
        }
        let mut member = loader.load(object)?;
        let dispatcher: &Xdp = self
            .ebpf
            .program("xdp_dispatcher")
            .ok_or("dispatcher not loaded")?
            .try_into()?;
        let dispatcher_fd = dispatcher.fd()?.try_clone()?;
        let ext: &mut Extension = member
            .program_mut(prog_name)
            .ok_or_else(|| format!("{prog_name} not found in member object"))?
            .try_into()?;
        ext.load(dispatcher_fd, target_func)?;
        let link_id = ext.attach()?;
        Ok((member, link_id))
    }

    pub fn attach_member(
        &mut self,
        object: &[u8],
        prog_name: &str,
    ) -> Result<&mut Ebpf, Box<dyn Error>> {
        let slot = self.members.len();
        if slot >= self.slots as usize {
            return Err(format!("all {} dispatcher slots are configured", self.slots).into());
        }
        let (member, link_id) =
            self.load_freplace_member(object, prog_name, &format!("prog{slot}"), false)?;
        self.members.push(Member {
            ebpf: member,
            prog_name: prog_name.to_string(),
            link_id: Some(link_id),
        });
        Ok(&mut self.members.last_mut().expect("just pushed").ebpf)
    }

    pub fn attach(&mut self, dev: &NetworkDevice, require_native: bool) -> Result<(), Box<dyn Error>> {
        self.attach_and_replace(dev, require_native, None)
    }

    fn attach_and_replace(
        &mut self,
        dev: &NetworkDevice,
        require_native: bool,
        replace: Option<XdpAttachment>,
    ) -> Result<(), Box<dyn Error>> {
        let prog_fd = {
            let p: &Xdp = self
                .ebpf
                .program("xdp_dispatcher")
                .ok_or("dispatcher not loaded")?
                .try_into()?;
            p.fd()?.as_fd().as_raw_fd()
        };
        let ifindex = dev.if_index();
        match replace {
            Some(att) => {
                if require_native && !att.drv {
                    return Err(format!(
                        "--xdp-zero-copy requires native (driver) mode, and the XDP dispatcher \
                         already on {name} is attached in generic (SKB) mode, which cannot be \
                         upgraded in place. Detach it with `ip link set dev {name} xdp off` so \
                         it can be reattached natively, or start without --xdp-zero-copy",
                        name = dev.name()
                    )
                    .into());
                }
                let expected = bpf_sys::prog_fd_by_id(att.prog_id)?;
                netlink_attach_xdp(ifindex, prog_fd, att.drv, Some(expected.as_raw_fd())).map_err(
                    |e| format!("replacing the XDP dispatcher on {} failed: {e}", dev.name()),
                )?;
                self.mode_drv = att.drv;
            }
            None => match netlink_attach_xdp(ifindex, prog_fd, true, None) {
                Ok(()) => self.mode_drv = true,
                Err(e) if require_native => {
                    return Err(format!(
                        "--xdp-zero-copy requires the XDP dispatcher to attach in native (driver) \
                         mode, which failed on {}: {e}. The driver may not support native XDP — \
                         start without --xdp-zero-copy to run in copy mode instead",
                        dev.name()
                    )
                    .into());
                }
                Err(e) => {
                    log::debug!(
                        "xdp dispatcher: native attach to {} failed ({e}); using skb mode",
                        dev.name()
                    );
                    netlink_attach_xdp(ifindex, prog_fd, false, None)?;
                    self.mode_drv = false;
                }
            },
        }
        self.if_index = Some(ifindex);
        Ok(())
    }

    fn pin_and_attach(
        &mut self,
        dev: &NetworkDevice,
        require_native: bool,
        replace: Option<XdpAttachment>,
        foreign: &[OwnedFd],
        foreign_links: &[OwnedFd],
        lock: &BpffsLock,
    ) -> Result<(), Box<dyn Error>> {
        if let Err(e) = self.pin_members(dev.if_index(), foreign, foreign_links, lock) {
            self.cleanup_pin_dir();
            return Err(e);
        }
        if let Err(e) = self.attach_and_replace(dev, require_native, replace) {
            self.cleanup_pin_dir();
            return Err(e);
        }
        Ok(())
    }

    fn our_member_id(&self) -> Result<u32, Box<dyn Error>> {
        let member = self.members.first().ok_or("dispatcher has no member of ours")?;
        let program: &Extension = member
            .ebpf
            .program(&member.prog_name)
            .ok_or("our member program is not in its object")?
            .try_into()?;
        Ok(bpf_sys::prog_info(program.fd()?.as_fd())?.id)
    }

    fn record_member(&mut self, if_index: u32) -> Result<(), Box<dyn Error>> {
        let prog_id = self.our_member_id()?;
        let marker = MemberMarker::claim(if_index, prog_id)?;
        let dir = member_dir(if_index, prog_id);
        for name in [XSKS_MAP, PORTS_MAP] {
            let path = dir.join(name.to_lowercase());
            if path.exists() {
                continue;
            }
            let map = self
                .our_member_mut()
                .map(name)
                .ok_or_else(|| format!("{name} map not found in our member"))?;
            map.pin(&path)?;
        }
        self.marker = Some(marker);
        Ok(())
    }

    pub fn abandon_presence(&mut self) {
        self.marker = None;
    }

    fn cleanup_pin_dir(&mut self) {
        if let Some(dir) = self.pin_dir.take() {
            let _ = fs::remove_dir_all(&dir);
        }
    }

    pub fn pin(&mut self, if_index: u32) -> Result<(), Box<dyn Error>> {
        let lock = BpffsLock::acquire()?;
        self.pin_members(if_index, &[], &[], &lock)
    }

    fn pin_members(
        &mut self,
        if_index: u32,
        foreign: &[OwnedFd],
        foreign_links: &[OwnedFd],
        _lock: &BpffsLock,
    ) -> Result<(), Box<dyn Error>> {
        let prog_id = self.prog_id.ok_or("dispatcher not loaded")?;
        let dir = Path::new(BPFFS_XDP_DIR).join(format!("dispatch-{if_index}-{prog_id}"));
        fs::create_dir_all(&dir).map_err(|err| bpffs_dir_error(&dir.to_string_lossy(), err))?;
        self.pin_dir = Some(dir.clone());
        for (i, (prog, link)) in foreign.iter().zip(foreign_links).enumerate() {
            bpf_sys::obj_pin(prog.as_fd(), &dir.join(format!("prog{i}-prog")))?;
            bpf_sys::obj_pin(link.as_fd(), &dir.join(format!("prog{i}-link")))?;
        }
        let base = self.our_slot;
        for (j, member) in self.members.iter_mut().enumerate() {
            let slot = base + j;
            let ext: &mut Extension = member
                .ebpf
                .program_mut(&member.prog_name)
                .ok_or("member program missing at pin")?
                .try_into()?;
            ext.pin(dir.join(format!("prog{slot}-prog")))?;
            let link_id = member.link_id.take().ok_or("member freplace link already taken")?;
            let fd_link: FdLink = ext.take_link(link_id)?.into();
            fd_link.pin(dir.join(format!("prog{slot}-link")))?;
        }
        Ok(())
    }

    pub fn our_member_mut(&mut self) -> &mut Ebpf {
        &mut self
            .members
            .get_mut(0)
            .expect("dispatcher always has our member")
            .ebpf
    }
}

fn is_i40e(dev: &NetworkDevice) -> bool {
    dev.driver().map(|d| d == "i40e").unwrap_or(false)
}

fn sweep_orphan_dirs(if_index: u32, keep_prog_id: u32, _lock: &BpffsLock) {
    if xdp_attachment(if_index).ok().flatten().map(|a| a.prog_id) != Some(keep_prog_id) {
        return;
    }
    let prefix = format!("dispatch-{if_index}-");
    let keep = format!("dispatch-{if_index}-{keep_prog_id}");
    let Ok(entries) = fs::read_dir(BPFFS_XDP_DIR) else {
        return;
    };
    for entry in entries.flatten() {
        match entry.file_name().to_str() {
            Some(name) if name.starts_with(&prefix) && name != keep => {
                let _ = fs::remove_dir_all(entry.path());
            }
            _ => {}
        }
    }
}

pub enum Incumbent {
    Free,
    OurLeftover(XdpAttachment),
    LiveSibling(XdpAttachment),
    ForeignDispatcher(XdpAttachment),
    Opaque(u32),
    Unreadable(u32),
}

pub fn classify(dev: &NetworkDevice) -> Result<Incumbent, Box<dyn Error>> {
    let if_index = dev.if_index();
    let Some(attachment) = xdp_attachment(if_index)? else {
        return Ok(Incumbent::Free);
    };
    let dir = pin_dir(if_index, attachment.prog_id);
    match fs::metadata(&dir) {
        Ok(meta) if meta.is_dir() => {}
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            return Ok(Incumbent::Unreadable(attachment.prog_id));
        }
        _ => return Ok(Incumbent::Opaque(attachment.prog_id)),
    }
    let members = read_members(&dir).unwrap_or_default();
    let ours = !members.is_empty() && members.iter().all(|(name, _)| name == OUR_MEMBER_NAME);
    let live = members
        .iter()
        .any(|(_, prog_id)| member_is_live(if_index, *prog_id));
    Ok(match (ours, live) {
        (true, false) => Incumbent::OurLeftover(attachment),
        (true, true) => Incumbent::LiveSibling(attachment),
        (false, _) => Incumbent::ForeignDispatcher(attachment),
    })
}

pub const CHANGED_UNDER_US: &str = "changed while it was being cleaned up";

pub fn remove_leftover(dev: &NetworkDevice, attachment: &XdpAttachment) -> Result<(), Box<dyn Error>> {
    let if_index = dev.if_index();
    let lock = BpffsLock::acquire()?;
    match classify(dev)? {
        Incumbent::OurLeftover(current) if current.prog_id == attachment.prog_id => {}
        _ => {
            return Err(format!("the XDP program on {} {CHANGED_UNDER_US}", dev.name()).into());
        }
    }
    netlink_detach_xdp(if_index, attachment.drv)?;
    let _ = fs::remove_dir_all(pin_dir(if_index, attachment.prog_id));
    drop(lock);
    Ok(())
}

fn pin_dir(if_index: u32, prog_id: u32) -> PathBuf {
    Path::new(BPFFS_XDP_DIR).join(format!("dispatch-{if_index}-{prog_id}"))
}

pub struct AdoptedMember {
    xsks: OwnedFd,
    ports: OwnedFd,
    #[allow(dead_code)]
    marker: MemberMarker,
    pub prog_id: u32,
}

impl AdoptedMember {
    pub fn add_rx_port(&self, port: u16) -> io::Result<()> {
        bpf_sys::map_update(self.ports.as_fd(), &port.to_ne_bytes(), &[1u8])
    }

    fn disarm(&self) -> io::Result<()> {
        let mut key = None;
        let mut ports = Vec::new();
        while let Some(next) = bpf_sys::map_next_key(self.ports.as_fd(), key.as_deref(), 2)? {
            ports.push(next.clone());
            key = Some(next);
        }
        for port in ports {
            bpf_sys::map_delete(self.ports.as_fd(), &port)?;
        }
        Ok(())
    }

    pub fn register_xsk(&self, queue: u32, fd: RawFd) -> io::Result<()> {
        bpf_sys::map_update(
            self.xsks.as_fd(),
            &queue.to_ne_bytes(),
            &(fd as u32).to_ne_bytes(),
        )
    }
}

pub fn adoptable(dev: &NetworkDevice) -> Option<AdoptedMember> {
    let (prog_id, xsks, ports) = inspect_adoptable(dev)?;
    let marker = MemberMarker::claim(dev.if_index(), prog_id).ok()?;
    let member = AdoptedMember {
        xsks,
        ports,
        marker,
        prog_id,
    };
    member.disarm().ok()?;
    Some(member)
}

pub fn can_adopt(dev: &NetworkDevice) -> bool {
    inspect_adoptable(dev).is_some()
}

fn inspect_adoptable(dev: &NetworkDevice) -> Option<(u32, OwnedFd, OwnedFd)> {
    let if_index = dev.if_index();
    let attachment = xdp_attachment(if_index).ok().flatten()?;
    let dir = pin_dir(if_index, attachment.prog_id);
    let members = read_members(&dir).ok()?;
    let (_, prog_id) = members.into_iter().find(|(name, id)| {
        name.as_str() == OUR_MEMBER_NAME && !member_is_live(if_index, *id)
            && member_build_matches(if_index, *id)
    })?;

    let member = member_dir(if_index, prog_id);
    let xsks = bpf_sys::obj_get(&member.join(XSKS_MAP.to_lowercase())).ok()?;
    let ports = bpf_sys::obj_get(&member.join(PORTS_MAP.to_lowercase())).ok()?;

    let member_prog = bpf_sys::obj_get(&pin_dir(if_index, attachment.prog_id).join(
        format!("prog{}-prog", slot_of(&pin_dir(if_index, attachment.prog_id), prog_id)?),
    ))
    .ok()?;
    let used = bpf_sys::prog_info(member_prog.as_fd()).ok()?.map_ids;
    for map in [&xsks, &ports] {
        let id = bpf_sys::map_info(map.as_fd()).ok()?.id;
        if !used.contains(&id) {
            log::debug!(
                "xdp: the pinned maps under m{prog_id} are not the ones its program uses; not \
                 adopting"
            );
            return None;
        }
    }
    Some((prog_id, xsks, ports))
}

fn slot_of(dir: &Path, prog_id: u32) -> Option<usize> {
    read_members(dir)
        .ok()?
        .into_iter()
        .position(|(_, id)| id == prog_id)
}

pub fn attach_or_join(dev: &NetworkDevice, require_native: bool) -> Result<Dispatcher, Box<dyn Error>> {
    let ifindex = dev.if_index();
    let lock = BpffsLock::acquire()?;
    let mut dispatcher = match xdp_attachment(ifindex)? {
        None => Dispatcher::build_and_attach_fresh(dev, require_native, None, &lock)?,
        Some(att) => {
            let dir = Path::new(BPFFS_XDP_DIR).join(format!("dispatch-{ifindex}-{}", att.prog_id));
            if !dir.is_dir() {
                return Err(format!(
                    "--xdp-chain-loading requires the incumbent program to be a libxdp \
                     dispatcher, and {name} has a non-dispatcher XDP program on its hook (id \
                     {id}) — there is nothing to chain onto. Load that program through libxdp \
                     (`xdp-loader load`) so both can share the hook, detach it with `ip link \
                     set dev {name} xdp off`, or start the validator with --no-xdp and leave \
                     the interface to it",
                    name = dev.name(),
                    id = att.prog_id,
                )
                .into());
            }
            let members = read_members(&dir)?;
            let names: Vec<String> = members.iter().map(|(name, _)| name.clone()).collect();
            let foreign_slots: Vec<usize> = members
                .iter()
                .enumerate()
                .filter(|(_, (name, id))| {
                    name.as_str() != OUR_MEMBER_NAME || member_is_live(ifindex, *id)
                })
                .map(|(slot, _)| slot)
                .collect();
            if !foreign_slots.is_empty() {
                log::debug!(
                    "xdp chain-loading: keeping {} of {} members on {}",
                    foreign_slots.len(),
                    members.len(),
                    dev.name()
                );
            }
            if names.is_empty() && any_member_live(ifindex) {
                return Err(format!(
                    "{} carries an XDP dispatcher that is in use, but its pin directory is \
                     empty, so its members cannot be identified; refusing to rebuild it. Stop \
                     the other user of this interface, or detach the program with `ip link set \
                     dev {} xdp off`",
                    dev.name(),
                    dev.name()
                )
                .into());
            }
            if foreign_slots.is_empty() {
                log::debug!(
                    "xdp chain-loading: replacing our own stale dispatcher on {}",
                    dev.name()
                );
                Dispatcher::build_and_attach_fresh(dev, require_native, Some(att), &lock)?
            } else {
                Dispatcher::build_and_attach_joined(dev, require_native, att, &dir, &foreign_slots, &lock)?
            }
        }
    };
    if let Some(prog_id) = dispatcher.prog_id {
        sweep_orphan_dirs(ifindex, prog_id, &lock);
    }
    drop(lock);

    dispatcher.record_member(ifindex)?;
    sweep_member_markers(ifindex, &current_member_ids(ifindex));
    Ok(dispatcher)
}

fn current_member_ids(if_index: u32) -> Vec<u32> {
    let prefix = format!("dispatch-{if_index}-");
    let Ok(entries) = fs::read_dir(BPFFS_XDP_DIR) else {
        return Vec::new();
    };
    let mut ids = Vec::new();
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(&prefix)
            && let Ok(members) = read_members(&entry.path())
        {
            ids.extend(members.into_iter().map(|(_, id)| id));
        }
    }
    ids
}

fn sweep_member_markers(if_index: u32, current: &[u32]) {
    let Ok(entries) = fs::read_dir(presence_dir(if_index)) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(id) = name.strip_prefix('m').and_then(|id| id.parse::<u32>().ok()) else {
            continue;
        };
        if current.contains(&id) || member_is_live(if_index, id) {
            continue;
        }
        let _ = fs::remove_dir_all(entry.path());
    }
}

fn read_dispatcher_config(prog_id: u32) -> Result<Vec<u8>, Box<dyn Error>> {
    let fd = bpf_sys::prog_fd_by_id(prog_id)?;
    let info = bpf_sys::prog_info(fd.as_fd())?;
    for map_id in info.map_ids {
        let map_fd = bpf_sys::map_fd_by_id(map_id)?;
        let map = bpf_sys::map_info(map_fd.as_fd())?;
        if map.map_type == bpf_sys::BPF_MAP_TYPE_ARRAY
            && map.name.contains("rodata")
            && (44..=256).contains(&map.value_size)
        {
            return Ok(bpf_sys::map_lookup_first(map_fd.as_fd(), map.value_size as usize)?);
        }
    }
    Err(format!("dispatcher {prog_id} has no readable .rodata config map").into())
}

fn read_members(dir: &Path) -> Result<Vec<(String, u32)>, Box<dyn Error>> {
    let mut members = Vec::new();
    for slot in 0..MAX_DISPATCHER_SLOTS {
        let prog = dir.join(format!("prog{slot}-prog"));
        if !prog.exists() {
            break;
        }
        let fd = bpf_sys::obj_get(&prog)?;
        let info = bpf_sys::prog_info(fd.as_fd())?;
        members.push((info.name, info.id));
    }
    Ok(members)
}

impl Drop for Dispatcher {
    fn drop(&mut self) {
        if self.has_foreign {
            return;
        }
        let (Some(if_index), Some(prog_id)) = (self.if_index, self.prog_id) else {
            return;
        };
        if xdp_attachment(if_index).ok().flatten().map(|a| a.prog_id) != Some(prog_id) {
            return;
        }
        let _ = netlink_detach_xdp(if_index, self.mode_drv);
        if let Some(dir) = self.pin_dir.take()
            && let Ok(_lock) = BpffsLock::acquire()
        {
            let _ = fs::remove_dir_all(&dir);
        }
    }
}

