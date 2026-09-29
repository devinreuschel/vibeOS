use super::*;

/// The backup boot sector (BPB_BkBootSec).
const BACKUP_BOOT: u32 = 6;
/// The backup FSInfo sector, after the backup boot sector.
const BACKUP_FSINFO: u32 = BACKUP_BOOT + 1;

/// Reserved sectors [`mkfs`] writes: the boot sector, FSInfo and their
/// backups.
const RSVD: u32 = 32;
/// FAT copies [`mkfs`] writes.
const NUM_FATS: u8 = 2;
/// Sectors per cluster [`mkfs`] writes.
const SPC: u8 = 1;

/// The smallest image [`mkfs`] formats: the reserved sectors, two
/// one-sector FATs and two data clusters.
pub const MIN_SECTORS: u32 = 36;

/// Free space `mkinitrd` leaves in the initrd for the in-guest write tests
/// (ROADMAP §10.5): room for the about 180 KB `exec_large_elf_from_file`
/// holds at once, and for the small FAT write tests.
pub const INITRD_FREE_BYTES: u64 = 512 * 1024;

/// Where [`mkfs`] puts things on an image of `totsec` sectors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    /// Sectors per FAT copy.
    pub fatsz: u32,
    /// First data sector.
    pub data_lba: u32,
    /// Data clusters.
    pub nclus: u32,
}

/// The layout [`mkfs`] writes on `totsec` sectors: [`RSVD`] reserved
/// sectors, two FATs just big enough for the clusters that follow them, and
/// one sector per cluster. `Inval` below two clusters.
pub fn geometry(totsec: u32) -> Result<Geometry, FatError> {
    // The first data sector for a FAT of `fatsz` sectors.
    let data_start = |fatsz: u32| {
        u32::from(NUM_FATS)
            .checked_mul(fatsz)
            .and_then(|f| f.checked_add(RSVD))
            .ok_or(FatError::Inval)
    };
    let mut fatsz = 1u32;
    loop {
        let data = totsec.saturating_sub(data_start(fatsz)?);
        let nclus = data.checked_div(u32::from(SPC)).ok_or(FatError::Inval)?;
        if nclus < 2 {
            return Err(FatError::Inval);
        }
        let need = nclus
            .checked_add(2)
            .and_then(|n| n.checked_mul(4))
            .ok_or(FatError::Inval)?
            .div_ceil(SEC as u32);
        if need <= fatsz {
            break;
        }
        fatsz = need;
        if data_start(fatsz)? >= totsec {
            return Err(FatError::Inval);
        }
    }
    let data_lba = data_start(fatsz)?;
    let nclus = totsec
        .checked_sub(data_lba)
        .and_then(|n| n.checked_div(u32::from(SPC)))
        .ok_or(FatError::Inval)?;
    Ok(Geometry {
        fatsz,
        data_lba,
        nclus,
    })
}

/// The smallest image, in sectors, whose [`geometry`] has at least
/// `data_clusters` clusters plus enough for `free_bytes` more. `Inval` when
/// no `u32` sector count has.
pub fn image_sectors(data_clusters: u32, free_bytes: u64) -> Result<u32, FatError> {
    let free = u32::try_from(free_bytes.div_ceil(SEC as u64)).map_err(|_| FatError::Inval)?;
    let need = data_clusters.checked_add(free).ok_or(FatError::Inval)?;
    // A FAT is at least one sector, so no image below `need` data sectors
    // past the reserved sectors and two one-sector FATs has enough.
    let mut totsec = need
        .checked_add(RSVD)
        .and_then(|n| n.checked_add(u32::from(NUM_FATS)))
        .ok_or(FatError::Inval)?
        .max(MIN_SECTORS);
    loop {
        if let Ok(g) = geometry(totsec)
            && g.nclus >= need
        {
            return Ok(totsec);
        }
        totsec = totsec.checked_add(1).ok_or(FatError::Inval)?;
    }
}

