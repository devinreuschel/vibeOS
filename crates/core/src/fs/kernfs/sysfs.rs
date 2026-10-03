use super::*;

impl<S: Guarded<KernState>> KernFs<S> {
    /// PCI device + driver binding under the mounted sysfs. `name` is the
    /// BDF (`00:01.0`). `driver` none or empty → unbound (`-`).
    pub fn sysfs_add_device(
        &self,
        name: &[u8],
        vendor: u16,
        device: u16,
        class: u8,
        driver: Option<&[u8]>,
    ) -> Result<(), FsError> {
        self.with_room(SYSFS_DEVICE_NODES, |k| {
            sysfs_add(k, name, vendor, device, class, driver)
        })
    }
}

fn sysfs_add(
    k: &mut KernState,
    name: &[u8],
    vendor: u16,
    device: u16,
    class: u8,
    driver: Option<&[u8]>,
) -> Result<(), FsError> {
    let (inst, _) = k.skin(FsType::Sys).ok_or(FsError::Io)?;
    let mut made = Made::new();
    let r = sysfs_make(k, &mut made, name, vendor, device, class, driver);
    if r.is_err() {
        made.undo(k, inst);
    }
    r
}

/// The nodes one [`sysfs_add`] made, oldest first, as `(parent, ino,
/// dir)`; a node it found already there is not among them. It makes at
/// most [`SYSFS_DEVICE_NODES`], the room `with_room` reserved for it.
struct Made {
    nodes: [(u32, u32, bool); SYSFS_DEVICE_NODES],
    n: usize,
}

impl Made {
    const fn new() -> Self {
        Self {
            nodes: [(0, 0, false); SYSFS_DEVICE_NODES],
            n: 0,
        }
    }

    fn push(&mut self, parent: u32, ino: u32, dir: bool) {
        if let Some(slot) = self.nodes.get_mut(self.n) {
            *slot = (parent, ino, dir);
            self.n += 1;
        }
    }

    /// Take back every node made, newest first, so a failed add leaves
    /// sysfs as it found it: out of its parent's children, a directory's
    /// link off its parent's count, then the node. Nothing else has seen
    /// them: the add runs under one hold of the store's lock.
    fn undo(&self, k: &mut KernState, inst: u32) {
        for &(parent, ino, dir) in self.nodes.iter().take(self.n).rev() {
            kern_unlink_child(k, parent, ino);
            if dir && let Some(p) = kern_get_mut(k, inst, parent) {
                p.nlink = p.nlink.saturating_sub(1);
            }
            if let Some(i) = kern_idx(k, ino) {
                kern_release(k, i);
            }
        }
    }
}

/// [`sysfs_add`]'s nodes, each one it makes recorded in `made`.
fn sysfs_make(
    k: &mut KernState,
    made: &mut Made,
    name: &[u8],
    vendor: u16,
    device: u16,
    class: u8,
    driver: Option<&[u8]>,
) -> Result<(), FsError> {
    let (inst, root) = k.skin(FsType::Sys).ok_or(FsError::Io)?;
    let now = k.now;
    let devices = kern_lookup_ino(k, inst, root, b"devices")?;
    let bus = kern_lookup_ino(k, inst, root, b"bus")?;
    let pci = kern_lookup_ino(k, inst, bus, b"pci")?;
    let pci_devs = kern_lookup_ino(k, inst, pci, b"devices")?;
    let pci_drvs = kern_lookup_ino(k, inst, pci, b"drivers")?;

    let ddir = dir_in(k, made, now, inst, devices, name)?;
    let packed = ((vendor as u64) << 16) | (device as u64);
    let drv_bytes = match driver {
        Some(d) if !d.is_empty() => d,
        _ => b"-",
    };
    for (attr, which, drv) in [
        (&b"vendor"[..], SYS_ATTR_VENDOR, None),
        (&b"device"[..], SYS_ATTR_DEVICE, None),
        (&b"class"[..], SYS_ATTR_CLASS, None),
        (&b"driver"[..], SYS_ATTR_DRIVER, Some(drv_bytes)),
    ] {
        let new = kern_find_child(k, inst, ddir, attr).is_none();
        let ino = kern_mk_sys_attr(k, now, inst, ddir, attr, which, packed, class, drv)?;
        if new {
            made.push(ddir, ino, false);
        }
    }

    let mut rel = [0u8; MAX_NAME];
    let rlen = rel_to_devices(name, &mut rel)?;
    lnk_in(k, made, now, inst, pci_devs, name, &rel[..rlen])?;

    if drv_bytes != b"-" {
        let ddir_drv = dir_in(k, made, now, inst, pci_drvs, drv_bytes)?;
        lnk_in(k, made, now, inst, ddir_drv, name, &rel[..rlen])?;
    }
    Ok(())
}

/// Directory `name` in `parent`, made when it is missing.
fn dir_in(
    k: &mut KernState,
    made: &mut Made,
    now: u64,
    inst: u32,
    parent: u32,
    name: &[u8],
) -> Result<u32, FsError> {
    match kern_lookup_ino(k, inst, parent, name) {
        Ok(ino) => Ok(ino),
        Err(FsError::NotFound) => {
            let ino = kern_mk_dir(k, now, inst, parent, name)?;
            made.push(parent, ino, true);
            Ok(ino)
        }
        Err(e) => Err(e),
    }
}

