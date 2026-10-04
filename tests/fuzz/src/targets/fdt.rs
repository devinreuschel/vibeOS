//! `fdt_parse`: the device-tree walker and the one UART chooser.

use vibeos::machine::fdt;

/// [`fdt::parse`] and [`fdt::pick_pl011`] on the same blob.
pub fn parse(data: &[u8]) {
    let _ = fdt::parse(data);
    let _ = fdt::pick_pl011(data);
}
