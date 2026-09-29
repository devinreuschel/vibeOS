use super::*;

impl<S: Guarded<KernState>> KernFs<S> {
    /// Copy the last console write into `out`; the count copied.
    pub fn cons_captured(&self, out: &mut [u8]) -> usize {
        self.with(|k| {
            let n = (k.cons_len as usize).min(out.len());
            out[..n].copy_from_slice(&k.cons_out[..n]);
            n
        })
    }

    /// Add a block device node under the mounted devfs. Name should
    /// match `block: <name>` (ram0, vda, ram0p1, …).
    pub fn devfs_add_block(&self, name: &[u8], size: u64) -> Result<u32, FsError> {
        self.with(|k| {
            let (inst, root) = k.skin(FsType::Dev).ok_or(FsError::Io)?;
            let now = k.now;
            kern_mk_special(k, now, inst, root, name, KernKind::Block, size)
        })
    }
}

pub(super) fn mix_rng(k: &mut KernState) -> u64 {
    let mut x = k.rng;
    if x == 0 {
        x = k.now ^ 0x9E37_79B9_7F4A_7C15;
        if x == 0 {
            x = 1;
        }
    }
    // xorshift64. Not a CSPRNG.
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    k.rng = x;
    x
}