/// Link `name` in `parent` to `target`, made when it is missing.
fn lnk_in(
    k: &mut KernState,
    made: &mut Made,
    now: u64,
    inst: u32,
    parent: u32,
    name: &[u8],
    target: &[u8],
) -> Result<(), FsError> {
    if kern_find_child(k, inst, parent, name).is_some() {
        return Ok(());
    }
    let ino = kern_mk_lnk(k, now, inst, parent, name, target)?;
    made.push(parent, ino, false);
    Ok(())
}

fn rel_to_devices(name: &[u8], out: &mut [u8; MAX_NAME]) -> Result<usize, FsError> {
    // ../../../devices/<name>
    let p = b"../../../devices/";
    if p.len() + name.len() > MAX_NAME {
        return Err(FsError::NameTooLong);
    }
    out[..p.len()].copy_from_slice(p);
    out[p.len()..p.len() + name.len()].copy_from_slice(name);
    Ok(p.len() + name.len())
}

#[allow(clippy::too_many_arguments)] // sysfs attr packed PCI identity
fn kern_mk_sys_attr(
    k: &mut KernState,
    now: u64,
    inst: u32,
    parent: u32,
    name: &[u8],
    which: u8,
    packed: u64,
    class: u8,
    driver: Option<&[u8]>,
) -> Result<u32, FsError> {
    if let Some(ino) = kern_find_child(k, inst, parent, name) {
        return Ok(ino);
    }
    let nm = Name::from_bytes(name)?;
    let ino = kern_alloc(k, inst)?;
    let t = now;
    let size = match which {
        SYS_ATTR_VENDOR | SYS_ATTR_DEVICE => 7,
        SYS_ATTR_CLASS => 5,
        SYS_ATTR_DRIVER => {
            if let Some(d) = driver {
                (d.len() + 1) as u64
            } else {
                2
            }
        }
        _ => 0,
    };
    let mut bad = Ok(());
    if let Some(n) = kern_get_mut(k, inst, ino) {
        n.kind = KernKind::SysAttr;
        n.mode = S_IFREG_MODE;
        n.nlink = 1;
        n.size = size;
        n.atime = t;
        n.mtime = t;
        n.ctime = t;
        n.name = nm;
        if let Some(d) = driver {
            bad = set_target(&mut n.target, &mut n.target_len, d);
        }
        n.tag = packed;
        n.tag2 = ((class as u64) << 8) | (which as u64);
    }
    if let Err(e) = bad {
        if let Some(i) = kern_idx(k, ino) {
            kern_release(k, i);
        }
        return Err(e);
    }
    kern_link(k, parent, ino);
    touch_dir(k, inst, parent, t);
    Ok(ino)
}

pub(super) fn sys_attr_read(
    k: &KernState,
    inst: u32,
    ino: u32,
    off: u64,
    buf: &mut [u8],
) -> Result<usize, FsError> {
    let n = kern_get(k, inst, ino).ok_or(FsError::NotFound)?;
    let which = (n.tag2 & 0xff) as u8;
    let class = ((n.tag2 >> 8) & 0xff) as u8;
    let vendor = (n.tag >> 16) as u16;
    let device = (n.tag & 0xffff) as u16;
    let mut tmp = [0u8; 64];
    let len = match which {
        SYS_ATTR_VENDOR => fmt_hex_u16(vendor, &mut tmp),
        SYS_ATTR_DEVICE => fmt_hex_u16(device, &mut tmp),
        SYS_ATTR_CLASS => fmt_hex_u8(class, &mut tmp),
        SYS_ATTR_DRIVER => {
            let d = target_bytes(n);
            if d.is_empty() {
                tmp[0] = b'-';
                tmp[1] = b'\n';
                2
            } else {
                let n = d.len().min(62);
                tmp[..n].copy_from_slice(&d[..n]);
                tmp[n] = b'\n';
                n + 1
            }
        }
        _ => 0,
    };
    copy_off(&tmp[..len], off, buf)
}

fn hex_nib(d: u8) -> u8 {
    if d < 10 { b'0' + d } else { b'a' + (d - 10) }
}

fn fmt_hex_u16(n: u16, out: &mut [u8]) -> usize {
    if out.len() < 7 {
        return 0;
    }
    out[0] = b'0';
    out[1] = b'x';
    out[2] = hex_nib(((n >> 12) & 0xf) as u8);
    out[3] = hex_nib(((n >> 8) & 0xf) as u8);
    out[4] = hex_nib(((n >> 4) & 0xf) as u8);
    out[5] = hex_nib((n & 0xf) as u8);
    out[6] = b'\n';
    7
}

fn fmt_hex_u8(n: u8, out: &mut [u8]) -> usize {
    if out.len() < 5 {
        return 0;
    }
    out[0] = b'0';
    out[1] = b'x';
    out[2] = hex_nib((n >> 4) & 0xf);
    out[3] = hex_nib(n & 0xf);
    out[4] = b'\n';
    5
}
