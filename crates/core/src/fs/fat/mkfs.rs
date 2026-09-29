use super::*;

/// Format `buf` as FAT32. Size must be a multiple of 512 and at least 64 KiB.
pub fn mkfs(buf: &mut [u8], label: &[u8]) -> Result<FatInfo, FatError> {
    if buf.len() < INITRD_BYTES || !buf.len().is_multiple_of(SEC) {
        return Err(FatError::Inval);
    }
    buf.fill(0);
    let totsec = (buf.len() / SEC) as u32;
    let rsvd = 32u32;
    let spc = 1u8;
    let num_fats = 2u8;
    let mut fatsz = 1u32;
    loop {
        let data = totsec.saturating_sub(rsvd + num_fats as u32 * fatsz);
        let nclus = data / spc as u32;
        if nclus < 2 {
            return Err(FatError::Inval);
        }
        let need = ((nclus + 2) * 4).div_ceil(SEC as u32);
        if need <= fatsz {
            break;
        }
        fatsz = need;
        if rsvd + num_fats as u32 * fatsz >= totsec {
            return Err(FatError::Inval);
        }
    }
    let data_lba = rsvd + num_fats as u32 * fatsz;
    let nclus = (totsec - data_lba) / spc as u32;
    let root = 2u32;
    let mut boot = [0u8; SEC];
    boot[0] = 0xEB;
    boot[1] = 0x58;
    boot[2] = 0x90;
    boot[3..11].copy_from_slice(b"VIBEOS  ");
    put_le16(&mut boot, 11, SEC as u16);
    boot[13] = spc;
    put_le16(&mut boot, 14, rsvd as u16);
    boot[16] = num_fats;
    boot[21] = 0xF8;
    put_le16(&mut boot, 24, 32);
    put_le16(&mut boot, 26, 2);
    put_le32(&mut boot, 32, totsec);
    put_le32(&mut boot, 36, fatsz);
    put_le32(&mut boot, 44, root);
    put_le16(&mut boot, 48, 1);
    put_le16(&mut boot, 50, 6);
    boot[64] = 0x80;
    boot[66] = 0x29;
    put_le32(&mut boot, 67, 0x5642_4F53);
    let mut lab = [b' '; 11];
    let n = label.len().min(11);
    lab[..n].copy_from_slice(&label[..n]);
    boot[71..82].copy_from_slice(&lab);
    boot[82..90].copy_from_slice(b"FAT32   ");
    boot[510] = 0x55;
    boot[511] = 0xAA;
    buf[..SEC].copy_from_slice(&boot);
    if 6 < rsvd {
        buf[6 * SEC..7 * SEC].copy_from_slice(&boot);
    }
    let mut fs = [0u8; SEC];
    put_le32(&mut fs, 0, 0x4161_5252);
    put_le32(&mut fs, 484, 0x6141_7272);
    put_le32(&mut fs, 488, nclus.saturating_sub(1));
    put_le32(&mut fs, 492, 3);
    put_le32(&mut fs, 508, 0xAA55_0000);
    buf[SEC..2 * SEC].copy_from_slice(&fs);
    if 7 < rsvd {
        buf[7 * SEC..8 * SEC].copy_from_slice(&fs);
    }
    let mut fat0 = [0u8; SEC];
    put_le32(&mut fat0, 0, 0x0FFF_FFF8);
    put_le32(&mut fat0, 4, 0x0FFF_FFFF);
    put_le32(&mut fat0, 8, 0x0FFF_FFFF);
    let fat0_off = rsvd as usize * SEC;
    let fat1_off = fat0_off + fatsz as usize * SEC;
    buf[fat0_off..fat0_off + SEC].copy_from_slice(&fat0);
    buf[fat1_off..fat1_off + SEC].copy_from_slice(&fat0);
    let data_off = data_lba as usize * SEC;
    if data_off + ENT <= buf.len() {
        let mut ent = [0u8; ENT];
        ent[..11].copy_from_slice(&lab);
        ent[11] = ATTR_VOL;
        buf[data_off..data_off + ENT].copy_from_slice(&ent);
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
        backup: 6,
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
