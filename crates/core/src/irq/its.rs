//! GICv3 ITS command encodings from ARM IHI 0069G.
//!
//! Builders only. No queue, no redistributor, no runtime. Each command is
//! 32 bytes, four little-endian doublewords, as the architecture
//! specification defines them.