/// Format `buf` as FAT32. Its length must be a multiple of 512 and at
/// least [`MIN_SECTORS`] sectors.
pub fn mkfs(buf: &mut [u8], label: &[u8]) -> Result<FatInfo, FatError> {
    if !buf.len().is_multiple_of(SEC) {
        return Err(FatError::Inval);
    }
    let totsec = u32::try_from(buf.len() / SEC).map_err(|_| FatError::Inval)?;
    let Geometry {
        fatsz,
        data_lba,
        nclus,
    } = geometry(totsec)?;
    buf.fill(0);
    let rsvd = RSVD;
    let spc = SPC;
    let num_fats = NUM_FATS;
    let root = 2u32;
    let mut lab = [b' '; 11];
    lab.iter_mut().zip(label).for_each(|(o, &c)| *o = c);
    let mut boot = [0u8; SEC];
    boot[0] = 0xEB;
    boot[1] = 0x58;
    boot[2] = 0x90;
    boot[3..11].copy_from_slice(b"VIBEOS  ");
    put_le16(&mut boot, 11, SEC as u16)?;
    boot[13] = spc;
    put_le16(&mut boot, 14, rsvd as u16)?;
    boot[16] = num_fats;
    boot[21] = 0xF8;
    put_le16(&mut boot, 24, 32)?;
    put_le16(&mut boot, 26, 2)?;
    put_le32(&mut boot, 32, totsec)?;
    put_le32(&mut boot, 36, fatsz)?;
    put_le32(&mut boot, 44, root)?;
    put_le16(&mut boot, 48, 1)?;
    put_le16(&mut boot, 50, BACKUP_BOOT as u16)?;
    boot[64] = 0x80;
    boot[66] = 0x29;
    put_le32(&mut boot, 67, 0x5642_4F53)?;
    boot[71..82].copy_from_slice(&lab);
    boot[82..90].copy_from_slice(b"FAT32   ");
    boot[510] = 0x55;
    boot[511] = 0xAA;
    let mut fs = [0u8; SEC];
    put_le32(&mut fs, 0, 0x4161_5252)?;
    put_le32(&mut fs, 484, 0x6141_7272)?;
    put_le32(&mut fs, 488, nclus.saturating_sub(1))?;
    put_le32(&mut fs, 492, 3)?;
    put_le32(&mut fs, 508, 0xAA55_0000)?;
    let mut fat0 = [0u8; SEC];
    put_le32(&mut fat0, 0, 0x0FFF_FFF8)?;
    put_le32(&mut fat0, 4, 0x0FFF_FFFF)?;
    put_le32(&mut fat0, 8, 0x0FFF_FFFF)?;
    let mut vol_ent = [0u8; ENT];
    vol_ent[..11].copy_from_slice(&lab);
    vol_ent[11] = ATTR_VOL;

    // Sector offsets into `buf`, each checked against its length by `put_at`.
    let at = |lba: u32| (lba as usize).checked_mul(SEC).ok_or(FatError::Inval);
    put_at(buf, 0, &boot)?;
    if BACKUP_BOOT < rsvd {
        put_at(buf, at(BACKUP_BOOT)?, &boot)?;
    }
    put_at(buf, at(1)?, &fs)?;
    if BACKUP_FSINFO < rsvd {
        put_at(buf, at(BACKUP_FSINFO)?, &fs)?;
    }
    put_at(buf, at(rsvd)?, &fat0)?;
    put_at(
        buf,
        at(rsvd.checked_add(fatsz).ok_or(FatError::Inval)?)?,
        &fat0,
    )?;
    let data_off = at(data_lba)?;
    if let Some(dst) = buf.get_mut(data_off..).and_then(|b| b.get_mut(..ENT)) {
        dst.copy_from_slice(&vol_ent);
    }
    Ok(FatInfo {
        bps: SEC as u32,
        spc,
        rsvd,
        num_fats,
        fatsz,
        totsec,
        root_clus: root,
        fsinfo: 1,
        backup: BACKUP_BOOT,
        data_lba,
        nclus,
        media: 0xF8,
    })
}

/// Seed an image with `hello.txt` and `etc/`.
pub fn mkinitrd(buf: &mut [u8]) -> Result<(), FatError> {
    mkfs(buf, b"VIBEOS")?;
    let mut disk = MemDisk::new(buf, SEC as u32)?;
    let mut vol = FatVol::mount(&mut disk)?;
    vol.now = 1_262_304_000;
    vol.create(&mut disk, vol.info.root_clus, b"hello.txt", false)?;
    let hello = vol.lookup(&mut disk, vol.info.root_clus, b"hello.txt")?;
    let mut clu = hello.clu;
    let mut size = hello.size;
    let msg = b"hello from initrd\n";
    vol.write(
        &mut disk,
        hello.dir_clu,
        hello.dir_off,
        &mut clu,
        &mut size,
        0,
        msg,
    )?;
    vol.create(&mut disk, vol.info.root_clus, b"etc", true)?;
    vol.sync(&mut disk)
}
